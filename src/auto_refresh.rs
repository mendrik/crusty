//! The Observatory's background index refresher.
//!
//! One refresher runs per server process. A filesystem watcher (Crusty state,
//! Cargo `target` directories, and Git internals other than `HEAD` and refs
//! are ignored) and a cheap periodic `HEAD` check mark the index as possibly
//! out of date; after a quiet period, bounded by a maximum delay, the
//! Observatory checks freshness and, only when the published generation is
//! stale, runs the ordinary incremental refresh as a durable `index.refresh`
//! task under the publisher lease. Reads never wait for it: they keep serving
//! the last published generation.

use crate::observatory::Observatory;
use chrono::Utc;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, RecvTimeoutError, SyncSender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

/// Set to `0`, `false`, `no`, or `off` to disable automatic refresh.
pub(crate) const AUTO_REFRESH_ENV: &str = "CRUSTY_AUTO_REFRESH";

/// Whether automatic refresh is enabled for an environment value; on by default.
pub(crate) fn auto_refresh_enabled(value: Option<&str>) -> bool {
    !value.is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        )
    })
}

/// Timing of the refresher. Tests shorten it; servers use the default.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Timing {
    /// Delay before the startup check, so MCP initialisation is never contended.
    pub(crate) startup_delay: Duration,
    /// Quiet period after the last relevant change.
    pub(crate) quiet: Duration,
    /// Upper bound between the first change and the refresh during constant churn.
    pub(crate) max_delay: Duration,
    /// Interval of the `HEAD` check, which also catches commits and checkouts
    /// in linked worktrees whose Git directory is not watched.
    pub(crate) head_poll: Duration,
    /// Interval of a full freshness check when no watcher is available.
    pub(crate) unwatched_poll: Duration,
    /// Delay before retrying while another refresh holds the publisher.
    pub(crate) retry: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            startup_delay: Duration::from_millis(500),
            quiet: Duration::from_secs(2),
            max_delay: Duration::from_secs(30),
            head_poll: Duration::from_secs(5),
            unwatched_poll: Duration::from_secs(30),
            retry: Duration::from_secs(5),
        }
    }
}

/// What one refresh attempt did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TickOutcome {
    /// The published generation already describes the workspace.
    Fresh,
    /// A refresh ran and completed.
    Refreshed,
    /// A refresh ran and failed; the next change tries again.
    Failed,
    /// Another refresh holds the publisher; retry later.
    Busy,
}

impl TickOutcome {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Fresh => "fresh",
            Self::Refreshed => "refreshed",
            Self::Failed => "failed",
            Self::Busy => "busy",
        }
    }
}

/// Refresher state shared with `index.status` and the freshness envelope.
#[derive(Debug, Default)]
pub(crate) struct AutoRefreshState {
    running: AtomicBool,
    detail: Mutex<Value>,
}

impl AutoRefreshState {
    /// Whether this process runs a background refresher.
    pub(crate) fn enabled(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    pub(crate) fn status(&self) -> Value {
        let detail = self.lock().clone();
        let mut status = if detail.is_object() {
            detail
        } else {
            json!({"enabled": false, "reason": "not started by this process"})
        };
        status["enabled"] = Value::from(self.enabled());
        status
    }

    pub(crate) fn record(
        &self,
        trigger: &str,
        outcome: TickOutcome,
        task_id: Option<&str>,
        error: Option<&str>,
    ) {
        let mut detail = self.lock();
        if detail.is_object() {
            detail["last"] = json!({
                "trigger": trigger,
                "outcome": outcome.label(),
                "task_id": task_id,
                "error": error,
                "at": Utc::now().to_rfc3339(),
            });
        }
    }

    fn set(&self, detail: Value) {
        *self.lock() = detail;
    }

    fn update(&self, key: &str, value: Value) {
        let mut detail = self.lock();
        if detail.is_object() {
            detail[key] = value;
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Value> {
        self.detail
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }
}

enum Signal {
    /// A relevant path changed.
    Changed,
    /// A directory appeared directly under the root and needs its own watch.
    Directory(PathBuf),
    Stop,
}

/// Handle to the running refresher; dropping it stops the thread.
pub struct AutoRefresh {
    stop: Arc<AtomicBool>,
    sender: SyncSender<Signal>,
    thread: Option<JoinHandle<()>>,
    state: Arc<AutoRefreshState>,
}

impl AutoRefresh {
    pub(crate) fn start(
        observatory: Observatory,
        state: Arc<AutoRefreshState>,
        timing: Timing,
        watch: bool,
    ) -> std::io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let (sender, receiver) = mpsc::sync_channel(256);
        state.set(json!({
            "enabled": true,
            "watcher": if watch { "starting" } else { "disabled" },
            "debounce_ms": {"quiet": timing.quiet.as_millis() as u64, "max": timing.max_delay.as_millis() as u64},
            "head_poll_ms": timing.head_poll.as_millis() as u64,
            "last": Value::Null,
        }));
        state.running.store(true, Ordering::SeqCst);
        let worker = Worker {
            root: observatory.root().to_path_buf(),
            observatory,
            state: state.clone(),
            timing,
            stop: stop.clone(),
        };
        let watcher_sender = sender.clone();
        let thread = thread::Builder::new()
            .name("crusty-auto-refresh".into())
            .spawn(move || worker.run(receiver, watch.then_some(watcher_sender)));
        match thread {
            Ok(thread) => Ok(Self {
                stop,
                sender,
                thread: Some(thread),
                state,
            }),
            Err(error) => {
                state.running.store(false, Ordering::SeqCst);
                Err(error)
            }
        }
    }

    /// Stops the refresher, waiting briefly for an idle thread. A refresh in
    /// progress is left to finish or to die with the process: its savepoint
    /// rolls back and the next server marks its task interrupted.
    pub fn shutdown(mut self) {
        self.stop_thread(Duration::from_secs(3));
    }

    fn stop_thread(&mut self, wait: Duration) {
        self.stop.store(true, Ordering::SeqCst);
        self.state.running.store(false, Ordering::SeqCst);
        let _ = self.sender.try_send(Signal::Stop);
        let Some(thread) = self.thread.take() else {
            return;
        };
        let deadline = Instant::now() + wait;
        while !thread.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        if thread.is_finished() {
            let _ = thread.join();
        }
    }
}

impl Drop for AutoRefresh {
    fn drop(&mut self) {
        self.stop_thread(Duration::from_secs(3));
    }
}

struct Worker {
    root: PathBuf,
    observatory: Observatory,
    state: Arc<AutoRefreshState>,
    timing: Timing,
    stop: Arc<AtomicBool>,
}

impl Worker {
    fn run(self, receiver: mpsc::Receiver<Signal>, watch: Option<SyncSender<Signal>>) {
        let mut watcher = watch.and_then(|sender| self.start_watcher(sender));
        self.state.update(
            "watcher",
            Value::from(match (&watcher, self.state.status()["watcher"].as_str()) {
                (Some(_), _) => "active",
                (None, Some("disabled")) => "disabled",
                (None, _) => "unavailable",
            }),
        );
        let now = Instant::now();
        let mut startup = Some(now + self.timing.startup_delay);
        let mut first_change: Option<Instant> = None;
        let mut last_change: Option<Instant> = None;
        let mut retry: Option<Instant> = None;
        let mut last_head = head(&self.root);
        let mut next_head_poll = now + self.timing.head_poll;
        let mut next_full_poll = watcher.is_none().then(|| now + self.timing.unwatched_poll);
        while !self.stop.load(Ordering::SeqCst) {
            let debounced = first_change
                .zip(last_change)
                .map(|(first, last)| (last + self.timing.quiet).min(first + self.timing.max_delay));
            let deadline = [
                startup,
                debounced,
                retry,
                Some(next_head_poll),
                next_full_poll,
            ]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(next_head_poll);
            // Due work is evaluated after every wake-up, so a constant event
            // stream cannot postpone a refresh past the maximum delay.
            match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(Signal::Changed) => {
                    let now = Instant::now();
                    first_change.get_or_insert(now);
                    last_change = Some(now);
                }
                Ok(Signal::Directory(path)) => {
                    if let Some(watcher) = watcher.as_mut() {
                        let _ = watcher.watch(&path, RecursiveMode::Recursive);
                    }
                    let now = Instant::now();
                    first_change.get_or_insert(now);
                    last_change = Some(now);
                }
                Ok(Signal::Stop) | Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {}
            }
            let now = Instant::now();
            let mut trigger = None;
            if startup.is_some_and(|due| due <= now) {
                startup = None;
                trigger = Some("startup");
            }
            if debounced.is_some_and(|due| due <= now) {
                first_change = None;
                last_change = None;
                trigger = trigger.or(Some("filesystem"));
            }
            if retry.is_some_and(|due| due <= now) {
                retry = None;
                trigger = trigger.or(Some("retry"));
            }
            if next_full_poll.is_some_and(|due| due <= now) {
                next_full_poll = Some(now + self.timing.unwatched_poll);
                trigger = trigger.or(Some("periodic"));
            }
            if next_head_poll <= now {
                next_head_poll = now + self.timing.head_poll;
                let current = head(&self.root);
                if current != last_head {
                    last_head = current;
                    // A rebase or checkout moves HEAD repeatedly; debounce it
                    // like any other change.
                    first_change.get_or_insert(now);
                    last_change = Some(now);
                }
            }
            let Some(trigger) = trigger else {
                continue;
            };
            if self.stop.load(Ordering::SeqCst) {
                break;
            }
            if self.observatory.auto_refresh_tick(trigger) == TickOutcome::Busy {
                retry = Some(Instant::now() + self.timing.retry);
            }
            last_head = head(&self.root);
        }
        drop(watcher.take());
    }

    /// Watches the root shallowly, each eligible top-level directory
    /// recursively, and the Git directory's `HEAD` and refs. Recursively
    /// watching the root would also watch every directory under `target/`.
    fn start_watcher(&self, sender: SyncSender<Signal>) -> Option<RecommendedWatcher> {
        let root = self.root.clone();
        let git_dirs = git_directories(&root);
        let filter_dirs = git_dirs.clone();
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                let Ok(event) = event else {
                    // An overflowed queue lost events; treat it as a change.
                    let _ = sender.try_send(Signal::Changed);
                    return;
                };
                if matches!(event.kind, EventKind::Access(_)) {
                    return;
                }
                for path in &event.paths {
                    if matches!(event.kind, EventKind::Create(_))
                        && path.parent() == Some(root.as_path())
                        && path.is_dir()
                        && watchable_directory(&root, path)
                    {
                        let _ = sender.try_send(Signal::Directory(path.clone()));
                        return;
                    }
                    if relevant_path(&root, &filter_dirs, path) {
                        let _ = sender.try_send(Signal::Changed);
                        return;
                    }
                }
            })
            .ok()?;
        watcher
            .watch(&self.root, RecursiveMode::NonRecursive)
            .ok()?;
        for entry in fs::read_dir(&self.root).ok()?.flatten() {
            let path = entry.path();
            if path.is_dir() && watchable_directory(&self.root, &path) {
                // A directory that cannot be watched (permissions) is covered
                // by the periodic HEAD check and the next explicit refresh.
                let _ = watcher.watch(&path, RecursiveMode::Recursive);
            }
        }
        for directory in &git_dirs {
            let _ = watcher.watch(directory, RecursiveMode::NonRecursive);
            let refs = directory.join("refs");
            if refs.is_dir() {
                let _ = watcher.watch(&refs, RecursiveMode::Recursive);
            }
        }
        Some(watcher)
    }
}

/// The work tree's Git directory and, for a linked worktree, the common one.
fn git_directories(root: &Path) -> Vec<PathBuf> {
    let mut directories = Vec::new();
    for argument in ["--absolute-git-dir", "--git-common-dir"] {
        let Some(path) =
            crate::command_text(root, &["rev-parse", "--path-format=absolute", argument])
        else {
            continue;
        };
        if let Ok(path) = fs::canonicalize(path)
            && !directories.contains(&path)
        {
            directories.push(path);
        }
    }
    directories
}

/// Top-level directories worth a recursive watch.
fn watchable_directory(root: &Path, path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    name != ".git" && !crate::excluded_from_inputs(root, &format!("{name}/entry"))
}

/// Whether a filesystem event path can change the published index.
pub(crate) fn relevant_path(root: &Path, git_dirs: &[PathBuf], path: &Path) -> bool {
    if let Some(directory) = git_dirs
        .iter()
        .find(|directory| path.starts_with(directory))
    {
        let Ok(relative) = path.strip_prefix(directory) else {
            return false;
        };
        let name = relative.to_string_lossy();
        return name == "HEAD"
            || name == "packed-refs"
            || relative.starts_with("refs")
            || name == "HEAD.lock";
    }
    let Ok(relative) = path.strip_prefix(root) else {
        return false;
    };
    let relative = relative.to_string_lossy().replace('\\', "/");
    if relative.is_empty() || relative == ".git" || relative.starts_with(".git/") {
        return false;
    }
    if crate::excluded_from_inputs(root, &relative) {
        return false;
    }
    // A removed path may have been an input or a directory of inputs.
    crate::input_kind(path).is_some() || path.is_dir() || !path.exists()
}

fn head(root: &Path) -> Option<String> {
    crate::command_text(root, &["rev-parse", "HEAD"])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_refresh_is_on_unless_explicitly_disabled() {
        assert!(auto_refresh_enabled(None));
        assert!(auto_refresh_enabled(Some("1")));
        assert!(auto_refresh_enabled(Some("yes")));
        for value in ["0", "false", "OFF", " no "] {
            assert!(!auto_refresh_enabled(Some(value)), "{value}");
        }
    }

    #[test]
    fn state_target_and_git_internals_are_not_relevant() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::write(root.join("Cargo.toml"), "[package]\nname='demo'\n").unwrap();
        fs::create_dir_all(root.join("target/debug")).unwrap();
        fs::create_dir_all(root.join("src/target")).unwrap();
        let git = root.join(".git");
        fs::create_dir_all(git.join("refs/heads")).unwrap();
        fs::create_dir_all(git.join("objects/ab")).unwrap();
        fs::write(root.join("image.png"), b"png").unwrap();
        let git_dirs = vec![git.clone()];
        assert!(relevant_path(root, &git_dirs, &root.join("src/lib.rs")));
        assert!(relevant_path(
            root,
            &git_dirs,
            &root.join("src/target/mod.rs")
        ));
        assert!(relevant_path(root, &git_dirs, &root.join("README.md")));
        assert!(!relevant_path(root, &git_dirs, &root.join("image.png")));
        assert!(!relevant_path(
            root,
            &git_dirs,
            &root.join("target/debug/x.json")
        ));
        assert!(!relevant_path(
            root,
            &git_dirs,
            &root.join(".rust-repo-intelligence/index.sqlite3")
        ));
        assert!(!relevant_path(
            root,
            &git_dirs,
            &root.join("src/.rust-repo-intelligence/state.json")
        ));
        assert!(relevant_path(root, &git_dirs, &git.join("HEAD")));
        assert!(relevant_path(root, &git_dirs, &git.join("refs/heads/main")));
        assert!(!relevant_path(root, &git_dirs, &git.join("index")));
        assert!(!relevant_path(root, &git_dirs, &git.join("objects/ab/cd")));
        assert!(!watchable_directory(root, &root.join("target")));
        assert!(watchable_directory(root, &root.join("src")));
    }
}

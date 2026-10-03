//! The rust-analyzer companion shared by one server process.
//!
//! The companion is opt-in (`RUST_REPO_INTELLIGENCE_ENABLE_RUST_ANALYZER`)
//! because it holds a whole workspace model in memory. When enabled, the
//! server warms it in the background right after startup, so MCP
//! initialisation never waits for it. Every caller shares one canonical
//! profile whose key covers the analyzer version, initialization options,
//! toolchain and Cargo configuration, never source contents: source edits
//! reach the running analyzer as `workspace/didChangeWatchedFiles` and
//! document notifications instead of restarts. A dead child is restarted with
//! backoff. A reader thread answers the analyzer's own requests and records
//! readiness (`experimental/serverStatus`) and published diagnostics, so that
//! state stays current while no query runs.

use crate::{
    execution::{self, ExecutionControl},
    live_semantics::file_uri,
    verification::BuildProfile,
};
use anyhow::{Context, Result, ensure};
use chrono::Utc;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    ffi::OsString,
    fs,
    io::{self, BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{ChildStdin, ChildStdout, Command, Stdio},
    sync::{
        Arc, Mutex, MutexGuard, TryLockError,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime},
};

/// Set to `1` to enable the analyzer when the server starts. Otherwise it stays
/// off until `semantic.enable` turns it on for the running server.
pub(crate) const AUTOSTART_ENV: &str = "RUST_REPO_INTELLIGENCE_RUST_ANALYZER_AUTOSTART";
/// The pre-0.4 switch. It no longer starts the analyzer; status reports it so
/// configurations that still set it are not silently misread.
pub(crate) const LEGACY_ENABLE_ENV: &str = "RUST_REPO_INTELLIGENCE_ENABLE_RUST_ANALYZER";
pub(crate) const RUST_ANALYZER_PATH_ENV: &str = "RUST_REPO_INTELLIGENCE_RUST_ANALYZER_PATH";
/// Set to `0`, `false`, `no`, or `off` to start an enabled analyzer on first
/// use instead of right after it is enabled.
pub(crate) const WARM_START_ENV: &str = "RUST_REPO_INTELLIGENCE_RUST_ANALYZER_WARM_START";
/// Seconds a semantic query waits for a warming analyzer (default 60, at most 600).
pub(crate) const READY_TIMEOUT_ENV: &str = "RUST_REPO_INTELLIGENCE_RUST_ANALYZER_READY_SECONDS";
/// Set to `1` to let the analyzer run `cargo check` after edits.
pub(crate) const CHECK_ON_SAVE_ENV: &str = "RUST_REPO_INTELLIGENCE_RUST_ANALYZER_CHECK_ON_SAVE";
pub(crate) const ENABLE_HINT: &str = "rust-analyzer is off. Call semantic.enable to start it for this server (live semantics and diagnostics; it costs one analyzer process, often several GB for large workspaces) and semantic.disable to stop it. RUST_REPO_INTELLIGENCE_RUST_ANALYZER_AUTOSTART=1 starts it with the server. Exact source search remains available.";

const MAX_THREADS: usize = 4;
const DEFAULT_READY_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_READY_TIMEOUT: Duration = Duration::from_secs(600);
/// Without any `experimental/serverStatus` by then, readiness is unconfirmed:
/// the analyzer does not support the extension.
const STATUS_GRACE: Duration = Duration::from_secs(3);
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
const MAX_OPEN_DOCUMENTS: usize = 64;
/// Changed files opened per synchronisation, so their diagnostics are published.
const MAX_OPENED_PER_SYNC: usize = 16;
const MAX_DIAGNOSTIC_FILES: usize = 2048;
const MAX_DIAGNOSTICS_PER_FILE: usize = 200;
const MAX_MESSAGE_CHARS: usize = 1000;
pub(crate) const MAX_SOURCE_BYTES: u64 = 4_000_000;

fn switch(value: Option<&str>, default: bool) -> bool {
    match value
        .map(|value| value.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("1" | "true" | "yes" | "on") => true,
        Some("0" | "false" | "no" | "off") => false,
        _ => default,
    }
}

/// Whether the companion is enabled at server start; off unless explicitly
/// opted in.
pub(crate) fn autostart_enabled(value: Option<&str>) -> bool {
    switch(value, false)
}

/// Whether an enabled companion starts loading right away; on by default.
pub(crate) fn warm_start_enabled(value: Option<&str>) -> bool {
    switch(value, true)
}

fn check_on_save_enabled() -> bool {
    switch(std::env::var(CHECK_ON_SAVE_ENV).ok().as_deref(), false)
}

/// How long queries wait for a warming analyzer.
pub(crate) fn ready_timeout(value: Option<&str>) -> Duration {
    value
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_READY_TIMEOUT)
        .min(MAX_READY_TIMEOUT)
}

pub(crate) fn configured_ready_timeout() -> Duration {
    ready_timeout(std::env::var(READY_TIMEOUT_ENV).ok().as_deref())
}

/// Analyzer and cache-priming worker threads: at most four, so a background
/// companion never claims a whole machine.
pub(crate) fn analyzer_threads() -> usize {
    thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .clamp(1, MAX_THREADS)
}

/// The analyzer executable: an explicit path, else `rustup which` run in the
/// workspace so its `rust-toolchain` pin selects the analyzer, else
/// `rust-analyzer` from `PATH`. Resolved once per workspace and toolchain pin.
pub(crate) fn rust_analyzer_program(root: &Path, explicit: Option<OsString>) -> PathBuf {
    if let Some(explicit) = explicit {
        return PathBuf::from(explicit);
    }
    static RUSTUP: Mutex<BTreeMap<(PathBuf, String), Option<PathBuf>>> =
        Mutex::new(BTreeMap::new());
    let key = (root.to_path_buf(), toolchain_pin(root));
    if let Some(program) = lock(&RUSTUP).get(&key) {
        return program
            .clone()
            .unwrap_or_else(|| PathBuf::from("rust-analyzer"));
    }
    let program = Command::new("rustup")
        .args(["which", "rust-analyzer"])
        .current_dir(root)
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|path| !path.is_empty())
        .map(PathBuf::from);
    lock(&RUSTUP).insert(key, program.clone());
    program.unwrap_or_else(|| PathBuf::from("rust-analyzer"))
}

/// The nearest `rust-toolchain(.toml)` contents at or above `root`, which
/// decide the toolchain `rustup which` resolves there.
fn toolchain_pin(root: &Path) -> String {
    root.ancestors()
        .flat_map(|directory| {
            ["rust-toolchain.toml", "rust-toolchain"].map(|name| directory.join(name))
        })
        .find_map(|path| fs::read_to_string(path).ok())
        .unwrap_or_default()
}

pub(crate) fn configured_program(root: &Path) -> PathBuf {
    rust_analyzer_program(
        root,
        std::env::var_os(RUST_ANALYZER_PATH_ENV).filter(|value| !value.is_empty()),
    )
}

/// Stable Rust ships every six weeks, so an analyzer older than two release
/// cycles misses fixes the agent's own toolchain likely has.
const OUTDATED_AFTER_DAYS: i64 = 90;

/// The release date in a `rust-analyzer --version` string such as
/// `rust-analyzer 1.99.0 (b940084 2026-09-28)`. Standalone builds report no
/// date and yield `None`.
fn version_release_date(version: &str) -> Option<chrono::NaiveDate> {
    let inner = version.rsplit_once('(')?.1.split_once(')')?.0;
    inner
        .split_whitespace()
        .find_map(|part| chrono::NaiveDate::parse_from_str(part, "%Y-%m-%d").ok())
}

/// Version, release date and age of the analyzer at `program`, with an update
/// hint once it is older than [`OUTDATED_AFTER_DAYS`].
pub(crate) fn version_currency(program: &Path, today: chrono::NaiveDate) -> Value {
    let Some(version) = rust_analyzer_version(program) else {
        return json!({"version": null});
    };
    let Some(released) = version_release_date(&version) else {
        return json!({"version": version, "release_date": null, "outdated": null});
    };
    let age = (today - released).num_days();
    let outdated = age > OUTDATED_AFTER_DAYS;
    json!({
        "version": version,
        "release_date": released.to_string(),
        "age_days": age,
        "outdated": outdated,
        "update_hint": outdated.then(|| format!(
            "{version} is {age} days old; run `rustup update` (or raise the project's rust-toolchain pin). Crusty restarts the analyzer when its version changes."
        )),
    })
}

/// The executable `program` names: itself when it has a directory, else the
/// first match on `PATH`.
pub(crate) fn resolve_executable(program: &Path) -> Option<PathBuf> {
    if program.components().count() > 1 {
        return program.is_file().then(|| program.to_path_buf());
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|directory| directory.join(program))
        .find(|candidate| candidate.is_file())
}

/// `rust-analyzer --version`, cached per executable until its modification
/// time changes.
pub(crate) fn rust_analyzer_version(program: &Path) -> Option<String> {
    static VERSIONS: Mutex<BTreeMap<PathBuf, (SystemTime, Option<String>)>> =
        Mutex::new(BTreeMap::new());
    let executable = resolve_executable(program)?;
    let modified = fs::metadata(&executable)
        .and_then(|metadata| metadata.modified())
        .ok();
    if let Some(modified) = modified
        && let Some((cached, version)) = lock(&VERSIONS).get(&executable)
        && *cached == modified
    {
        return version.clone();
    }
    let version = Command::new(program)
        .arg("--version")
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned());
    if let Some(modified) = modified {
        lock(&VERSIONS).insert(executable, (modified, version.clone()));
    }
    version
}

pub(crate) fn rust_analyzer_initialization_options() -> Value {
    let threads = analyzer_threads();
    let jobs = threads.to_string();
    let mut options = json!({
        // `cargo check` after edits is opt-in: it competes with the agent's
        // own builds. Its results are published as diagnostics.
        "checkOnSave": check_on_save_enabled(),
        "check": {"command": "check", "extraEnv": {"CARGO_BUILD_JOBS": jobs}},
        "cachePriming": {"enable": true, "numThreads": threads},
        "numThreads": threads,
        "cargo": {
            // Build scripts and checks use target/rust-analyzer, so the
            // companion never holds the lock on the agent's build directory.
            "targetDir": true,
            "extraEnv": {"CARGO_BUILD_JOBS": jobs},
        },
        // Crusty reports file changes itself, without an analyzer-side watcher.
        "files": {"watcher": "client"},
    });
    if let Ok(features) = std::env::var(crate::FEATURES_ENV)
        && features != "cargo-default"
    {
        options["cargo"]["features"] = if features == "all" {
            json!("all")
        } else {
            json!(
                features
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
            )
        };
    }
    if let Ok(target) = std::env::var("CARGO_BUILD_TARGET")
        && !target.is_empty()
    {
        options["cargo"]["target"] = json!(target);
    }
    options
}

/// What an analyzer process is started with.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Launch {
    pub(crate) options: Value,
    pub(crate) toolchain: Option<String>,
}

impl Launch {
    pub(crate) fn workspace_default() -> Self {
        Self {
            options: rust_analyzer_initialization_options(),
            toolchain: None,
        }
    }

    /// The default launch with only the profile's explicit selections
    /// applied, so a default profile shares the default analyzer.
    pub(crate) fn for_profile(profile: &BuildProfile) -> Self {
        let mut launch = Self::workspace_default();
        if profile.all_features {
            launch.options["cargo"]["features"] = json!("all");
        } else if !profile.features.is_empty() {
            launch.options["cargo"]["features"] = json!(profile.features);
        }
        if profile.no_default_features {
            launch.options["cargo"]["noDefaultFeatures"] = json!(true);
        }
        if let Some(target) = &profile.target {
            launch.options["cargo"]["target"] = json!(target);
        }
        launch.toolchain = profile.toolchain.clone();
        launch
    }

    fn check_on_save(&self) -> bool {
        self.options["checkOnSave"] == true
    }
}

/// The inputs whose change restarts the analyzer, and their digest.
pub(crate) fn profile_key(
    launch: &Launch,
    version: Option<&str>,
    cargo_digest: &str,
) -> (String, Value) {
    let inputs = json!({
        "version": version,
        "toolchain": launch.toolchain,
        "options": format!("b3:{}", blake3::hash(launch.options.to_string().as_bytes()).to_hex()),
        "cargo_configuration": cargo_digest,
    });
    let key = format!(
        "ra:{}",
        &blake3::hash(inputs.to_string().as_bytes()).to_hex()[..32]
    );
    (key, inputs)
}

fn is_cargo_configuration(relative: &str) -> bool {
    let name = relative.rsplit('/').next().unwrap_or(relative);
    matches!(
        name,
        "Cargo.toml" | "Cargo.lock" | "rust-toolchain" | "rust-toolchain.toml"
    ) || relative == ".cargo/config"
        || relative == ".cargo/config.toml"
        || relative.ends_with("/.cargo/config")
        || relative.ends_with("/.cargo/config.toml")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileStamp {
    modified: Option<SystemTime>,
    len: u64,
}

/// Rust sources and Cargo configuration of the workspace, by modification
/// stamp, plus a content digest of the Cargo configuration.
#[derive(Debug, Default)]
pub(crate) struct WorkspaceScan {
    files: HashMap<PathBuf, FileStamp>,
    pub(crate) cargo_digest: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum FileChange {
    Created = 1,
    Changed = 2,
    Deleted = 3,
}

impl WorkspaceScan {
    pub(crate) fn take(root: &Path) -> Self {
        let mut paths = crate::workspace_files(root);
        paths.sort();
        let mut files = HashMap::new();
        let mut cargo = blake3::Hasher::new();
        for path in paths {
            let Ok(relative) = path.strip_prefix(root) else {
                continue;
            };
            let relative = relative.to_string_lossy().replace('\\', "/");
            let configuration = is_cargo_configuration(&relative);
            if !configuration && path.extension().is_none_or(|extension| extension != "rs") {
                continue;
            }
            let Ok(metadata) = fs::metadata(&path) else {
                continue;
            };
            if configuration {
                cargo.update(relative.as_bytes());
                cargo.update(&[0]);
                if let Ok(content) = fs::read(&path) {
                    cargo.update(blake3::hash(&content).as_bytes());
                }
            }
            files.insert(
                path,
                FileStamp {
                    modified: metadata.modified().ok(),
                    len: metadata.len(),
                },
            );
        }
        Self {
            files,
            cargo_digest: format!("b3:{}", cargo.finalize().to_hex()),
        }
    }

    fn changes_since(&self, previous: &Self) -> Vec<(PathBuf, FileChange)> {
        let mut changes = self
            .files
            .iter()
            .filter_map(|(path, stamp)| match previous.files.get(path) {
                None => Some((path.clone(), FileChange::Created)),
                Some(before) if before != stamp => Some((path.clone(), FileChange::Changed)),
                Some(_) => None,
            })
            .chain(
                previous
                    .files
                    .keys()
                    .filter(|path| !self.files.contains_key(*path))
                    .map(|path| (path.clone(), FileChange::Deleted)),
            )
            .collect::<Vec<_>>();
        changes.sort();
        changes
    }
}

/// Readiness of the running analyzer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Readiness {
    /// Started; no status reported yet.
    Starting,
    /// Loading or priming the workspace (`quiescent:false`).
    Warming,
    /// The workspace is loaded (`quiescent:true`).
    Ready,
    /// The analyzer never reported `experimental/serverStatus`.
    Unconfirmed,
    /// The process ended.
    Exited,
}

impl Readiness {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Warming => "warming",
            Self::Ready => "ready",
            Self::Unconfirmed => "unconfirmed",
            Self::Exited => "exited",
        }
    }

    pub(crate) fn answers(self) -> bool {
        matches!(self, Self::Ready | Self::Unconfirmed)
    }
}

#[derive(Debug, Clone)]
struct OpenDocument {
    version: i64,
    hash: String,
    used: u64,
}

#[derive(Debug, Clone)]
struct Published {
    version: Option<i64>,
    /// Hash of the content the analyzer diagnosed, when known.
    content_hash: Option<String>,
    diagnostics: Vec<Value>,
    total: usize,
    received_at: String,
}

/// State maintained by the reader thread of the current analyzer process.
#[derive(Debug, Default)]
struct LiveState {
    /// Incremented per started process; older reader threads stop writing.
    generation: u64,
    started: Option<Instant>,
    server_status: Value,
    status_seen: bool,
    exited: bool,
    documents: HashMap<String, OpenDocument>,
    diagnostics: BTreeMap<String, Published>,
    dropped_files: u64,
    /// Monotonic document versions and recency, unique across reopenings.
    clock: u64,
}

impl LiveState {
    fn readiness(&self) -> Readiness {
        if self.exited {
            Readiness::Exited
        } else if self.status_seen {
            if self.server_status["quiescent"] == true {
                Readiness::Ready
            } else {
                Readiness::Warming
            }
        } else if self
            .started
            .is_some_and(|started| started.elapsed() < STATUS_GRACE)
        {
            Readiness::Starting
        } else {
            Readiness::Unconfirmed
        }
    }

    fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

pub(crate) struct RustAnalyzerClient {
    child: execution::OwnedChild,
    stdin: Arc<Mutex<ChildStdin>>,
    responses: Receiver<Value>,
    next_id: u64,
    pub(crate) capabilities: Value,
    live: Arc<Mutex<LiveState>>,
}

impl RustAnalyzerClient {
    fn start(
        workspace: &Path,
        program: &Path,
        launch: &Launch,
        live: Arc<Mutex<LiveState>>,
        generation: u64,
    ) -> Result<Self> {
        let mut command = Command::new(program);
        if let Some(toolchain) = &launch.toolchain {
            command.env("RUSTUP_TOOLCHAIN", toolchain);
        }
        let mut child = execution::OwnedChild::spawn(
            command
                .current_dir(workspace)
                // Keep the analyzer's Cargo subprocesses within its own
                // worker bound.
                .env("CARGO_BUILD_JOBS", analyzer_threads().to_string())
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null()),
        )
        .with_context(|| format!("starting rust-analyzer at {}", program.display()))?;
        let stdout = child
            .child
            .stdout
            .take()
            .context("opening rust-analyzer stdout")?;
        let stdin = Arc::new(Mutex::new(
            child
                .child
                .stdin
                .take()
                .context("opening rust-analyzer stdin")?,
        ));
        let (sender, responses) = mpsc::channel();
        {
            let stdin = stdin.clone();
            let configuration = launch.options.clone();
            let live = live.clone();
            thread::Builder::new()
                .name("crusty-rust-analyzer".into())
                .spawn(move || {
                    read_messages(stdout, stdin, configuration, live, generation, sender)
                })
                .context("starting the rust-analyzer reader")?;
        }
        let mut client = Self {
            child,
            stdin,
            responses,
            next_id: 1,
            capabilities: Value::Null,
            live,
        };
        let root = file_uri(workspace);
        let initialized = client.request(
            "initialize",
            json!({"processId": std::process::id(), "rootUri": root,
                "capabilities": {"general":{"positionEncodings":["utf-16"]},"experimental":{"serverStatusNotification":true},
                    "workspace":{"configuration":true,"didChangeWatchedFiles":{"dynamicRegistration":true}},
                    "textDocument":{"publishDiagnostics":{"versionSupport":true},
                        "codeAction":{"codeActionLiteralSupport":{"codeActionKind":{"valueSet":["quickfix","refactor"]}}}}},
                "initializationOptions": launch.options,
                "workspaceFolders": [{"uri": root, "name": "workspace"}]}),
            INITIALIZE_TIMEOUT,
            &ExecutionControl::default(),
        )?;
        ensure!(
            initialized.get("protocol_error").is_none(),
            "rust-analyzer initialization failed: {}",
            initialized["protocol_error"]
        );
        client.capabilities = initialized["capabilities"].clone();
        client.notify("initialized", json!({}))?;
        Ok(client)
    }

    pub(crate) fn alive(&mut self) -> bool {
        self.child.is_running().unwrap_or(false) && !lock(&self.live).exited
    }

    fn exit_description(&mut self) -> String {
        match self.child.child.try_wait() {
            Ok(Some(status)) => format!("rust-analyzer exited: {status}"),
            _ => "rust-analyzer closed its output".into(),
        }
    }

    fn notify(&self, method: &str, params: Value) -> Result<()> {
        write_lsp(
            &mut *lock(&self.stdin),
            &json!({"jsonrpc":"2.0","method":method,"params":params}),
        )
    }

    /// One request; a protocol error is returned as `{"protocol_error":...}`.
    pub(crate) fn request(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
        control: &ExecutionControl,
    ) -> Result<Value> {
        // Responses to requests that timed out earlier are discarded.
        while self.responses.try_recv().is_ok() {}
        let id = self.next_id;
        self.next_id += 1;
        write_lsp(
            &mut *lock(&self.stdin),
            &json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}),
        )?;
        let deadline = Instant::now() + timeout;
        loop {
            let cancel = |client: &Self| {
                let _ = client.notify("$/cancelRequest", json!({"id": id}));
            };
            if let Err(error) = control.check() {
                cancel(self);
                return Err(error.into());
            }
            if Instant::now() >= deadline {
                cancel(self);
                anyhow::bail!("rust-analyzer {method} timed out; empty results were not inferred");
            }
            let message = match self.responses.recv_timeout(Duration::from_millis(100)) {
                Ok(message) => message,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => {
                    anyhow::bail!("rust-analyzer disconnected")
                }
            };
            if message["id"].as_u64() != Some(id) {
                continue;
            }
            if let Some(error) = message.get("error") {
                return Ok(json!({"protocol_error":error}));
            }
            return Ok(message["result"].clone());
        }
    }

    /// Makes the analyzer see `text` for `path`. A changed open document is
    /// closed and reopened under a new version, which also makes the analyzer
    /// republish its diagnostics even when they did not change.
    pub(crate) fn sync_document(&mut self, path: &Path, text: &str) -> Result<()> {
        let uri = file_uri(path);
        let hash = content_hash(text.as_bytes());
        let (reopen, evicted) = {
            let mut live = lock(&self.live);
            let used = live.tick();
            if let Some(document) = live.documents.get_mut(&uri) {
                document.used = used;
                if document.hash == hash {
                    return Ok(());
                }
                (true, None)
            } else if live.documents.len() >= MAX_OPEN_DOCUMENTS {
                let oldest = live
                    .documents
                    .iter()
                    .min_by_key(|(_, document)| document.used)
                    .map(|(uri, _)| uri.clone());
                (false, oldest)
            } else {
                (false, None)
            }
        };
        if let Some(evicted) = evicted {
            self.close_uri(&evicted)?;
        }
        if reopen {
            self.close_uri(&uri)?;
        }
        let version = {
            let mut live = lock(&self.live);
            let version = live.tick() as i64;
            live.diagnostics.remove(&uri);
            live.documents.insert(
                uri.clone(),
                OpenDocument {
                    version,
                    hash,
                    used: version as u64,
                },
            );
            version
        };
        self.notify(
            "textDocument/didOpen",
            json!({"textDocument":{"uri":uri,"languageId":"rust","version":version,"text":text}}),
        )
    }

    fn close_uri(&mut self, uri: &str) -> Result<()> {
        lock(&self.live).documents.remove(uri);
        self.notify("textDocument/didClose", json!({"textDocument":{"uri":uri}}))
    }

    fn is_open(&self, uri: &str) -> bool {
        lock(&self.live).documents.contains_key(uri)
    }
}

/// Reads analyzer output until it closes: answers its requests, forwards
/// responses, and records readiness and diagnostics.
fn read_messages(
    stdout: ChildStdout,
    stdin: Arc<Mutex<ChildStdin>>,
    configuration: Value,
    live: Arc<Mutex<LiveState>>,
    generation: u64,
    responses: Sender<Value>,
) {
    let mut reader = BufReader::new(stdout);
    while let Some(message) = read_lsp(&mut reader) {
        let Some(message) = message else {
            continue;
        };
        match (message.get("id").is_some(), message["method"].as_str()) {
            (true, Some(_)) => {
                let _ = answer_server_request(&mut *lock(&stdin), &message, &configuration);
            }
            (true, None) => {
                if responses.send(message).is_err() {
                    break;
                }
            }
            (false, Some("experimental/serverStatus")) => {
                let mut state = lock(&live);
                if state.generation == generation {
                    state.status_seen = true;
                    state.server_status = message["params"].clone();
                }
            }
            (false, Some("textDocument/publishDiagnostics")) => {
                record_diagnostics(&live, generation, &message["params"]);
            }
            _ => {}
        }
    }
    let mut state = lock(&live);
    if state.generation == generation {
        state.exited = true;
    }
}

fn record_diagnostics(live: &Mutex<LiveState>, generation: u64, params: &Value) {
    let Some(uri) = params["uri"].as_str() else {
        return;
    };
    let version = params["version"].as_i64();
    // Unversioned diagnostics (for example `cargo check` results for files
    // that are not open) describe the file on disk.
    let disk_hash = version
        .is_none()
        .then(|| uri_to_path(uri).and_then(|path| file_hash(&path)))
        .flatten();
    let mut raw = params["diagnostics"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    raw.sort_by_key(|item| {
        (
            item["severity"].as_u64().unwrap_or(1),
            item["range"]["start"]["line"].as_u64().unwrap_or(0),
        )
    });
    let total = raw.len();
    let diagnostics = raw
        .iter()
        .take(MAX_DIAGNOSTICS_PER_FILE)
        .map(compact_diagnostic)
        .collect::<Vec<_>>();
    let mut state = lock(live);
    if state.generation != generation {
        return;
    }
    let document = state.documents.get(uri).cloned();
    let content_hash = match (version, &document) {
        (Some(version), Some(document)) => {
            (document.version == version).then(|| document.hash.clone())
        }
        (Some(_), None) => None,
        // An open document's diagnostics are versioned; an unversioned
        // publish for it is the clearing that follows a close.
        (None, Some(_)) => return,
        (None, None) => disk_hash,
    };
    if diagnostics.is_empty() && document.is_none() {
        state.diagnostics.remove(uri);
        return;
    }
    if !state.diagnostics.contains_key(uri) && state.diagnostics.len() >= MAX_DIAGNOSTIC_FILES {
        state.dropped_files += 1;
        return;
    }
    state.diagnostics.insert(
        uri.to_owned(),
        Published {
            version,
            content_hash,
            diagnostics,
            total,
            received_at: Utc::now().to_rfc3339(),
        },
    );
}

fn severity_name(severity: Option<u64>) -> &'static str {
    match severity {
        Some(2) => "warning",
        Some(3) => "information",
        Some(4) => "hint",
        _ => "error",
    }
}

fn severity_rank(name: &str) -> u64 {
    match name {
        "error" => 1,
        "warning" => 2,
        "information" => 3,
        _ => 4,
    }
}

/// A bounded diagnostic. `source` is `cargo check` for compiler and Clippy
/// results relayed by the analyzer and `rust-analyzer` for its own.
fn compact_diagnostic(raw: &Value) -> Value {
    let start = &raw["range"]["start"];
    let end = &raw["range"]["end"];
    let reported_by = raw["source"].as_str().unwrap_or("rust-analyzer");
    let source = if matches!(reported_by, "rustc" | "clippy") {
        "cargo check"
    } else {
        "rust-analyzer"
    };
    let message = raw["message"].as_str().unwrap_or_default();
    let message = if message.chars().count() > MAX_MESSAGE_CHARS {
        format!(
            "{}…",
            message.chars().take(MAX_MESSAGE_CHARS).collect::<String>()
        )
    } else {
        message.to_owned()
    };
    json!({
        "severity": severity_name(raw["severity"].as_u64()),
        "line": start["line"].as_u64().map(|line| line + 1),
        "character": start["character"],
        "end_line": end["line"].as_u64().map(|line| line + 1),
        "end_character": end["character"],
        "code": raw["code"],
        "source": source,
        "reported_by": reported_by,
        "message": message,
    })
}

/// The LSP form of a compact diagnostic, for code-action context.
pub(crate) fn lsp_diagnostic(compact: &Value) -> Value {
    let position = |line: &Value, character: &Value| json!({"line": line.as_u64().unwrap_or(1).saturating_sub(1), "character": character.as_u64().unwrap_or(0)});
    json!({
        "range": {"start": position(&compact["line"], &compact["character"]),
            "end": position(&compact["end_line"], &compact["end_character"])},
        "severity": severity_rank(compact["severity"].as_str().unwrap_or("error")),
        "code": compact["code"],
        "source": compact["reported_by"],
        "message": compact["message"],
    })
}

pub(crate) fn content_hash(bytes: &[u8]) -> String {
    format!("b3:{}", blake3::hash(bytes).to_hex())
}

fn file_hash(path: &Path) -> Option<String> {
    let metadata = fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_SOURCE_BYTES {
        return None;
    }
    fs::read(path).ok().map(|bytes| content_hash(&bytes))
}

/// The path of a `file://` URI, percent-decoded.
pub(crate) fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let encoded = uri.strip_prefix("file://")?.as_bytes();
    let mut bytes = Vec::with_capacity(encoded.len());
    let mut index = 0;
    while index < encoded.len() {
        if encoded[index] == b'%'
            && let Some(hex) = encoded.get(index + 1..index + 3)
            && let Ok(byte) = u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16)
        {
            bytes.push(byte);
            index += 3;
        } else {
            bytes.push(encoded[index]);
            index += 1;
        }
    }
    String::from_utf8(bytes).ok().map(PathBuf::from)
}

/// Answers a request the analyzer sends to its client.
fn answer_server_request(
    stdin: &mut impl Write,
    message: &Value,
    configuration: &Value,
) -> Result<()> {
    let (Some(id), Some(method)) = (message.get("id"), message["method"].as_str()) else {
        return Ok(());
    };
    let result = match method {
        "workspace/configuration" => json!(
            message["params"]["items"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|item| {
                    let section = item["section"].as_str().unwrap_or("");
                    if section.is_empty() || section == "rust-analyzer" {
                        configuration.clone()
                    } else if let Some(path) = section.strip_prefix("rust-analyzer.") {
                        path.split('.')
                            .fold(configuration, |value, key| &value[key])
                            .clone()
                    } else {
                        Value::Null
                    }
                })
                .collect::<Vec<_>>()
        ),
        "client/registerCapability"
        | "client/unregisterCapability"
        | "window/workDoneProgress/create"
        | "workspace/semanticTokens/refresh"
        | "workspace/diagnostic/refresh"
        | "workspace/inlayHint/refresh"
        | "workspace/codeLens/refresh" => Value::Null,
        "workspace/applyEdit" => {
            json!({"applied":false,"failureReason":"Crusty's semantic companion is read-only"})
        }
        _ => {
            return write_lsp(
                stdin,
                &json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Unsupported client method"}}),
            );
        }
    };
    write_lsp(stdin, &json!({"jsonrpc":"2.0","id":id,"result":result}))
}

pub(crate) fn write_lsp(out: &mut impl Write, value: &Value) -> Result<()> {
    let body = serde_json::to_vec(value)?;
    write!(out, "Content-Length: {}\r\n\r\n", body.len())?;
    out.write_all(&body)?;
    out.flush()?;
    Ok(())
}

/// The next message: `None` at end of output, `Some(None)` for a message
/// that was skipped because it is oversized or malformed.
pub(crate) fn read_lsp(reader: &mut impl BufRead) -> Option<Option<Value>> {
    let mut content_length = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        if let Some(value) = trimmed.strip_prefix("Content-Length:") {
            content_length = value.trim().parse::<usize>().ok();
        }
    }
    let Some(length) = content_length else {
        return Some(None);
    };
    if length > execution::MAX_CAPTURE_BYTES {
        io::copy(&mut reader.take(length as u64), &mut io::sink()).ok()?;
        return Some(None);
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).ok()?;
    Some(serde_json::from_slice(&body).ok())
}

/// Why no analyzer is available for a request.
#[derive(Debug, Clone)]
pub(crate) enum Unavailable {
    /// Restarts are paused after repeated failures.
    Backoff {
        retry_in: Duration,
        error: Option<String>,
    },
    Failed(String),
}

impl Unavailable {
    pub(crate) fn report(&self, program: &Path) -> Value {
        match self {
            Self::Backoff { retry_in, error } => json!({
                "state": "restarting", "complete": false, "error": error,
                "retry_in_ms": retry_in.as_millis() as u64, "program": program,
                "next": "rust-analyzer failed repeatedly; Crusty retries with backoff. semantic.status shows the last error."}),
            Self::Failed(error) => json!({
                "state": "failed", "complete": false, "error": error, "program": program,
                "next": "Check semantic.status; install rust-analyzer with `rustup component add rust-analyzer` or set RUST_REPO_INTELLIGENCE_RUST_ANALYZER_PATH."}),
        }
    }
}

/// The analyzer process; locked for the duration of one use.
#[derive(Default)]
pub(crate) struct Process {
    client: Option<RustAnalyzerClient>,
    launch: Option<Launch>,
    profile: Option<String>,
    scan: Option<WorkspaceScan>,
}

impl Process {
    pub(crate) fn client(&mut self) -> Result<&mut RustAnalyzerClient> {
        self.client.as_mut().context("semantic companion missing")
    }
}

/// Lifecycle facts for status reports, never locked across process I/O.
#[derive(Debug, Default)]
struct Lifecycle {
    start_attempted: bool,
    running: bool,
    starts: u64,
    crashes: u64,
    profile: Option<String>,
    profile_inputs: Value,
    capabilities: Value,
    started_at: Option<String>,
    last_error: Option<String>,
    last_exit: Option<String>,
    last_restart_reason: Option<String>,
    failures: u32,
    retry_at: Option<Instant>,
    warm_start: Option<&'static str>,
}

impl Lifecycle {
    fn failed(&mut self) {
        self.failures += 1;
        // The first failure restarts at once; repeated ones back off.
        let delay = match self.failures {
            0 | 1 => Duration::ZERO,
            failures => Duration::from_secs(1u64 << (failures - 2).min(6)).min(MAX_BACKOFF),
        };
        self.retry_at = Some(Instant::now() + delay);
    }

    fn retry_in(&self) -> Option<Duration> {
        self.retry_at
            .and_then(|at| at.checked_duration_since(Instant::now()))
            .filter(|delay| !delay.is_zero())
    }
}

/// The companion shared by every service of one server process.
#[derive(Default)]
pub(crate) struct SemanticBackend {
    /// Off until `semantic.enable` (or the autostart switch) turns it on.
    enabled: AtomicBool,
    process: Mutex<Process>,
    lifecycle: Mutex<Lifecycle>,
    live: Arc<Mutex<LiveState>>,
}

impl SemanticBackend {
    pub(crate) fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    pub(crate) fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::SeqCst);
    }

    /// Locks the process, polling so a cancelled task stops waiting.
    pub(crate) fn acquire(&self, control: &ExecutionControl) -> Result<MutexGuard<'_, Process>> {
        loop {
            control.check()?;
            match self.process.try_lock() {
                Ok(process) => return Ok(process),
                Err(TryLockError::Poisoned(error)) => return Ok(error.into_inner()),
                Err(TryLockError::WouldBlock) => thread::sleep(Duration::from_millis(10)),
            }
        }
    }

    pub(crate) fn lock_process(&self) -> MutexGuard<'_, Process> {
        lock(&self.process)
    }

    pub(crate) fn try_process(&self) -> Option<MutexGuard<'_, Process>> {
        match self.process.try_lock() {
            Ok(process) => Some(process),
            Err(TryLockError::Poisoned(error)) => Some(error.into_inner()),
            Err(TryLockError::WouldBlock) => None,
        }
    }

    /// Ensures a live analyzer for `launch` (`None` keeps the running
    /// profile, else the default) that has seen the current workspace, and
    /// returns its profile key. Restarts only for a dead process or changed
    /// profile inputs; source edits are synchronised instead.
    pub(crate) fn prepare(
        &self,
        process: &mut Process,
        root: &Path,
        program: &Path,
        launch: Option<&Launch>,
    ) -> Result<String, Unavailable> {
        let scan = WorkspaceScan::take(root);
        if let Some(client) = process.client.as_mut()
            && !client.alive()
        {
            let exit = client.exit_description();
            process.client = None;
            process.scan = None;
            let mut lifecycle = lock(&self.lifecycle);
            lifecycle.running = false;
            lifecycle.crashes += 1;
            lifecycle.last_exit = Some(exit);
            lifecycle.last_restart_reason = Some("the analyzer process exited".into());
            lifecycle.failed();
        }
        let launch = launch
            .cloned()
            .or_else(|| process.launch.clone())
            .unwrap_or_else(Launch::workspace_default);
        let version = rust_analyzer_version(program);
        let (key, inputs) = profile_key(&launch, version.as_deref(), &scan.cargo_digest);
        if process.client.is_some() {
            if process.profile.as_deref() == Some(key.as_str()) {
                self.synchronize(process, scan);
                if lock(&self.live).readiness() == Readiness::Ready {
                    lock(&self.lifecycle).failures = 0;
                }
                return Ok(key);
            }
            let mut lifecycle = lock(&self.lifecycle);
            let changed = inputs
                .as_object()
                .into_iter()
                .flatten()
                .filter(|(name, value)| lifecycle.profile_inputs.get(name.as_str()) != Some(value))
                .map(|(name, _)| name.replace('_', " "))
                .collect::<Vec<_>>();
            lifecycle.last_restart_reason = Some(format!("{} changed", changed.join(", ")));
            lifecycle.running = false;
            drop(lifecycle);
            process.client = None;
            process.scan = None;
        }
        let generation = {
            let mut lifecycle = lock(&self.lifecycle);
            if let Some(retry_in) = lifecycle.retry_in() {
                return Err(Unavailable::Backoff {
                    retry_in,
                    error: lifecycle.last_error.clone().or(lifecycle.last_exit.clone()),
                });
            }
            lifecycle.start_attempted = true;
            lifecycle.starts += 1;
            let mut live = lock(&self.live);
            let generation = live.generation + 1;
            *live = LiveState {
                generation,
                started: Some(Instant::now()),
                clock: live.clock,
                ..LiveState::default()
            };
            generation
        };
        match RustAnalyzerClient::start(root, program, &launch, self.live.clone(), generation) {
            Ok(client) => {
                let mut lifecycle = lock(&self.lifecycle);
                lifecycle.running = true;
                lifecycle.profile = Some(key.clone());
                lifecycle.profile_inputs = inputs;
                lifecycle.capabilities = client.capabilities.clone();
                lifecycle.started_at = Some(Utc::now().to_rfc3339());
                lifecycle.last_error = None;
                lifecycle.retry_at = None;
                process.client = Some(client);
                process.launch = Some(launch);
                process.profile = Some(key.clone());
                process.scan = Some(scan);
                Ok(key)
            }
            Err(error) => {
                let error = format!("{error:#}");
                let mut lifecycle = lock(&self.lifecycle);
                lifecycle.running = false;
                lifecycle.last_error = Some(error.clone());
                lifecycle.failed();
                Err(Unavailable::Failed(error))
            }
        }
    }

    /// Reports workspace changes since the last scan to the running
    /// analyzer: watched-file notifications for every change, and fresh
    /// content for open and recently changed documents.
    fn synchronize(&self, process: &mut Process, scan: WorkspaceScan) {
        let check_on_save = process.launch.as_ref().is_some_and(Launch::check_on_save);
        let previous = process.scan.replace(scan);
        let (Some(previous), Some(current), Some(client)) =
            (previous, process.scan.as_ref(), process.client.as_mut())
        else {
            return;
        };
        let changes = current.changes_since(&previous);
        if changes.is_empty() {
            return;
        }
        let events = changes
            .iter()
            .map(|(path, change)| json!({"uri": file_uri(path), "type": *change as u8}))
            .collect::<Vec<_>>();
        let _ = client.notify(
            "workspace/didChangeWatchedFiles",
            json!({"changes": events}),
        );
        let mut opened = 0;
        let mut saved = None;
        for (path, change) in &changes {
            if path.extension().is_none_or(|extension| extension != "rs") {
                continue;
            }
            let uri = file_uri(path);
            if *change == FileChange::Deleted {
                if client.is_open(&uri) {
                    let _ = client.close_uri(&uri);
                }
                continue;
            }
            let open = client.is_open(&uri);
            if !open && opened >= MAX_OPENED_PER_SYNC {
                continue;
            }
            let Some(text) = read_source(path) else {
                continue;
            };
            if client.sync_document(path, &text).is_ok() && !open {
                opened += 1;
            }
            saved.get_or_insert(uri);
        }
        if check_on_save && let Some(uri) = saved {
            // The analyzer runs `cargo check` for the workspace on save.
            let _ = client.notify("textDocument/didSave", json!({"textDocument":{"uri":uri}}));
        }
    }

    /// Opens `paths` in the analyzer so it publishes their diagnostics.
    /// Returns the URIs that are open.
    pub(crate) fn open_documents(&self, process: &mut Process, paths: &[PathBuf]) -> Vec<String> {
        let Some(client) = process.client.as_mut() else {
            return Vec::new();
        };
        paths
            .iter()
            .take(MAX_OPEN_DOCUMENTS / 2)
            .filter_map(|path| {
                let text = read_source(path)?;
                client.sync_document(path, &text).ok()?;
                Some(file_uri(path))
            })
            .collect()
    }

    pub(crate) fn readiness(&self) -> Readiness {
        lock(&self.live).readiness()
    }

    /// Waits until the analyzer can answer, or `timeout` passes.
    pub(crate) fn wait_ready(
        &self,
        timeout: Duration,
        control: &ExecutionControl,
    ) -> Result<Readiness> {
        let deadline = Instant::now() + timeout;
        loop {
            let readiness = self.readiness();
            if !matches!(readiness, Readiness::Starting | Readiness::Warming)
                || Instant::now() >= deadline
            {
                return Ok(readiness);
            }
            control.check()?;
            thread::sleep(Duration::from_millis(50));
        }
    }

    /// Waits until diagnostics for the current version of each open URI
    /// were published, or `timeout` passes.
    pub(crate) fn wait_diagnostics(
        &self,
        uris: &[String],
        timeout: Duration,
        control: &ExecutionControl,
    ) -> Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            let pending = {
                let live = lock(&self.live);
                !live.exited
                    && uris.iter().any(|uri| {
                        live.documents.get(uri).is_some_and(|document| {
                            live.diagnostics
                                .get(uri)
                                .is_none_or(|published| published.version != Some(document.version))
                        })
                    })
            };
            if !pending || Instant::now() >= deadline {
                return Ok(());
            }
            control.check()?;
            thread::sleep(Duration::from_millis(50));
        }
    }

    pub(crate) fn server_status(&self) -> Value {
        lock(&self.live).server_status.clone()
    }

    /// Captured diagnostics of one file in LSP form, for code-action context.
    pub(crate) fn lsp_diagnostics_at(&self, path: &Path, line: usize) -> Vec<Value> {
        let live = lock(&self.live);
        live.diagnostics
            .get(&file_uri(path))
            .into_iter()
            .flat_map(|published| &published.diagnostics)
            .filter(|item| {
                let start = item["line"].as_u64().unwrap_or(0) as usize;
                let end = item["end_line"].as_u64().unwrap_or(start as u64) as usize;
                (start..=end).contains(&(line + 1))
            })
            .map(lsp_diagnostic)
            .collect()
    }

    /// Stops the analyzer process.
    pub(crate) fn stop(&self) {
        let mut process = self.lock_process();
        process.client = None;
        process.scan = None;
        lock(&self.lifecycle).running = false;
    }

    fn set_warm_start(&self, state: &'static str) {
        lock(&self.lifecycle).warm_start = Some(state);
    }

    /// The analyzer state label used across status reports.
    pub(crate) fn state(&self, enabled: bool) -> &'static str {
        let lifecycle = lock(&self.lifecycle);
        let readiness = self.readiness();
        state_label(enabled, &lifecycle, readiness)
    }

    /// Compact backend facts for `index.status`.
    pub(crate) fn index_facts(&self, enabled: bool, program: &Path) -> Value {
        let lifecycle = lock(&self.lifecycle);
        let readiness = self.readiness();
        let state = state_label(enabled, &lifecycle, readiness);
        json!({
            "state": state,
            "running": lifecycle.running && readiness != Readiness::Exited,
            "start_attempted": lifecycle.start_attempted,
            "error": lifecycle.last_error,
            "restarts": lifecycle.starts.saturating_sub(1),
            "program_found": resolve_executable(program).is_some(),
            "outdated": enabled
                .then(|| version_currency(program, Utc::now().date_naive())["outdated"].clone()),
            "hint": hint(enabled, state, program, &lifecycle),
        })
    }

    /// Health, lifecycle, profile and capture state without starting anything.
    pub(crate) fn status(&self, enabled: bool, program: &Path) -> Value {
        let lifecycle = lock(&self.lifecycle);
        let live = lock(&self.live);
        let readiness = live.readiness();
        let state = state_label(enabled, &lifecycle, readiness);
        let warm_start = lifecycle.warm_start.unwrap_or(
            if warm_start_enabled(std::env::var(WARM_START_ENV).ok().as_deref()) {
                "not_started"
            } else {
                "disabled"
            },
        );
        json!({
            "enabled": enabled,
            "state": state,
            "running": lifecycle.running && readiness != Readiness::Exited,
            "readiness": (lifecycle.running).then_some(readiness.label()),
            "program": program,
            "program_found": resolve_executable(program).is_some(),
            "version": lifecycle.profile_inputs.get("version"),
            "currency": version_currency(program, Utc::now().date_naive()),
            "hint": hint(enabled, state, program, &lifecycle),
            "start_attempted": lifecycle.start_attempted,
            "started_at": lifecycle.started_at,
            "starts": lifecycle.starts,
            "restarts": lifecycle.starts.saturating_sub(1),
            "crashes": lifecycle.crashes,
            "last_error": lifecycle.last_error,
            "last_exit": lifecycle.last_exit,
            "last_restart_reason": lifecycle.last_restart_reason,
            "retry_in_ms": lifecycle.retry_in().map(|delay| delay.as_millis() as u64),
            "profile_digest": lifecycle.profile,
            "profile_inputs": lifecycle.profile_inputs,
            "capabilities": lifecycle.capabilities,
            "server_status": live.server_status,
            "warm_start": {"state": warm_start, "env": WARM_START_ENV},
            "enable_with": {"tool": "semantic.enable", "disable_tool": "semantic.disable", "autostart_env": AUTOSTART_ENV},
            "legacy_env_note": std::env::var_os(LEGACY_ENABLE_ENV).map(|_| format!(
                "{LEGACY_ENABLE_ENV} is set but no longer starts rust-analyzer; call semantic.enable, or set {AUTOSTART_ENV}=1 to start it with the server."
            )),
            "ready_timeout_seconds": configured_ready_timeout().as_secs(),
            "check_on_save": check_on_save_enabled(),
            "documents_open": live.documents.len(),
            "diagnostic_files": live.diagnostics.len(),
            "diagnostic_files_dropped": live.dropped_files,
        })
    }

    /// Captured diagnostics, optionally for `paths` only, labelled with
    /// per-file freshness against the content on disk.
    pub(crate) fn diagnostics_report(
        &self,
        root: &Path,
        paths: &[PathBuf],
        minimum: &str,
        limit: usize,
        enabled: bool,
    ) -> Value {
        let minimum = severity_rank(minimum);
        let (entries, readiness, dropped) = {
            let live = lock(&self.live);
            let entry = |uri: &str| {
                (
                    live.diagnostics.get(uri).cloned(),
                    live.documents.get(uri).map(|document| document.version),
                )
            };
            let entries = if paths.is_empty() {
                live.diagnostics
                    .keys()
                    .filter_map(|uri| {
                        let path = uri_to_path(uri)?;
                        path.starts_with(root).then(|| (path, entry(uri)))
                    })
                    .collect::<Vec<_>>()
            } else {
                paths
                    .iter()
                    .map(|path| (path.clone(), entry(&file_uri(path))))
                    .collect()
            };
            (entries, live.readiness(), live.dropped_files)
        };
        let state = {
            let lifecycle = lock(&self.lifecycle);
            state_label(enabled, &lifecycle, readiness)
        };
        let mut remaining = limit;
        let mut truncated = false;
        let mut summary = BTreeMap::from([
            ("error", 0),
            ("warning", 0),
            ("information", 0),
            ("hint", 0),
        ]);
        let mut unsettled = 0;
        let files = entries
            .into_iter()
            .map(|(path, (published, open_version))| {
                let relative = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                let disk = file_hash(&path);
                let freshness = match (&published, open_version) {
                    _ if !path.exists() => "missing",
                    (Some(published), _) => match &published.content_hash {
                        Some(hash) if Some(hash) == disk.as_ref() => "fresh",
                        Some(_) => "stale",
                        None => "unconfirmed",
                    },
                    (None, Some(_)) => "pending",
                    (None, None) => "not_analyzed",
                };
                if freshness != "fresh" {
                    unsettled += 1;
                }
                let Some(published) = published else {
                    return json!({"path": relative, "freshness": freshness, "document_version": open_version, "diagnostics": []});
                };
                let matching = published
                    .diagnostics
                    .iter()
                    .filter(|item| severity_rank(item["severity"].as_str().unwrap_or("error")) <= minimum)
                    .collect::<Vec<_>>();
                for item in &matching {
                    if let Some(count) = summary.get_mut(item["severity"].as_str().unwrap_or("error")) {
                        *count += 1;
                    }
                }
                let shown = matching.len().min(remaining);
                remaining -= shown;
                truncated |= shown < matching.len() || published.total > published.diagnostics.len();
                json!({
                    "path": relative,
                    "freshness": freshness,
                    "document_version": open_version,
                    "published_version": published.version,
                    "received_at": published.received_at,
                    "count": matching.len(),
                    "diagnostics": matching.into_iter().take(shown).collect::<Vec<_>>(),
                })
            })
            .collect::<Vec<_>>();
        let complete =
            readiness == Readiness::Ready && unsettled == 0 && !truncated && dropped == 0;
        json!({
            "state": state,
            "complete": complete,
            "files": files,
            "summary": summary,
            "truncated": truncated,
            "check_on_save": check_on_save_enabled(),
            "coverage": "rust-analyzer publishes its own diagnostics for files open in the companion (queried, requested by path, recently changed, or changed in a validated diff) and, with RUST_REPO_INTELLIGENCE_RUST_ANALYZER_CHECK_ON_SAVE=1, `cargo check` results for the workspace (source:\"cargo check\").",
            "freshness_contract": "fresh: diagnosed content equals the file on disk; stale: the file changed since; unconfirmed: no publish for the current version yet; pending: opened, not yet diagnosed; not_analyzed: never opened.",
            "authority": "Advisory live analyzer evidence; cargo/rustc results from verification.run remain authoritative.",
        })
    }
}

fn state_label(enabled: bool, lifecycle: &Lifecycle, readiness: Readiness) -> &'static str {
    if !enabled {
        "disabled"
    } else if lifecycle.running {
        match readiness {
            Readiness::Exited => "exited",
            readiness => readiness.label(),
        }
    } else if lifecycle.retry_in().is_some() {
        "restarting"
    } else if lifecycle.last_error.is_some() {
        "failed"
    } else {
        "not_started"
    }
}

fn hint(enabled: bool, state: &str, program: &Path, lifecycle: &Lifecycle) -> Option<String> {
    if !enabled {
        return Some(ENABLE_HINT.into());
    }
    if resolve_executable(program).is_none() {
        return Some(format!(
            "rust-analyzer was not found at {}; install it with `rustup component add rust-analyzer` or set {RUST_ANALYZER_PATH_ENV}.",
            program.display()
        ));
    }
    match state {
        "failed" | "restarting" | "exited" => Some(format!(
            "rust-analyzer is unavailable ({}); Crusty restarts it with backoff on the next use.",
            lifecycle
                .last_error
                .as_deref()
                .or(lifecycle.last_exit.as_deref())
                .unwrap_or("no error recorded")
        )),
        "starting" | "warming" => Some(format!(
            "rust-analyzer is loading the workspace; semantic.query waits up to {}s ({READY_TIMEOUT_ENV}).",
            configured_ready_timeout().as_secs()
        )),
        "not_started" => Some(
            "rust-analyzer starts in the background after server start, or on first semantic use when warm start is disabled."
                .into(),
        ),
        _ => version_currency(program, Utc::now().date_naive())["update_hint"]
            .as_str()
            .map(str::to_owned),
    }
}

pub(crate) fn read_source(path: &Path) -> Option<String> {
    let metadata = fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_SOURCE_BYTES {
        return None;
    }
    fs::read_to_string(path).ok()
}

/// Timing of the warm-start thread. Tests shorten it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct WarmTiming {
    /// Delay before the first start, so MCP initialisation is never contended.
    pub(crate) startup_delay: Duration,
    /// Interval of liveness checks and workspace synchronisation.
    pub(crate) poll: Duration,
}

impl Default for WarmTiming {
    fn default() -> Self {
        Self {
            startup_delay: Duration::from_secs(1),
            poll: Duration::from_secs(3),
        }
    }
}

/// Keeps the enabled analyzer running: starts it after server start,
/// restarts it after it dies (with backoff), and reports workspace edits so
/// its diagnostics stay current. Dropping the handle stops the thread.
pub struct SemanticWarmer {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    backend: Arc<SemanticBackend>,
}

impl SemanticWarmer {
    pub(crate) fn start(
        root: PathBuf,
        program: PathBuf,
        backend: Arc<SemanticBackend>,
        timing: WarmTiming,
    ) -> io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        backend.set_warm_start("active");
        let thread = {
            let stop = stop.clone();
            let backend = backend.clone();
            thread::Builder::new()
                .name("crusty-rust-analyzer-warm".into())
                .spawn(move || {
                    let mut wait = timing.startup_delay;
                    while pause(&stop, wait) {
                        if let Some(mut process) = backend.try_process() {
                            let _ = backend.prepare(&mut process, &root, &program, None);
                        }
                        wait = timing.poll;
                    }
                })?
        };
        Ok(Self {
            stop,
            thread: Some(thread),
            backend,
        })
    }

    /// Stops the thread and the analyzer process.
    pub fn shutdown(mut self) {
        self.halt();
        self.backend.stop();
    }

    fn halt(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        self.backend.set_warm_start("stopped");
    }
}

impl Drop for SemanticWarmer {
    fn drop(&mut self) {
        self.halt();
    }
}

/// Sleeps for `duration` unless stopped; returns whether to continue.
fn pause(stop: &AtomicBool, duration: Duration) -> bool {
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        if stop.load(Ordering::SeqCst) {
            return false;
        }
        thread::sleep(Duration::from_millis(25).min(duration));
    }
    !stop.load(Ordering::SeqCst)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn companion_requires_explicit_opt_in_and_warms_by_default() {
        assert!(!autostart_enabled(None));
        assert!(!autostart_enabled(Some("0")));
        assert!(!autostart_enabled(Some("false")));
        assert!(autostart_enabled(Some("1")));
        assert!(autostart_enabled(Some("TRUE")));
        assert!(autostart_enabled(Some(" yes ")));
        assert!(warm_start_enabled(None));
        assert!(!warm_start_enabled(Some("off")));
        assert_eq!(ready_timeout(None), DEFAULT_READY_TIMEOUT);
        assert_eq!(ready_timeout(Some("5")), Duration::from_secs(5));
        assert_eq!(ready_timeout(Some("99999")), MAX_READY_TIMEOUT);
        assert_eq!(
            rust_analyzer_program(
                Path::new("/"),
                Some(OsString::from("/custom/rust-analyzer"))
            ),
            PathBuf::from("/custom/rust-analyzer")
        );
    }

    #[test]
    fn analyzer_currency_flags_releases_older_than_two_cycles() {
        let date = |text| chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d").unwrap();
        assert_eq!(
            version_release_date("rust-analyzer 1.99.0 (b940084 2026-09-28)"),
            Some(date("2026-09-28"))
        );
        assert_eq!(
            version_release_date("rust-analyzer 1.101.0-nightly (c36f145 2026-10-01)"),
            Some(date("2026-10-01"))
        );
        assert_eq!(
            version_release_date("rust-analyzer 0.3.3049-standalone"),
            None
        );

        let temp = tempfile::tempdir().unwrap();
        let program = temp.path().join("rust-analyzer");
        fs::write(
            &program,
            "#!/bin/sh\necho 'rust-analyzer 1.94.1 (e408947 2026-03-25)'\n",
        )
        .unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        let current = version_currency(&program, date("2026-04-30"));
        assert_eq!(current["outdated"], false);
        assert!(current["update_hint"].is_null());
        let stale = version_currency(&program, date("2026-10-02"));
        assert_eq!(stale["outdated"], true);
        assert_eq!(stale["age_days"], 191);
        assert!(
            stale["update_hint"]
                .as_str()
                .unwrap()
                .contains("rustup update")
        );
    }

    #[test]
    fn rustup_resolves_the_analyzer_of_the_workspace_toolchain_pin() {
        let temp = tempfile::tempdir().unwrap();
        let nested = temp.path().join("crates/member");
        fs::create_dir_all(&nested).unwrap();
        assert_eq!(toolchain_pin(&nested), "");
        fs::write(
            temp.path().join("rust-toolchain.toml"),
            "[toolchain]\nchannel = \"1.98.0\"\n",
        )
        .unwrap();
        assert!(toolchain_pin(&nested).contains("1.98.0"));
    }

    #[test]
    fn companion_primes_caches_with_bounded_background_resources() {
        let options = rust_analyzer_initialization_options();
        let threads = options["numThreads"].as_u64().unwrap();
        assert!((1..=MAX_THREADS as u64).contains(&threads));
        assert_eq!(options["cachePriming"]["enable"], true);
        assert_eq!(options["cachePriming"]["numThreads"], threads);
        assert_eq!(
            options["cargo"]["extraEnv"]["CARGO_BUILD_JOBS"],
            threads.to_string()
        );
        assert_eq!(options["cargo"]["targetDir"], true);
        assert_eq!(options["files"]["watcher"], "client");
        assert_eq!(options["checkOnSave"], check_on_save_enabled());
    }

    #[test]
    fn default_profiles_share_one_key_that_ignores_source_contents() {
        assert_eq!(
            Launch::for_profile(&BuildProfile::default()),
            Launch::workspace_default()
        );
        let featured = Launch::for_profile(&BuildProfile {
            features: vec!["extra".into()],
            ..BuildProfile::default()
        });
        assert_eq!(featured.options["cargo"]["features"], json!(["extra"]));
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join(".cargo")).unwrap();
        fs::write(root.join("Cargo.toml"), "[package]\nname='p'\n").unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
        fs::write(root.join("README.md"), "ignored\n").unwrap();
        let launch = Launch::workspace_default();
        let first = WorkspaceScan::take(root);
        assert_eq!(first.files.len(), 2);
        fs::write(root.join("src/lib.rs"), "pub fn a() { let _ = 1; }\n").unwrap();
        fs::write(root.join("src/new.rs"), "").unwrap();
        let edited = WorkspaceScan::take(root);
        assert_eq!(
            profile_key(&launch, Some("ra 1"), &first.cargo_digest).0,
            profile_key(&launch, Some("ra 1"), &edited.cargo_digest).0
        );
        assert_eq!(
            edited.changes_since(&first),
            vec![
                (root.join("src/lib.rs"), FileChange::Changed),
                (root.join("src/new.rs"), FileChange::Created),
            ]
        );
        fs::remove_file(root.join("src/new.rs")).unwrap();
        fs::write(root.join(".cargo/config.toml"), "[build]\n").unwrap();
        let configured = WorkspaceScan::take(root);
        assert!(
            configured
                .changes_since(&edited)
                .contains(&(root.join("src/new.rs"), FileChange::Deleted))
        );
        assert_ne!(configured.cargo_digest, edited.cargo_digest);
        assert_ne!(
            profile_key(&launch, Some("ra 1"), &edited.cargo_digest).0,
            profile_key(&launch, Some("ra 2"), &edited.cargo_digest).0
        );
    }

    #[test]
    fn lsp_framing_skips_oversized_messages_and_uris_round_trip() {
        let small = br#"{"jsonrpc":"2.0","method":"x"}"#;
        let mut stream = format!(
            "Content-Length: {}\r\n\r\n",
            execution::MAX_CAPTURE_BYTES + 1
        )
        .into_bytes();
        stream.extend(vec![b' '; execution::MAX_CAPTURE_BYTES + 1]);
        stream.extend(format!("Content-Length: {}\r\n\r\n", small.len()).into_bytes());
        stream.extend(small);
        let mut reader = io::Cursor::new(stream);
        assert_eq!(read_lsp(&mut reader), Some(None));
        assert_eq!(read_lsp(&mut reader).unwrap().unwrap()["method"], "x");
        assert_eq!(read_lsp(&mut reader), None);
        let path = Path::new("/tmp/Rust 🦀/a%b.rs");
        assert_eq!(uri_to_path(&file_uri(path)).as_deref(), Some(path));
    }

    #[test]
    fn diagnostics_are_bounded_labelled_and_versioned() {
        let live = Mutex::new(LiveState {
            generation: 1,
            ..LiveState::default()
        });
        let uri = "file:///workspace/src/lib.rs";
        lock(&live).documents.insert(
            uri.into(),
            OpenDocument {
                version: 7,
                hash: "b3:seven".into(),
                used: 1,
            },
        );
        let diagnostic = |severity: u64, source: &str, line: u64| {
            json!({"range":{"start":{"line":line,"character":0},"end":{"line":line,"character":3}},
                "severity":severity,"source":source,"code":"E1","message":"m".repeat(3000)})
        };
        let mut many = (0..MAX_DIAGNOSTICS_PER_FILE as u64 + 5)
            .map(|line| diagnostic(2, "rust-analyzer", line))
            .collect::<Vec<_>>();
        many.push(diagnostic(1, "rustc", 9));
        record_diagnostics(&live, 1, &json!({"uri":uri,"version":7,"diagnostics":many}));
        // Other generations and close-time clearing of open documents are ignored.
        record_diagnostics(&live, 0, &json!({"uri":uri,"version":7,"diagnostics":[]}));
        record_diagnostics(&live, 1, &json!({"uri":uri,"diagnostics":[]}));
        let state = lock(&live);
        let published = &state.diagnostics[uri];
        assert_eq!(published.content_hash.as_deref(), Some("b3:seven"));
        assert_eq!(published.total, MAX_DIAGNOSTICS_PER_FILE + 6);
        assert_eq!(published.diagnostics.len(), MAX_DIAGNOSTICS_PER_FILE);
        let first = &published.diagnostics[0];
        assert_eq!(first["severity"], "error");
        assert_eq!(first["source"], "cargo check");
        assert_eq!(first["reported_by"], "rustc");
        assert_eq!(first["line"], 10);
        assert_eq!(
            first["message"].as_str().unwrap().chars().count(),
            MAX_MESSAGE_CHARS + 1
        );
        assert_eq!(published.diagnostics[1]["source"], "rust-analyzer");
        let lsp = lsp_diagnostic(first);
        assert_eq!(lsp["range"]["start"]["line"], 9);
        assert_eq!(lsp["severity"], 1);
        drop(state);
        // An outdated version is kept but unconfirmed; empty unopened files are dropped.
        record_diagnostics(
            &live,
            1,
            &json!({"uri":uri,"version":6,"diagnostics":[diagnostic(2, "rust-analyzer", 0)]}),
        );
        assert_eq!(lock(&live).diagnostics[uri].content_hash, None);
        record_diagnostics(
            &live,
            1,
            &json!({"uri":"file:///workspace/src/other.rs","diagnostics":[]}),
        );
        assert!(
            !lock(&live)
                .diagnostics
                .contains_key("file:///workspace/src/other.rs")
        );
    }

    /// A scripted analyzer that reports readiness, publishes two diagnostics
    /// for every opened document, logs each message, and records its pid.
    #[cfg(unix)]
    fn fake_analyzer(directory: &Path) -> Option<PathBuf> {
        use std::os::unix::fs::PermissionsExt;
        Command::new("python3").arg("--version").output().ok()?;
        let script = directory.join("fake-rust-analyzer");
        let log = directory.join("analyzer.log");
        let pid = directory.join("analyzer.pid");
        fs::write(
            &script,
            format!(
                r#"#!/usr/bin/env python3
import json, os, sys
if '--version' in sys.argv:
    print('rust-analyzer fake'); sys.exit(0)
open({pid:?}, 'w').write(str(os.getpid()))
log = open({log:?}, 'a')
def send(message):
    data = json.dumps(message).encode()
    sys.stdout.buffer.write(b'Content-Length: ' + str(len(data)).encode() + b'\r\n\r\n' + data)
    sys.stdout.buffer.flush()
while True:
    size = None
    while True:
        line = sys.stdin.buffer.readline()
        if not line: sys.exit(0)
        if line in (b'\r\n', b'\n'): break
        if line.startswith(b'Content-Length:'): size = int(line.split(b':')[1])
    message = json.loads(sys.stdin.buffer.read(size))
    method = message.get('method')
    log.write(str(method) + '\n'); log.flush()
    if method == 'initialize':
        send({{'jsonrpc':'2.0','id':message['id'],'result':{{'capabilities':{{'hoverProvider':True}}}}}})
    elif method == 'initialized':
        send({{'jsonrpc':'2.0','method':'experimental/serverStatus','params':{{'health':'ok','quiescent':True}}}})
    elif method == 'textDocument/didOpen':
        document = message['params']['textDocument']
        span = {{'start':{{'line':0,'character':0}},'end':{{'line':0,'character':1}}}}
        send({{'jsonrpc':'2.0','method':'textDocument/publishDiagnostics','params':{{'uri':document['uri'],'version':document['version'],
            'diagnostics':[{{'range':span,'severity':2,'source':'rust-analyzer','message':'fake warning'}},
                {{'range':span,'severity':1,'source':'rustc','message':'fake error'}}]}}}})
    elif 'id' in message:
        send({{'jsonrpc':'2.0','id':message['id'],'result':None}})
"#,
                pid = pid.to_string_lossy(),
                log = log.to_string_lossy(),
            ),
        )
        .ok()?;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).ok()?;
        Some(script)
    }

    #[cfg(unix)]
    #[test]
    fn companion_synchronises_edits_restarts_for_cargo_changes_and_after_death() {
        let temp = tempfile::tempdir().unwrap();
        let Some(program) = fake_analyzer(temp.path()) else {
            eprintln!("python3 unavailable; scripted analyzer fixture skipped");
            return;
        };
        let root = temp.path().join("workspace");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("Cargo.toml"), "[package]\nname='p'\n").unwrap();
        let lib = root.join("src/lib.rs");
        fs::write(&lib, "pub fn a() {}\n").unwrap();
        let backend = SemanticBackend::default();
        let control = ExecutionControl::default();
        let prepare = || {
            let mut process = backend.lock_process();
            backend
                .prepare(&mut process, &root, &program, None)
                .unwrap()
        };
        let open = |paths: &[PathBuf]| {
            let mut process = backend.lock_process();
            backend.open_documents(&mut process, paths)
        };
        let status = || backend.status(true, &program);
        let first = prepare();
        assert_eq!(
            backend
                .wait_ready(Duration::from_secs(10), &control)
                .unwrap(),
            Readiness::Ready
        );
        assert_eq!(status()["state"], "ready");
        let uris = open(std::slice::from_ref(&lib));
        backend
            .wait_diagnostics(&uris, Duration::from_secs(10), &control)
            .unwrap();
        let report =
            backend.diagnostics_report(&root, std::slice::from_ref(&lib), "hint", 10, true);
        let file = &report["files"][0];
        assert_eq!(file["path"], "src/lib.rs");
        assert_eq!(file["freshness"], "fresh", "{report}");
        assert_eq!(file["count"], 2);
        assert_eq!(file["diagnostics"][0]["source"], "cargo check");
        assert_eq!(file["diagnostics"][1]["source"], "rust-analyzer");
        assert_eq!(report["summary"]["error"], 1);
        assert_eq!(report["complete"], true);
        let errors = backend.diagnostics_report(&root, &[], "error", 10, true);
        assert_eq!(errors["files"][0]["count"], 1, "{errors}");
        let limited = backend.diagnostics_report(&root, &[], "hint", 1, true);
        assert_eq!(limited["truncated"], true);

        // A source edit is synchronised into the running analyzer.
        fs::write(&lib, "pub fn edited() {}\n").unwrap();
        let stale = backend.diagnostics_report(&root, std::slice::from_ref(&lib), "hint", 10, true);
        assert_eq!(stale["files"][0]["freshness"], "stale");
        assert_eq!(prepare(), first);
        backend
            .wait_diagnostics(&uris, Duration::from_secs(10), &control)
            .unwrap();
        let fresh = backend.diagnostics_report(&root, std::slice::from_ref(&lib), "hint", 10, true);
        assert_eq!(fresh["files"][0]["freshness"], "fresh", "{fresh}");
        assert_eq!(status()["starts"], 1);
        let log = fs::read_to_string(temp.path().join("analyzer.log")).unwrap();
        assert!(log.contains("workspace/didChangeWatchedFiles"), "{log}");
        assert!(log.contains("textDocument/didClose"), "{log}");
        assert_eq!(log.matches("initialize\n").count(), 1, "{log}");

        // Cargo configuration changes restart it.
        fs::write(root.join("Cargo.toml"), "[package]\nname='p'\n# changed\n").unwrap();
        let second = prepare();
        assert_ne!(second, first);
        assert_eq!(status()["starts"], 2);
        assert!(
            status()["last_restart_reason"]
                .as_str()
                .unwrap()
                .contains("cargo configuration")
        );

        // A dead child is detected and restarted.
        let pid = fs::read_to_string(temp.path().join("analyzer.pid")).unwrap();
        assert!(
            Command::new("kill")
                .args(["-9", pid.trim()])
                .status()
                .unwrap()
                .success()
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        while backend.readiness() != Readiness::Exited {
            assert!(Instant::now() < deadline, "the reader never saw the exit");
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(status()["state"], "exited");
        assert_eq!(prepare(), second);
        let restarted = status();
        assert_eq!(restarted["starts"], 3);
        assert_eq!(restarted["restarts"], 2);
        assert_eq!(restarted["crashes"], 1);
        assert!(restarted["last_exit"].as_str().unwrap().contains("exited"));
        assert_eq!(
            backend
                .wait_ready(Duration::from_secs(10), &control)
                .unwrap(),
            Readiness::Ready
        );
        backend.stop();
        assert_eq!(status()["state"], "not_started");
    }

    #[cfg(unix)]
    #[test]
    fn warm_start_runs_the_analyzer_without_any_request() {
        let temp = tempfile::tempdir().unwrap();
        let Some(program) = fake_analyzer(temp.path()) else {
            return;
        };
        let root = temp.path().join("workspace");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("Cargo.toml"), "[package]\nname='p'\n").unwrap();
        let backend = Arc::new(SemanticBackend::default());
        let warmer = SemanticWarmer::start(
            root,
            program.clone(),
            backend.clone(),
            WarmTiming {
                startup_delay: Duration::from_millis(10),
                poll: Duration::from_millis(50),
            },
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while backend.state(true) != "ready" {
            assert!(
                Instant::now() < deadline,
                "{}",
                backend.status(true, &program)
            );
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            backend.status(true, &program)["warm_start"]["state"],
            "active"
        );
        warmer.shutdown();
        let status = backend.status(true, &program);
        assert_eq!(status["warm_start"]["state"], "stopped");
        assert_eq!(status["running"], false);
    }
}

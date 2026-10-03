//! Resolve validation input locally without sending repository-sized patches
//! through the agent's tool arguments.

use anyhow::{Context, Result, ensure};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Debug, Clone, Copy, Default, Deserialize, JsonSchema)]
pub enum DiffTarget {
    /// Compare the base commit with the current committed HEAD.
    #[serde(rename = "HEAD")]
    Head,
    /// Compare the base commit with staged and unstaged tracked worktree files.
    #[default]
    #[serde(rename = "worktree")]
    Worktree,
}

#[derive(Debug, Clone)]
pub enum DiffSource {
    Inline(String),
    PatchFile(PathBuf),
    GitComparison {
        base_ref: String,
        target: DiffTarget,
    },
    /// Staged and unstaged tracked changes against HEAD (or against the index
    /// when the repository has no first commit yet), plus untracked files that
    /// Git does not ignore, as new-file diffs.
    Pending,
}

pub(crate) struct ResolvedDiff {
    pub text: String,
    pub scope: Value,
}

impl DiffSource {
    pub(crate) fn resolve(&self, root: &Path) -> Result<ResolvedDiff> {
        let (text, mut scope) = match self {
            Self::Inline(text) => (text.clone(), json!({"source":"inline"})),
            Self::PatchFile(path) => {
                ensure!(!path.as_os_str().is_empty(), "diff_path must not be empty");
                let path = root.join(path);
                let mut file = File::open(&path)
                    .with_context(|| format!("cannot open diff_path {}", path.display()))?;
                ensure!(
                    file.metadata()?.is_file(),
                    "diff_path must be a regular file"
                );
                let mut text = String::new();
                file.read_to_string(&mut text)
                    .with_context(|| format!("cannot read UTF-8 patch {}", path.display()))?;
                (text, json!({"source":"patch_file","diff_path":path}))
            }
            Self::GitComparison { base_ref, target } => {
                let base_commit = resolve_commit(root, base_ref)?;
                let head_commit = resolve_commit(root, "HEAD")?;
                let target_commit = match target {
                    DiffTarget::Head => Some(head_commit.as_str()),
                    DiffTarget::Worktree => None,
                };
                let text = git_diff(root, Some(&base_commit), target_commit)?;
                (
                    text,
                    json!({
                        "source":"git_comparison", "base_ref":base_ref,
                        "base_commit":base_commit, "head_commit":head_commit,
                        "target":match target { DiffTarget::Head => "HEAD", DiffTarget::Worktree => "worktree" },
                        "target_commit":target_commit, "tracked_only":true,
                        "comparison":"direct"
                    }),
                )
            }
            Self::Pending => {
                // An unborn HEAD has no commit to compare against. Git still
                // validates the repository when executing the index diff.
                let head_commit = resolve_commit(root, "HEAD").ok();
                let mut text = git_diff(root, head_commit.as_deref(), None)?;
                let untracked = untracked_diff(root, &mut text)?;
                (
                    text,
                    json!({
                        "source":"pending", "base_commit":head_commit,
                        "target":"worktree", "target_commit":null,
                        "tracked_only":false, "untracked":untracked,
                        "comparison":if head_commit.is_some() { "HEAD_to_worktree" } else { "index_to_worktree" }
                    }),
                )
            }
        };
        scope["bytes"] = json!(text.len());
        scope["hash"] = json!(format!("b3:{}", blake3::hash(text.as_bytes()).to_hex()));
        Ok(ResolvedDiff { text, scope })
    }
}

/// Untracked files beyond these bounds are listed as skipped, not diffed.
const MAX_UNTRACKED_FILES: usize = 500;
const MAX_UNTRACKED_FILE_BYTES: u64 = 1024 * 1024;

/// Appends a new-file diff for every untracked, non-ignored file to `text`,
/// so files an agent created count as changed without `git add -N`.
///
/// Crusty's own state directory and untracked `target/` output are never
/// part of a change, even in projects that do not ignore them yet.
fn untracked_diff(root: &Path, text: &mut String) -> Result<Value> {
    let listing = git_text(root, &["ls-files", "--others", "--exclude-standard", "-z"])?;
    let mut included = Vec::new();
    let mut skipped = Vec::new();
    for path in listing.split('\0').filter(|path| !path.is_empty()) {
        // A trailing slash marks a nested repository rather than a file.
        if path.ends_with('/')
            || path.starts_with(".rust-repo-intelligence/")
            || path.starts_with("target/")
        {
            continue;
        }
        let bytes = std::fs::symlink_metadata(root.join(path))
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        if included.len() >= MAX_UNTRACKED_FILES {
            skipped.push(json!({"path":path,"reason":"untracked file limit reached"}));
        } else if bytes > MAX_UNTRACKED_FILE_BYTES {
            skipped.push(json!({"path":path,"reason":"larger than 1 MiB","bytes":bytes}));
        } else if let Some(diff) = new_file_diff(root, path)? {
            text.push_str(&diff);
            included.push(path.to_owned());
        } else {
            skipped.push(json!({"path":path,"reason":"diff is not UTF-8"}));
        }
    }
    Ok(json!({
        "included":included.len(), "paths":included.iter().take(50).collect::<Vec<_>>(),
        "paths_truncated":included.len() > 50, "skipped":skipped.iter().take(50).collect::<Vec<_>>(),
        "skipped_count":skipped.len(),
        "rule":"untracked files that Git does not ignore are validated as new files"
    }))
}

/// `None` when the file's text diff is not UTF-8 (binary files are encoded).
fn new_file_diff(root: &Path, path: &str) -> Result<Option<String>> {
    let output = Command::new("git")
        .current_dir(root)
        .args([
            "diff",
            "--no-index",
            "--no-ext-diff",
            "--no-textconv",
            "--binary",
            "--no-color",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            "--",
            "/dev/null",
            path,
        ])
        .output()
        .context("cannot start Git for validation diff")?;
    // `--no-index` exits 1 when the inputs differ, which they always do here.
    ensure!(
        matches!(output.status.code(), Some(0 | 1)),
        "Git new-file diff for {path} failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8(output.stdout).ok())
}

fn git_text(root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .context("cannot start Git for validation diff")?;
    ensure!(
        output.status.success(),
        "Git validation diff failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    String::from_utf8(output.stdout).context("Git validation diff is not UTF-8")
}

fn resolve_commit(root: &Path, reference: &str) -> Result<String> {
    ensure!(!reference.trim().is_empty(), "base_ref must not be empty");
    // End option parsing before the user-controlled ref and require a commit,
    // rather than interpreting ranges, path arguments, or arbitrary options.
    let commit = format!("{reference}^{{commit}}");
    git_text(
        root,
        &["rev-parse", "--verify", "--end-of-options", &commit],
    )
    .with_context(|| format!("cannot resolve commit {reference:?}"))
    .map(|text| text.trim().to_owned())
}

fn git_diff(root: &Path, base: Option<&str>, target: Option<&str>) -> Result<String> {
    let mut args = vec![
        "diff",
        "--no-ext-diff",
        "--no-textconv",
        "--binary",
        "--no-color",
        "--src-prefix=a/",
        "--dst-prefix=b/",
    ];
    args.extend(base);
    args.extend(target);
    args.push("--");
    git_text(root, &args)
}

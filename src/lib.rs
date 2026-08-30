//! Repository indexing and change-impact services used by the MCP binary.
//! Semantic facts returned by a static scan are explicitly marked as such.

pub mod dashboard;
pub mod observatory;
mod quality;

pub use quality::{
    ProblemInput, QualityConstraintInput, QualityScope, ValidationOutcomeInput, ValidationRecipe,
};

use anyhow::{Context, Result, bail, ensure};
use chrono::Utc;
use fs2::FileExt;
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use regex::Regex;
use rusqlite::{Connection, OptionalExtension, params};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet, HashMap},
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        Mutex,
        mpsc::{self, Receiver},
    },
    thread,
    time::{Duration, Instant},
};
use syn::spanned::Spanned;
use syn::visit::Visit;
use walkdir::WalkDir;

const INDEX_DIRECTORY: &str = ".rust-repo-intelligence";
const MAX_SLICE_BYTES: usize = 12_000;
const MAX_SEARCH_BODY_BYTES: usize = 48_000;
/// Upper bound on a stored document body. `all_text_files` accepts `.json`,
/// `.xml`, `.yaml`, and `.md`, so a checked-in fixture could otherwise be read
/// whole into memory and inserted verbatim.
const MAX_DOCUMENT_BYTES: usize = 256_000;
/// Upper bound on the raw command output attached to a check result.
const MAX_CHECK_OUTPUT_BYTES: usize = 8_000;
/// Upper bound on structured diagnostics attached to a check result.
const MAX_CHECK_DIAGNOSTICS: usize = 40;
/// Superseded index generations retained for publishing history.
const MAX_RETAINED_GENERATIONS: i64 = 20;
const SCHEMA_VERSION: &str = "7";
const INDEXER_VERSION: &str = "8";
const RUST_ANALYZER_THREADS: u64 = 1;
const RUST_ANALYZER_ENV: &str = "RUST_REPO_INTELLIGENCE_ENABLE_RUST_ANALYZER";
const RUST_ANALYZER_PATH_ENV: &str = "RUST_REPO_INTELLIGENCE_RUST_ANALYZER_PATH";
const WATCHER_ENV: &str = "RUST_REPO_INTELLIGENCE_ENABLE_WATCHER";
const FEATURES_ENV: &str = "RUST_REPO_INTELLIGENCE_FEATURES";
const CHECKPOINT_REF_PREFIX: &str = "refs/codex/checkpoints";
const EMBEDDING_MODEL: &str = "subword-hash-v1";
const EMBEDDING_DIMENSIONS: usize = 192;
const SYMBOL_CARD_VERSION: &str = "symbol-card-v1";
const RRF_K: f64 = 60.0;
const WATCH_RECONCILE_INTERVAL: Duration = Duration::from_secs(30);
const WATCH_DEBOUNCE: Duration = Duration::from_millis(20);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Revision {
    pub head: Option<String>,
    pub dirty: bool,
    pub workspace_digest: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceSlice {
    pub symbol: String,
    pub file: String,
    pub range: [usize; 2],
    pub content_hash: String,
    pub reason: String,
    pub semantic_relationship: String,
    pub provenance: String,
    pub confidence: f64,
    pub stale: bool,
    pub source: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Node {
    pub id: i64,
    pub kind: String,
    pub canonical_name: String,
    pub crate_name: Option<String>,
    pub file: String,
    pub start_line: usize,
    pub end_line: usize,
    pub visibility: String,
    pub content_hash: String,
}

#[derive(Debug, Clone, Serialize)]
struct SemanticSnapshot {
    id: String,
    workspace_digest: String,
    cargo_lock_hash: Option<String>,
    target_triple: String,
    feature_profile: String,
    build_environment_fingerprint: String,
    rust_analyzer_version: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
struct SemanticProfile {
    cargo_lock_hash: Option<String>,
    target_triple: String,
    feature_profile: String,
    build_environment_fingerprint: String,
    rust_analyzer_version: Option<String>,
}

impl SemanticSnapshot {
    fn profile(&self) -> SemanticProfile {
        SemanticProfile {
            cargo_lock_hash: self.cargo_lock_hash.clone(),
            target_triple: self.target_triple.clone(),
            feature_profile: self.feature_profile.clone(),
            build_environment_fingerprint: self.build_environment_fingerprint.clone(),
            rust_analyzer_version: self.rust_analyzer_version.clone(),
        }
    }
}

#[derive(Debug, Clone)]
struct HybridHit {
    node: Node,
    score: f64,
    channels: BTreeSet<String>,
    exact_match: bool,
    embedding: Option<EmbeddingEvidence>,
}

#[derive(Debug, Clone, Serialize)]
struct EmbeddingEvidence {
    model: &'static str,
    dimensions: usize,
    card_version: &'static str,
    card_hash: String,
    semantic_snapshot: String,
    similarity: f64,
}

struct VectorHit {
    node: Node,
    similarity: f64,
    evidence: EmbeddingEvidence,
}

struct SymbolCard {
    text: String,
    hash: String,
}

#[derive(Default)]
struct EmbeddingBuildStats {
    embedded: usize,
    reused: usize,
}

struct EdgeRecord<'a> {
    source: i64,
    target: i64,
    kind: &'a str,
    confidence: f64,
    provenance: &'a str,
    revision: &'a str,
    metadata: Value,
}

struct EmbeddingCache {
    semantic_snapshot: String,
    items: Vec<(Node, Vec<f32>, String)>,
}

struct ParsedSymbol {
    kind: String,
    name: String,
    scope: Vec<String>,
    start_line: usize,
    end_line: usize,
    visibility: String,
    parser: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct SyntaxReference {
    path: Vec<String>,
    line: usize,
    kind: &'static str,
}

type LifecycleRecord = (
    i64,
    Option<i64>,
    String,
    String,
    String,
    f64,
    Node,
    Option<String>,
);

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RecordDecision {
    pub title: String,
    #[serde(default = "accepted")]
    pub status: String,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub applies_to: Vec<String>,
    #[serde(default)]
    pub consequences: Vec<String>,
    pub supersedes: Option<String>,
    #[serde(default)]
    pub materialize: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RecordSteering {
    pub title: String,
    pub instruction: String,
    #[serde(default)]
    pub scope: Vec<String>,
    #[serde(default = "normal")]
    pub priority: String,
    #[serde(default = "active")]
    pub status: String,
    pub expires_at: Option<String>,
}

/// Editable, evidence-backed repository work. Automatically discovered work is
/// always `proposed`; callers must explicitly accept it before it becomes a plan.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkItemInput {
    pub title: String,
    #[serde(default = "proposed")]
    pub status: String,
    #[serde(default = "normal")]
    pub priority: String,
    #[serde(default = "cleanup")]
    pub kind: String,
    #[serde(default)]
    pub scope: Vec<String>,
    #[serde(default)]
    pub evidence: Vec<String>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub blocked_by: Vec<String>,
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,
    #[serde(default)]
    pub verification: Vec<String>,
}

fn proposed() -> String {
    "proposed".into()
}
fn normal() -> String {
    "normal".into()
}
fn cleanup() -> String {
    "cleanup".into()
}

fn accepted() -> String {
    "accepted".into()
}
fn active() -> String {
    "active".into()
}

pub struct RustAnalyzerClient {
    _child: Child,
    stdin: ChildStdin,
    responses: Receiver<Value>,
    next_id: u64,
    opened_versions: HashMap<String, i64>,
}

impl RustAnalyzerClient {
    fn start(workspace: &Path, program: &Path) -> Result<Self> {
        let mut child = Command::new(program)
            .current_dir(workspace)
            // Keep the analyzer's Cargo subprocesses from multiplying the
            // worker limit below.  This is intentionally a conservative
            // default for a background MCP service.
            .env("CARGO_BUILD_JOBS", RUST_ANALYZER_THREADS.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("starting rust-analyzer at {}", program.display()))?;
        let stdout = child
            .stdout
            .take()
            .context("opening rust-analyzer stdout")?;
        let (sender, responses) = mpsc::channel();
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            while let Some(message) = read_lsp(&mut reader) {
                if sender.send(message).is_err() {
                    break;
                }
            }
        });
        let mut stdin = child.stdin.take().context("opening rust-analyzer stdin")?;
        let request = json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"processId": null, "rootUri": format!("file://{}", workspace.display()),
                "capabilities": {},
                "initializationOptions": rust_analyzer_initialization_options(),
                "workspaceFolders": [{"uri": format!("file://{}", workspace.display()), "name": "workspace"}]}
        });
        write_lsp(&mut stdin, &request)?;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let wait = deadline
                .checked_duration_since(Instant::now())
                .context("rust-analyzer initialization timed out")?;
            let response = responses
                .recv_timeout(wait)
                .context("waiting for rust-analyzer initialization")?;
            if response.get("id").and_then(Value::as_u64) == Some(1) {
                ensure!(
                    response.get("error").is_none(),
                    "rust-analyzer initialization failed: {}",
                    response["error"]
                );
                break;
            }
        }
        write_lsp(
            &mut stdin,
            &json!({"jsonrpc":"2.0","method":"initialized","params":{}}),
        )?;
        Ok(Self {
            _child: child,
            stdin,
            responses,
            next_id: 2,
            opened_versions: HashMap::new(),
        })
    }

    fn did_change(&mut self, uri: &str, text: &str) {
        if let Some(version) = self.opened_versions.get_mut(uri) {
            *version += 1;
            let _ = write_lsp(
                &mut self.stdin,
                &json!({"jsonrpc":"2.0","method":"textDocument/didChange","params":{"textDocument":{"uri":uri,"version":*version},"contentChanges":[{"text":text}]}}),
            );
        } else {
            self.opened_versions.insert(uri.to_owned(), 1);
            let _ = write_lsp(
                &mut self.stdin,
                &json!({"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":uri,"languageId":"rust","version":1,"text":text}}}),
            );
        }
    }

    fn locations(
        &mut self,
        method: &str,
        uri: &str,
        line: usize,
        character: usize,
    ) -> Option<Vec<Value>> {
        let id = self.next_id;
        self.next_id += 1;
        write_lsp(&mut self.stdin, &json!({
            "jsonrpc":"2.0", "id":id, "method":method,
            "params":{"textDocument":{"uri":uri},"position":{"line":line,"character":character},"context":{"includeDeclaration":true}}
        })).ok()?;
        let deadline = Instant::now() + Duration::from_secs(3);
        while let Some(wait) = deadline.checked_duration_since(Instant::now()) {
            let response = self.responses.recv_timeout(wait).ok()?;
            if response.get("id").and_then(Value::as_u64) == Some(id) {
                return response.get("result").and_then(Value::as_array).cloned();
            }
        }
        None
    }

    fn references(&mut self, uri: &str, line: usize, character: usize) -> Option<Vec<Value>> {
        self.locations("textDocument/references", uri, line, character)
    }

    fn implementations(&mut self, uri: &str, line: usize, character: usize) -> Option<Vec<Value>> {
        self.locations("textDocument/implementation", uri, line, character)
    }
}

fn rust_analyzer_initialization_options() -> Value {
    json!({
        // This process is not an editor and does not need diagnostics after
        // every didOpen notification.  Semantic reference requests still work.
        "checkOnSave": false,
        "cachePriming": {
            "enable": false,
            "numThreads": RUST_ANALYZER_THREADS,
        },
        "numThreads": RUST_ANALYZER_THREADS,
        "cargo": {
            "extraEnv": {
                "CARGO_BUILD_JOBS": RUST_ANALYZER_THREADS.to_string(),
            },
        },
        "check": {
            "extraEnv": {
                "CARGO_BUILD_JOBS": RUST_ANALYZER_THREADS.to_string(),
            },
        },
    })
}

fn rust_analyzer_enabled(value: Option<&str>) -> bool {
    value.is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

fn rust_analyzer_program(explicit: Option<OsString>) -> PathBuf {
    if let Some(explicit) = explicit {
        return PathBuf::from(explicit);
    }
    let from_rustup = Command::new("rustup")
        .args(["which", "rust-analyzer"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|path| !path.is_empty());
    from_rustup
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("rust-analyzer"))
}

fn watcher_enabled(value: Option<&str>) -> bool {
    if cfg!(test) && value.is_none() {
        return false;
    }
    !value.is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        )
    })
}

fn start_watcher(
    root: &Path,
) -> (
    Option<RecommendedWatcher>,
    Option<Receiver<notify::Result<Event>>>,
) {
    if !watcher_enabled(std::env::var(WATCHER_ENV).ok().as_deref()) {
        return (None, None);
    }
    let (sender, receiver) = mpsc::sync_channel(1_024);
    let Ok(mut watcher) = notify::recommended_watcher(move |event| {
        let _ = sender.try_send(event);
    }) else {
        return (None, None);
    };
    if watcher.watch(root, RecursiveMode::Recursive).is_err() {
        return (None, None);
    }
    (Some(watcher), Some(receiver))
}

impl Drop for RustAnalyzerClient {
    fn drop(&mut self) {
        let _ = self._child.kill();
        let _ = self._child.wait();
    }
}

fn write_lsp(out: &mut ChildStdin, value: &Value) -> Result<()> {
    let body = serde_json::to_vec(value)?;
    write!(out, "Content-Length: {}\r\n\r\n", body.len())?;
    out.write_all(&body)?;
    out.flush()?;
    Ok(())
}

fn read_lsp(reader: &mut impl BufRead) -> Option<Value> {
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
    let mut body = vec![0; content_length?];
    reader.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}

pub struct Service {
    root: PathBuf,
    db: Connection,
    ra: Option<Mutex<RustAnalyzerClient>>,
    ra_enabled: bool,
    ra_start_attempted: bool,
    ra_program: PathBuf,
    ra_start_error: Option<String>,
    watcher: Option<RecommendedWatcher>,
    watch_events: Option<Receiver<notify::Result<Event>>>,
    watcher_trusted: bool,
    last_reconcile: Instant,
    embedding_cache: RefCell<Option<EmbeddingCache>>,
}

struct PublisherLease(File);

impl Drop for PublisherLease {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}

impl Service {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = fs::canonicalize(root.into()).context("resolving workspace path")?;
        let index_dir = root.join(INDEX_DIRECTORY);
        fs::create_dir_all(&index_dir)?;
        let db = Connection::open(index_dir.join("index.sqlite3"))?;
        // A refresh holds one write transaction for its whole duration. Without
        // a busy timeout every concurrent reader failed outright with
        // SQLITE_BUSY instead of waiting, contradicting the contract
        // `index.status` advertises about reads during a refresh.
        db.busy_timeout(std::time::Duration::from_secs(15))?;
        initialize_schema(&db)?;
        let (watcher, watch_events) = start_watcher(&root);
        let ra_program = rust_analyzer_program(
            std::env::var_os(RUST_ANALYZER_PATH_ENV).filter(|value| !value.is_empty()),
        );
        // MCP clients impose a short initialization deadline.  Indexing a large
        // workspace here makes the server appear unavailable. Cache creation and
        // the optional rust-analyzer companion are therefore both lazy.
        Ok(Self {
            root,
            db,
            ra: None,
            ra_enabled: rust_analyzer_enabled(std::env::var(RUST_ANALYZER_ENV).ok().as_deref()),
            ra_start_attempted: false,
            ra_program,
            ra_start_error: None,
            watcher,
            watch_events,
            watcher_trusted: false,
            last_reconcile: Instant::now(),
            embedding_cache: RefCell::new(None),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn start_rust_analyzer_if_enabled(&mut self) {
        if !self.ra_enabled || self.ra_start_attempted {
            return;
        }
        self.ra_start_attempted = true;
        match RustAnalyzerClient::start(&self.root, &self.ra_program) {
            Ok(client) => {
                self.ra = Some(Mutex::new(client));
                self.ra_start_error = None;
            }
            Err(error) => {
                self.ra = None;
                self.ra_start_error = Some(format!("{error:#}"));
            }
        }
    }

    pub fn revision(&self) -> Revision {
        self.revision_from_inputs(&index_inputs(&self.root))
    }

    fn revision_from_inputs(&self, inputs: &BTreeMap<String, (String, String)>) -> Revision {
        self.revision_from_inputs_and_head(inputs, command_text(&self.root, &["rev-parse", "HEAD"]))
    }

    fn revision_from_inputs_and_head(
        &self,
        inputs: &BTreeMap<String, (String, String)>,
        head: Option<String>,
    ) -> Revision {
        let dirty = git_output(
            &self.root,
            &["status", "--porcelain=v2", "-z", "--untracked-files=all"],
        )
        .is_some_and(|output| !output.is_empty());
        let mut hasher = blake3::Hasher::new();
        for (path, (_, hash)) in inputs {
            hasher.update(path.as_bytes());
            hasher.update(hash.as_bytes());
        }
        Revision {
            head,
            dirty,
            workspace_digest: format!("b3:{}", hasher.finalize().to_hex()),
        }
    }

    fn semantic_snapshot(&self, inputs: &BTreeMap<String, (String, String)>) -> SemanticSnapshot {
        let mut workspace = blake3::Hasher::new();
        for (path, (_, hash)) in inputs
            .iter()
            .filter(|(_, (kind, _))| matches!(kind.as_str(), "source" | "cargo"))
        {
            workspace.update(path.as_bytes());
            workspace.update(hash.as_bytes());
        }
        let semantic_workspace_digest = format!("b3:{}", workspace.finalize().to_hex());
        let cargo_lock_hash = inputs.get("Cargo.lock").map(|(_, hash)| hash.clone());
        let target_triple = std::env::var("CARGO_BUILD_TARGET")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(rustc_host_triple)
            .unwrap_or_else(|| "host-unknown".into());
        let feature_profile = std::env::var(FEATURES_ENV)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "cargo-default".into());
        let mut environment = blake3::Hasher::new();
        for key in [
            "RUSTFLAGS",
            "RUSTDOCFLAGS",
            "CARGO_ENCODED_RUSTFLAGS",
            "CARGO_PROFILE",
            "OUT_DIR",
        ] {
            environment.update(key.as_bytes());
            if let Ok(value) = std::env::var(key) {
                environment.update(value.as_bytes());
            }
        }
        let build_environment_fingerprint = format!("b3:{}", environment.finalize().to_hex());
        let rust_analyzer_version = self
            .ra_enabled
            .then(|| rust_analyzer_version(&self.ra_program))
            .flatten();
        let identity = json!({
            "workspace_digest": semantic_workspace_digest,
            "cargo_lock_hash": cargo_lock_hash,
            "target_triple": target_triple,
            "feature_profile": feature_profile,
            "build_environment_fingerprint": build_environment_fingerprint,
            "rust_analyzer_version": rust_analyzer_version,
        });
        let id = format!(
            "sem_{}",
            &blake3::hash(identity.to_string().as_bytes()).to_hex()[..16]
        );
        SemanticSnapshot {
            id,
            workspace_digest: semantic_workspace_digest,
            cargo_lock_hash,
            target_triple,
            feature_profile,
            build_environment_fingerprint,
            rust_analyzer_version,
        }
    }

    fn active_semantic_snapshot_id(&self) -> Option<String> {
        self.db
            .query_row(
                "SELECT value FROM metadata WHERE key='semantic_snapshot'",
                [],
                |row| row.get(0),
            )
            .optional()
            .ok()
            .flatten()
    }

    fn active_semantic_profile_matches(&self, candidate: &SemanticSnapshot) -> Result<bool> {
        let Some(snapshot_id) = self.active_semantic_snapshot_id() else {
            return Ok(false);
        };
        let profile: Option<SemanticProfile> = self
            .db
            .query_row(
                "SELECT cargo_lock_hash,target_triple,feature_profile,build_environment_fingerprint,rust_analyzer_version \
                 FROM semantic_snapshots WHERE id=?1",
                [&snapshot_id],
                |row| {
                    Ok(SemanticProfile {
                        cargo_lock_hash: row.get(0)?,
                        target_triple: row.get(1)?,
                        feature_profile: row.get(2)?,
                        build_environment_fingerprint: row.get(3)?,
                        rust_analyzer_version: row.get(4)?,
                    })
                },
            )
            .optional()?;
        Ok(profile.is_some_and(|profile| profile == candidate.profile()))
    }

    fn active_generation(&self) -> Option<i64> {
        self.db
            .query_row(
                "SELECT value FROM metadata WHERE key='active_generation'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .ok()
            .flatten()
            .and_then(|value| value.parse().ok())
    }

    fn semantic_snapshot_resource(&self) -> Value {
        let Some(id) = self.active_semantic_snapshot_id() else {
            return Value::Null;
        };
        self.db
            .query_row(
                "SELECT id,workspace_digest,cargo_lock_hash,target_triple,feature_profile,build_environment_fingerprint,rust_analyzer_version,created_at FROM semantic_snapshots WHERE id=?1",
                [&id],
                |row| Ok(json!({
                    "id":row.get::<_,String>(0)?,
                    "workspace_digest":row.get::<_,String>(1)?,
                    "cargo_lock_hash":row.get::<_,Option<String>>(2)?,
                    "target_triple":row.get::<_,String>(3)?,
                    "feature_profile":row.get::<_,String>(4)?,
                    "build_environment_fingerprint":row.get::<_,String>(5)?,
                    "rust_analyzer_version":row.get::<_,Option<String>>(6)?,
                    "created_at":row.get::<_,String>(7)?,
                })),
            )
            .unwrap_or(Value::Null)
    }

    pub fn reindex(&mut self) -> Result<()> {
        let _publisher_lease = self.acquire_publisher_lease()?;
        self.reindex_unlocked()
    }

    fn reindex_unlocked(&mut self) -> Result<()> {
        self.start_rust_analyzer_if_enabled();
        self.with_savepoint("full_reindex", |service| service.reindex_inner())?;
        self.watcher_trusted = self.watcher.is_some();
        self.last_reconcile = Instant::now();
        Ok(())
    }

    fn reindex_inner(&mut self) -> Result<()> {
        // Cargo may materialize Cargo.lock on first metadata resolution. Establish
        // the indexed snapshot only after that deterministic side effect.
        let cargo = cargo_metadata(&self.root)?;
        let inputs = index_inputs(&self.root);
        let revision = self.revision_from_inputs(&inputs);
        let semantic = self.semantic_snapshot(&inputs);
        self.db
            .execute("DELETE FROM edges WHERE provenance = 'StaticIndex'", [])?;
        self.db
            .execute("DELETE FROM edges WHERE provenance = 'RustAnalyzer'", [])?;
        self.db.execute("DELETE FROM nodes", [])?;
        self.db.execute("DELETE FROM packages", [])?;
        self.db.execute("DELETE FROM package_dependencies", [])?;
        self.db.execute("DELETE FROM package_targets", [])?;
        self.db.execute("DELETE FROM package_features", [])?;
        self.db.execute("DELETE FROM commits", [])?;
        self.db.execute("DELETE FROM commit_files", [])?;
        self.db.execute("DELETE FROM co_changes", [])?;
        self.db.execute("DELETE FROM documents", [])?;
        self.db.execute("DELETE FROM symbol_embeddings", [])?;
        self.db.execute("DELETE FROM use_case_nodes", [])?;
        self.db.execute("DELETE FROM use_cases", [])?;
        let packages = cargo_packages_from_metadata(&cargo);
        for package in &packages {
            self.db.execute("INSERT OR REPLACE INTO packages(name, manifest_path, metadata, revision) VALUES (?1, ?2, ?3, ?4)", params![package.0, package.1, package.2, revision.workspace_digest])?;
        }
        for (source, target, context) in cargo_dependency_edges_from_metadata(&cargo) {
            self.db.execute(
                "INSERT OR REPLACE INTO package_dependencies(source, target, context_json, revision) VALUES (?1, ?2, ?3, ?4)",
                params![source, target, context, revision.workspace_digest],
            )?;
        }
        self.index_cargo_matrix(&cargo, &revision.workspace_digest)?;
        let nodes = self.index_source_files(&rust_files(&self.root), &packages)?;
        self.index_static_references(&nodes, &revision.workspace_digest)?;
        self.index_structural_edges(&nodes, &revision.workspace_digest)?;
        self.index_lifecycle_evidence(&nodes, &revision.workspace_digest)?;
        self.relink_decision_targets()?;
        self.index_documents_and_use_cases(&nodes, &revision.workspace_digest)?;
        self.index_proposed_work(&revision.workspace_digest)?;
        self.index_git(&revision.workspace_digest)?;
        self.rebuild_symbol_embeddings(&nodes, &semantic.id)?;
        self.rebuild_search_index()?;
        ensure!(
            index_inputs(&self.root) == inputs,
            "workspace changed during full indexing; retry against a stable snapshot"
        );
        self.record_index_state(&revision, &inputs)?;
        Ok(())
    }

    fn with_savepoint<T>(
        &mut self,
        name: &str,
        operation: impl FnOnce(&mut Self) -> Result<T>,
    ) -> Result<T> {
        self.db.execute_batch(&format!("SAVEPOINT {name}"))?;
        match operation(self) {
            Ok(value) => {
                self.db.execute_batch(&format!("RELEASE {name}"))?;
                Ok(value)
            }
            Err(error) => {
                let _ = self
                    .db
                    .execute_batch(&format!("ROLLBACK TO {name}; RELEASE {name}"));
                Err(error)
            }
        }
    }

    /// Refresh only the portions invalidated by changed inputs. Cargo and toolchain
    /// inputs deliberately trigger a complete semantic refresh; documentation alone
    /// never invalidates source symbols or graph edges.
    pub fn refresh_if_stale(&mut self) -> Result<()> {
        let _publisher_lease = self.acquire_publisher_lease()?;
        self.start_rust_analyzer_if_enabled();
        let previous: BTreeMap<String, (String, String)> = self
            .db
            .prepare("SELECT path, kind, content_hash FROM input_state")?
            .query_map([], |row| Ok((row.get(0)?, (row.get(1)?, row.get(2)?))))?
            .collect::<rusqlite::Result<_>>()?;
        if previous.is_empty() {
            return self.reindex_unlocked();
        }
        let indexed_version: Option<String> = self
            .db
            .query_row(
                "SELECT value FROM metadata WHERE key='indexer_version'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        if indexed_version.as_deref() != Some(INDEXER_VERSION) {
            return self.reindex_unlocked();
        }
        let (watch_paths, watcher_overflowed) = self.drain_watch_events();
        let reconcile = !self.watcher_trusted
            || self.watcher.is_none()
            || watcher_overflowed
            || self.last_reconcile.elapsed() >= WATCH_RECONCILE_INTERVAL;
        let inputs = if reconcile {
            index_inputs(&self.root)
        } else {
            inputs_from_watch_paths(&self.root, &previous, &watch_paths)
        };
        let indexed_head: Option<String> = self
            .db
            .query_row(
                "SELECT value FROM metadata WHERE key='git_indexed_head'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .filter(|head| !head.is_empty());
        let current_head = command_text(&self.root, &["rev-parse", "HEAD"]);
        if !reconcile && watch_paths.is_empty() && indexed_head == current_head {
            return Ok(());
        }
        let revision = self.revision_from_inputs_and_head(&inputs, current_head);
        let head_changed = indexed_head != revision.head;
        let semantic = self.semantic_snapshot(&inputs);
        if !self.active_semantic_profile_matches(&semantic)? {
            return self.reindex_unlocked();
        }
        let changed: Vec<(String, String)> = inputs
            .iter()
            .filter(|(path, (kind, hash))| {
                previous.get(*path) != Some(&(kind.clone(), hash.clone()))
            })
            .map(|(path, (kind, _))| (path.clone(), kind.clone()))
            .chain(
                previous
                    .iter()
                    .filter(|(path, _)| !inputs.contains_key(*path))
                    .map(|(path, (kind, _))| (path.clone(), kind.clone())),
            )
            .collect();
        if changed.is_empty() {
            if head_changed {
                self.with_savepoint("git_refresh", |service| {
                    service.refresh_git(&revision.workspace_digest)?;
                    service.rebuild_search_index()?;
                    service.record_index_state(&revision, &inputs)
                })?;
            }
            self.watcher_trusted = self.watcher.is_some();
            if reconcile {
                self.last_reconcile = Instant::now();
            }
            return Ok(());
        }
        if changed.iter().any(|(_, kind)| kind == "cargo") {
            return self.reindex_unlocked();
        }
        self.with_savepoint("incremental_refresh", |service| {
            service.refresh_changed(&changed, &revision, &inputs, head_changed)
        })?;
        self.watcher_trusted = self.watcher.is_some();
        if reconcile {
            self.last_reconcile = Instant::now();
        }
        Ok(())
    }

    fn drain_watch_events(&self) -> (BTreeSet<PathBuf>, bool) {
        let Some(events) = &self.watch_events else {
            return (BTreeSet::new(), false);
        };
        let mut paths = BTreeSet::new();
        let mut overflowed = false;
        match events.recv_timeout(Duration::from_millis(2)) {
            Ok(Ok(event)) => paths.extend(event.paths),
            Ok(Err(_)) | Err(mpsc::RecvTimeoutError::Disconnected) => overflowed = true,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        for event in events.try_iter() {
            match event {
                Ok(event) => paths.extend(event.paths),
                Err(_) => overflowed = true,
            }
        }
        if !paths.is_empty() {
            thread::sleep(WATCH_DEBOUNCE);
            for event in events.try_iter() {
                match event {
                    Ok(event) => paths.extend(event.paths),
                    Err(_) => overflowed = true,
                }
            }
        }
        paths.retain(|path| watch_path_relevant(&self.root, path));
        (paths, overflowed)
    }

    fn refresh_changed(
        &mut self,
        changed: &[(String, String)],
        revision: &Revision,
        inputs: &BTreeMap<String, (String, String)>,
        head_changed: bool,
    ) -> Result<()> {
        let semantic = self.semantic_snapshot(inputs);
        let source_paths: Vec<PathBuf> = changed
            .iter()
            .filter(|(_, kind)| kind == "source")
            .map(|(path, _)| self.root.join(path))
            .collect();
        if !source_paths.is_empty() {
            let broad_semantic_invalidation = source_paths.iter().any(|path| {
                let file = relative(&self.root, path);
                self.db
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM nodes WHERE file=?1 AND (visibility='public' OR kind IN ('trait','impl','macro_rules!')))",
                        [&file],
                        |row| row.get::<_, bool>(0),
                    )
                    .unwrap_or(true)
            });
            if broad_semantic_invalidation {
                self.db
                    .execute("DELETE FROM edges WHERE provenance='RustAnalyzer'", [])?;
            }
            for path in &source_paths {
                self.db.execute(
                    "DELETE FROM nodes WHERE file = ?1",
                    [relative(&self.root, path)],
                )?;
            }
            let packages = cargo_packages(&self.root);
            let existing: Vec<PathBuf> = source_paths
                .into_iter()
                .filter(|path| path.exists())
                .collect();
            self.index_source_files(&existing, &packages)?;
            self.db.execute(
                "DELETE FROM edges WHERE provenance IN ('StaticIndex','Syntax')",
                [],
            )?;
            let nodes = self.all_nodes()?;
            self.index_static_references(&nodes, &revision.workspace_digest)?;
            self.index_structural_edges(&nodes, &revision.workspace_digest)?;
            if !broad_semantic_invalidation {
                self.db.execute(
                    "UPDATE edges SET revision=?1 WHERE provenance='RustAnalyzer'",
                    [&semantic.id],
                )?;
            }
            self.index_lifecycle_evidence(&nodes, &revision.workspace_digest)?;
            self.relink_decision_targets()?;
            self.refresh_documents_and_use_cases(&nodes, &revision.workspace_digest)?;
            self.index_proposed_work(&revision.workspace_digest)?;
            self.rebuild_symbol_embeddings(&nodes, &semantic.id)?;
        } else {
            let nodes = self.all_nodes()?;
            self.refresh_documents_and_use_cases(&nodes, &revision.workspace_digest)?;
            self.index_proposed_work(&revision.workspace_digest)?;
        }
        if head_changed {
            self.refresh_git(&revision.workspace_digest)?;
        }
        self.rebuild_search_index()?;
        ensure!(
            changed_inputs_still_match(&self.root, inputs, changed),
            "workspace changed during incremental indexing; retry against a stable snapshot"
        );
        self.record_index_state(revision, inputs)
    }

    fn record_index_state(
        &self,
        revision: &Revision,
        inputs: &BTreeMap<String, (String, String)>,
    ) -> Result<()> {
        let semantic = self.semantic_snapshot(inputs);
        self.db.execute(
            "INSERT OR REPLACE INTO semantic_snapshots(id,workspace_digest,cargo_lock_hash,target_triple,feature_profile,build_environment_fingerprint,rust_analyzer_version,created_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![semantic.id,semantic.workspace_digest,semantic.cargo_lock_hash,semantic.target_triple,semantic.feature_profile,semantic.build_environment_fingerprint,semantic.rust_analyzer_version,Utc::now().to_rfc3339()],
        )?;
        self.db.execute(
            "UPDATE index_generations SET status='superseded' WHERE status='published'",
            [],
        )?;
        self.db.execute(
            "INSERT INTO index_generations(workspace_digest,semantic_snapshot,status,created_at,published_at) VALUES (?1,?2,'published',?3,?3)",
            params![revision.workspace_digest,semantic.id,Utc::now().to_rfc3339()],
        )?;
        let generation = self.db.last_insert_rowid();
        // Superseded generations were never deleted, so the table grew by one
        // row per refresh forever. A bounded tail is enough to explain recent
        // publishing history.
        self.db.execute(
            "DELETE FROM index_generations WHERE status='superseded' AND id NOT IN (\
                 SELECT id FROM index_generations WHERE status='superseded' ORDER BY id DESC LIMIT ?1\
             )",
            [MAX_RETAINED_GENERATIONS],
        )?;
        self.db.execute(
            "INSERT OR REPLACE INTO metadata(key, value) VALUES ('revision', ?1)",
            [serde_json::to_string(revision)?],
        )?;
        self.db.execute(
            "INSERT OR REPLACE INTO metadata(key, value) VALUES ('indexed_head', ?1)",
            [revision.head.clone().unwrap_or_default()],
        )?;
        self.db.execute(
            "INSERT OR REPLACE INTO metadata(key, value) VALUES ('git_indexed_head', ?1)",
            [revision.head.clone().unwrap_or_default()],
        )?;
        self.db.execute(
            "INSERT OR REPLACE INTO metadata(key, value) VALUES ('indexed_at', ?1)",
            [Utc::now().to_rfc3339()],
        )?;
        self.db.execute(
            "INSERT OR REPLACE INTO metadata(key, value) VALUES ('indexer_version', ?1)",
            [INDEXER_VERSION],
        )?;
        self.db.execute(
            "INSERT OR REPLACE INTO metadata(key, value) VALUES ('active_generation', ?1)",
            [generation.to_string()],
        )?;
        self.db.execute(
            "INSERT OR REPLACE INTO metadata(key, value) VALUES ('semantic_snapshot', ?1)",
            [&semantic.id],
        )?;
        self.db.execute("DELETE FROM input_state", [])?;
        for (path, (kind, hash)) in inputs {
            self.db.execute(
                "INSERT INTO input_state(path, kind, content_hash, revision) VALUES (?1, ?2, ?3, ?4)",
                params![path, kind, hash, revision.workspace_digest],
            )?;
        }
        Ok(())
    }

    fn all_nodes(&self) -> Result<Vec<Node>> {
        let mut statement = self.db.prepare(
            "SELECT id,kind,canonical_name,crate_name,file,start_line,end_line,visibility,content_hash FROM nodes ORDER BY id",
        )?;
        Ok(statement
            .query_map([], node_from_row)?
            .filter_map(Result::ok)
            .collect())
    }

    fn refresh_documents_and_use_cases(&self, nodes: &[Node], revision: &str) -> Result<()> {
        self.db.execute("DELETE FROM documents", [])?;
        self.db.execute("DELETE FROM use_case_nodes", [])?;
        self.db.execute("DELETE FROM use_cases", [])?;
        self.index_documents_and_use_cases(nodes, revision)
    }

    /// Rebuilds the full-text index atomically.
    ///
    /// The body runs `DELETE` followed by one insert per node. Outside a
    /// transaction, a failure part-way through — a busy writer, an I/O error,
    /// a killed process — committed the delete and left the published search
    /// index empty, so every search, locate, and decision lookup silently
    /// returned nothing until the next full reindex.
    fn rebuild_search_index(&self) -> Result<()> {
        self.db.execute_batch("SAVEPOINT rebuild_search_index")?;
        match self.rebuild_search_index_inner() {
            Ok(value) => {
                self.db.execute_batch("RELEASE rebuild_search_index")?;
                Ok(value)
            }
            Err(error) => {
                let _ = self.db.execute_batch(
                    "ROLLBACK TO rebuild_search_index; RELEASE rebuild_search_index",
                );
                Err(error)
            }
        }
    }

    fn rebuild_search_index_inner(&self) -> Result<()> {
        self.db.execute("DELETE FROM search_index", [])?;
        for node in self.all_nodes()? {
            let body = self
                .source_slice(&node, "full-text index", "INDEXES")
                .map(|slice| trim_text(&slice.source, MAX_SEARCH_BODY_BYTES))
                .unwrap_or_default();
            self.db.execute(
                "INSERT INTO search_index(entity_type,entity_id,title,path,body) VALUES ('node',?1,?2,?3,?4)",
                params![node.id.to_string(), node.canonical_name, node.file, body],
            )?;
        }
        {
            let mut statement = self.db.prepare("SELECT id,path,text FROM documents")?;
            let rows = statement.query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?;
            for row in rows {
                let (id, path, text) = row?;
                self.db.execute(
                    "INSERT INTO search_index(entity_type,entity_id,title,path,body) VALUES ('document',?1,?2,?2,?3)",
                    params![id.to_string(), path, trim_text(&text, MAX_SEARCH_BODY_BYTES)],
                )?;
            }
        }
        self.db.execute(
            "INSERT INTO search_index(entity_type,entity_id,title,path,body) SELECT 'decision',id,title,'',rationale || ' ' || applies_to || ' ' || consequences FROM decisions",
            [],
        )?;
        self.db.execute(
            "INSERT INTO search_index(entity_type,entity_id,title,path,body) SELECT 'steering',id,title,scope,instruction || ' ' || status || ' ' || priority FROM steerings",
            [],
        )?;
        self.db.execute(
            "INSERT INTO search_index(entity_type,entity_id,title,path,body) SELECT 'work',id,title,scope_json,evidence_json || ' ' || acceptance_json || ' ' || verification_json FROM work_items",
            [],
        )?;
        self.db.execute(
            "INSERT INTO search_index(entity_type,entity_id,title,path,body) SELECT 'commit',hash,subject,'',author FROM commits",
            [],
        )?;
        quality::append_search_index(&self.db)?;
        Ok(())
    }

    fn search_hits(
        &self,
        query: &[String],
        entity_type: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Value>> {
        if query.iter().all(String::is_empty) {
            return Ok(Vec::new());
        }
        let fts = fts_query(query);
        let mut statement = self.db.prepare(
            "SELECT entity_type,entity_id,title,path,body,bm25(search_index,0.0,0.0,10.0,4.0,1.0) AS score \
             FROM search_index WHERE search_index MATCH ?1 AND (?2 IS NULL OR entity_type=?2) \
             ORDER BY score LIMIT ?3",
        )?;
        Ok(statement
            .query_map(params![fts, entity_type, limit as i64], |row| {
                let body: String = row.get(4)?;
                Ok(json!({
                    "entity_type": row.get::<_, String>(0)?,
                    "entity_id": row.get::<_, String>(1)?,
                    "title": row.get::<_, String>(2)?,
                    "path": row.get::<_, String>(3)?,
                    "evidence": trim_text(&body, 420),
                    "score": row.get::<_, f64>(5)?,
                    "provenance": "SQLiteFTS5",
                }))
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    fn search_contract_artifacts(&self, query: &str, limit: usize) -> Result<Vec<Value>> {
        let query = terms(query);
        if query.iter().all(String::is_empty) {
            return Ok(Vec::new());
        }
        let mut statement = self.db.prepare(
            "SELECT d.kind,d.path,d.text,bm25(search_index,0.0,0.0,10.0,4.0,1.0) AS score \
             FROM search_index JOIN documents d ON search_index.entity_type='document' AND search_index.entity_id=CAST(d.id AS TEXT) \
             WHERE search_index MATCH ?1 AND d.kind='runtime_contract' ORDER BY score LIMIT ?2",
        )?;
        Ok(statement
            .query_map(params![fts_query(&query), limit as i64], |row| {
                Ok(json!({
                    "kind": row.get::<_, String>(0)?,
                    "path": row.get::<_, String>(1)?,
                    "evidence": trim_text(&row.get::<_, String>(2)?, 420),
                    "score": row.get::<_, f64>(3)?,
                    "provenance": "SQLiteFTS5",
                    "verification": "Search evidence only; validate the artifact and its runtime consumer separately.",
                }))
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    fn unresolved_for_targets(&self, targets: &[Node]) -> Result<Vec<Value>> {
        let mut output = Vec::new();
        let mut seen = BTreeSet::new();
        for target in targets {
            let short = short_name(&target.canonical_name);
            let mut statement = self.db.prepare(
                "SELECT u.file,u.line,u.name,u.kind,u.candidate_count,u.reason,n.canonical_name \
                 FROM unresolved_references u JOIN nodes n ON n.id=u.source \
                 WHERE lower(u.name)=lower(?1) OR lower(u.name) LIKE lower(?2) \
                 ORDER BY u.file,u.line LIMIT 40",
            )?;
            let suffix = format!("%::{short}");
            for row in statement.query_map(params![short, suffix], |row| {
                Ok(json!({
                    "file": row.get::<_, String>(0)?,
                    "line": row.get::<_, usize>(1)?,
                    "name": row.get::<_, String>(2)?,
                    "kind": row.get::<_, String>(3)?,
                    "candidate_count": row.get::<_, usize>(4)?,
                    "reason": row.get::<_, String>(5)?,
                    "source": row.get::<_, String>(6)?,
                    "included_in_likely_change_surface": false,
                    "provenance": "Syntax",
                }))
            })? {
                let value = row?;
                let key = format!("{}:{}:{}", value["file"], value["line"], value["name"]);
                if seen.insert(key) {
                    output.push(value);
                }
            }
        }
        Ok(output)
    }

    fn symbol_card(&self, node: &Node) -> Result<SymbolCard> {
        let source = self
            .source_slice(node, "embedding card", "EMBEDS")
            .map(|slice| trim_text(&slice.source, 8_000))
            .unwrap_or_default();
        let relationships = self
            .db
            .prepare(
                "SELECT CASE WHEN edge.src=?1 THEN 'outgoing' ELSE 'incoming' END, \
                        edge.kind, related.canonical_name \
                 FROM edges edge \
                 JOIN nodes related ON related.id=CASE WHEN edge.src=?1 THEN edge.dst ELSE edge.src END \
                 WHERE edge.src=?1 OR edge.dst=?1 \
                 ORDER BY edge.confidence DESC,edge.kind,related.canonical_name LIMIT 48",
            )?
            .query_map([node.id], |row| {
                Ok(format!(
                    "{} {} {}",
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let module = node
            .canonical_name
            .rsplit_once("::")
            .map(|(module, _)| module)
            .unwrap_or("workspace");
        let text = format!(
            "card-version {SYMBOL_CARD_VERSION}\n\
             symbol {}\n\
             module {module}\n\
             kind {}\n\
             visibility {}\n\
             crate {}\n\
             file {}\n\
             relationships {}\n\
             source\n{}",
            node.canonical_name,
            node.kind,
            node.visibility,
            node.crate_name.as_deref().unwrap_or("workspace"),
            node.file,
            relationships.join("\n"),
            source
        );
        let hash = format!("b3:{}", blake3::hash(text.as_bytes()).to_hex());
        Ok(SymbolCard { text, hash })
    }

    fn rebuild_symbol_embeddings(
        &self,
        nodes: &[Node],
        snapshot_id: &str,
    ) -> Result<EmbeddingBuildStats> {
        *self.embedding_cache.borrow_mut() = None;
        let mut stats = EmbeddingBuildStats::default();
        for node in nodes {
            let card = self.symbol_card(node)?;
            let existing: Option<(String, usize, String)> = self
                .db
                .query_row(
                    "SELECT model,dimensions,content_hash FROM symbol_embeddings WHERE node_id=?1",
                    [node.id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?;
            if existing.as_ref().is_some_and(|(model, dimensions, hash)| {
                model == EMBEDDING_MODEL
                    && *dimensions == EMBEDDING_DIMENSIONS
                    && hash == &card.hash
            }) {
                self.db.execute(
                    "UPDATE symbol_embeddings SET semantic_snapshot=?1 WHERE node_id=?2",
                    params![snapshot_id, node.id],
                )?;
                stats.reused += 1;
                continue;
            }
            let vector = embed_text(&card.text);
            self.db.execute(
                "INSERT INTO symbol_embeddings(node_id,model,dimensions,vector,content_hash,semantic_snapshot) \
                 VALUES (?1,?2,?3,?4,?5,?6) \
                 ON CONFLICT(node_id) DO UPDATE SET model=excluded.model,dimensions=excluded.dimensions,vector=excluded.vector,content_hash=excluded.content_hash,semantic_snapshot=excluded.semantic_snapshot",
                params![node.id,EMBEDDING_MODEL,EMBEDDING_DIMENSIONS,encode_vector(&vector),card.hash,snapshot_id],
            )?;
            stats.embedded += 1;
        }
        for (key, value) in [
            ("embedding_model", EMBEDDING_MODEL.to_owned()),
            ("embedding_dimensions", EMBEDDING_DIMENSIONS.to_string()),
            ("embedding_card_version", SYMBOL_CARD_VERSION.to_owned()),
            ("embedding_recomputed", stats.embedded.to_string()),
            ("embedding_reused", stats.reused.to_string()),
            ("embedding_updated_at", Utc::now().to_rfc3339()),
        ] {
            self.db.execute(
                "INSERT OR REPLACE INTO metadata(key,value) VALUES (?1,?2)",
                params![key, value],
            )?;
        }
        Ok(stats)
    }

    fn vector_nodes(&self, query: &str, limit: usize) -> Result<Vec<VectorHit>> {
        let query_vector = embed_text(query);
        let snapshot = self.active_semantic_snapshot_id().unwrap_or_default();
        let reload = self
            .embedding_cache
            .borrow()
            .as_ref()
            .is_none_or(|cache| cache.semantic_snapshot != snapshot);
        if reload {
            let mut statement = self.db.prepare(
                "SELECT n.id,n.kind,n.canonical_name,n.crate_name,n.file,n.start_line,n.end_line,n.visibility,n.content_hash,e.vector,e.content_hash \
                 FROM symbol_embeddings e JOIN nodes n ON n.id=e.node_id \
                 WHERE e.model=?1 AND e.dimensions=?2 AND e.semantic_snapshot=?3",
            )?;
            let mut items = statement
                .query_map(
                    params![EMBEDDING_MODEL, EMBEDDING_DIMENSIONS, snapshot],
                    |row| {
                        let node = node_from_row(row)?;
                        let bytes: Vec<u8> = row.get(9)?;
                        Ok((node, decode_vector(&bytes), row.get(10)?))
                    },
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            items.retain(|(_, vector, _)| vector.len() == EMBEDDING_DIMENSIONS);
            *self.embedding_cache.borrow_mut() = Some(EmbeddingCache {
                semantic_snapshot: snapshot.clone(),
                items,
            });
        }
        let cache = self.embedding_cache.borrow();
        let mut scored = cache
            .as_ref()
            .into_iter()
            .flat_map(|cache| &cache.items)
            .map(|(node, vector, card_hash)| {
                let similarity = dot_product(&query_vector, vector);
                VectorHit {
                    node: node.clone(),
                    similarity,
                    evidence: EmbeddingEvidence {
                        model: EMBEDDING_MODEL,
                        dimensions: EMBEDDING_DIMENSIONS,
                        card_version: SYMBOL_CARD_VERSION,
                        card_hash: card_hash.clone(),
                        semantic_snapshot: snapshot.clone(),
                        similarity,
                    },
                }
            })
            .filter(|hit| hit.similarity > 0.0)
            .collect::<Vec<_>>();
        scored.sort_by(|left, right| {
            right
                .similarity
                .total_cmp(&left.similarity)
                .then_with(|| left.node.canonical_name.cmp(&right.node.canonical_name))
        });
        scored.truncate(limit);
        Ok(scored)
    }

    fn graph_neighbors(
        &self,
        ids: &[i64],
        incoming: bool,
        limit: usize,
    ) -> Result<Vec<(Node, String, f64)>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = std::iter::repeat_n("?", ids.len())
            .collect::<Vec<_>>()
            .join(",");
        let (join, filter) = if incoming {
            ("edge.src=node.id", format!("edge.dst IN ({placeholders})"))
        } else {
            ("edge.dst=node.id", format!("edge.src IN ({placeholders})"))
        };
        let sql = format!(
            "SELECT DISTINCT node.id,node.kind,node.canonical_name,node.crate_name,node.file,node.start_line,node.end_line,node.visibility,node.content_hash,edge.kind,edge.confidence FROM edges edge JOIN nodes node ON {join} WHERE {filter} ORDER BY edge.confidence DESC LIMIT {limit}"
        );
        let mut statement = self.db.prepare(&sql)?;
        Ok(statement
            .query_map(rusqlite::params_from_iter(ids), |row| {
                Ok((node_from_row(row)?, row.get(9)?, row.get(10)?))
            })?
            .filter_map(Result::ok)
            .collect())
    }

    fn hybrid_nodes(&self, query: &str, limit: usize) -> Result<Vec<HybridHit>> {
        let lexical = self.search_nodes(&terms(query), limit.saturating_mul(3).max(12))?;
        let vectors = self.vector_nodes(query, limit.saturating_mul(3).max(12))?;
        let mut ranked: HashMap<i64, HybridHit> = HashMap::new();
        for (rank, node) in lexical.iter().enumerate() {
            let id = node.id;
            let exact = is_exact_identifier_match(query, node);
            add_rrf_hit(&mut ranked, node.clone(), "lexical", rank, 1.0);
            if exact {
                let hit = ranked.get_mut(&id).expect("lexical hit was inserted");
                hit.exact_match = true;
                hit.channels.insert("exact_identifier".to_owned());
            }
        }
        for (rank, vector) in vectors.iter().enumerate() {
            let id = vector.node.id;
            add_rrf_hit(
                &mut ranked,
                vector.node.clone(),
                "embedding",
                rank,
                0.75 * vector.similarity.max(0.15),
            );
            ranked
                .get_mut(&id)
                .expect("vector hit was inserted")
                .embedding = Some(vector.evidence.clone());
        }
        let mut seed_hits = ranked.values().collect::<Vec<_>>();
        seed_hits.sort_by(|left, right| {
            right
                .exact_match
                .cmp(&left.exact_match)
                .then_with(|| right.score.total_cmp(&left.score))
                .then_with(|| left.node.canonical_name.cmp(&right.node.canonical_name))
        });
        let seed_ids = seed_hits
            .into_iter()
            .map(|hit| hit.node.id)
            .take(10)
            .collect::<Vec<_>>();
        let lower = query.to_ascii_lowercase();
        let wants_incoming = [
            "break",
            "impact",
            "change",
            "caller",
            "used by",
            "reference",
        ]
        .iter()
        .any(|term| lower.contains(term));
        let wants_outgoing = ["how", "work", "depend", "call", "implement"]
            .iter()
            .any(|term| lower.contains(term));
        for incoming in match (wants_incoming, wants_outgoing) {
            (true, false) => vec![true],
            (false, true) => vec![false],
            _ => vec![true, false],
        } {
            for (rank, (node, edge, confidence)) in self
                .graph_neighbors(&seed_ids, incoming, limit.saturating_mul(2))?
                .into_iter()
                .enumerate()
            {
                add_rrf_hit(
                    &mut ranked,
                    node,
                    &format!("graph:{edge}"),
                    rank,
                    0.6 * confidence,
                );
            }
        }
        let mut output = ranked.into_values().collect::<Vec<_>>();
        output.sort_by(|left, right| {
            right
                .exact_match
                .cmp(&left.exact_match)
                .then_with(|| right.score.total_cmp(&left.score))
                .then_with(|| left.node.canonical_name.cmp(&right.node.canonical_name))
        });
        output.truncate(limit);
        Ok(output)
    }

    pub fn context_pack(&self, query: &str, budget: usize, limit: usize) -> Result<Value> {
        let token_budget = budget.clamp(250, 20_000);
        let hits = self.hybrid_nodes(query, limit.clamp(1, 100))?;
        let max_bytes = token_budget.saturating_mul(4);
        let source_budget = max_bytes.saturating_mul(3) / 4;
        let mut used = 0usize;
        let mut source_slices = Vec::new();
        for hit in &hits {
            let channels = hit.channels.iter().cloned().collect::<Vec<_>>().join(",");
            let mut slice = self.source_slice(&hit.node, "hybrid retrieval", &channels)?;
            let remaining = source_budget.saturating_sub(used);
            if remaining == 0 {
                break;
            }
            if slice.source.len() > remaining {
                slice.source = trim_text(&slice.source, remaining);
            }
            used += slice.source.len();
            source_slices.push(slice);
        }
        let ranked_symbols = hits
            .iter()
            .map(|hit| {
                json!({
                    "symbol":hit.node.canonical_name,
                    "file":hit.node.file,
                    "kind":hit.node.kind,
                    "score":hit.score,
                    "channels":hit.channels,
                    "exact_match":hit.exact_match,
                    "embedding_evidence":hit.embedding,
                })
            })
            .collect::<Vec<_>>();
        let documentation = budget_values(
            self.search_hits(&terms(query), Some("document"), 3)?,
            &mut used,
            max_bytes,
        );
        let decisions = budget_values(self.decisions_for(query)?, &mut used, max_bytes);
        let steerings = budget_values(
            self.steering_list(Some(query), 12)?["steerings"]
                .as_array()
                .cloned()
                .unwrap_or_default(),
            &mut used,
            max_bytes,
        );
        let work_items = budget_values(
            self.work_list(Some(query), 12)?["items"]
                .as_array()
                .cloned()
                .unwrap_or_default(),
            &mut used,
            max_bytes,
        );
        Ok(json!({
            "query":query,
            "retrieval":"BM25 + subword embedding + typed graph expansion, fused with reciprocal-rank fusion",
            "retrieval_provenance":self.retrieval_provenance(),
            "ranked_symbols":ranked_symbols,
            "source_slices":source_slices,
            "documentation":documentation,
            "decisions":decisions,
            "steerings":steerings,
            "work_items":work_items,
            "context_budget":{"tokens":token_budget,"estimated_tokens":used.div_ceil(4)},
            "generation":self.active_generation(),
            "semantic_snapshot":self.semantic_snapshot_resource(),
            "blind_spots":["Generated code, runtime registration, external consumers, and inactive feature/target profiles still require profile-specific confirmation."]
        }))
    }

    fn refresh_git(&self, revision: &str) -> Result<()> {
        self.db.execute("DELETE FROM commits", [])?;
        self.db.execute("DELETE FROM commit_files", [])?;
        self.db.execute("DELETE FROM co_changes", [])?;
        self.index_git(revision)
    }

    fn relink_decision_targets(&self) -> Result<()> {
        let targets: Vec<(String, String)> = self
            .db
            .prepare("SELECT decision_id, target_ref FROM decision_targets")?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for (decision_id, target_ref) in targets {
            let node_id = self
                .search_nodes(&terms(&target_ref), 1)?
                .into_iter()
                .next()
                .map(|node| node.id);
            self.db.execute(
                "UPDATE decision_targets SET node_id=?1 WHERE decision_id=?2 AND target_ref=?3",
                params![node_id, decision_id, target_ref],
            )?;
        }
        Ok(())
    }

    fn index_source_files(
        &self,
        paths: &[PathBuf],
        packages: &[(String, String, String)],
    ) -> Result<Vec<Node>> {
        let mut nodes = Vec::new();
        let mut unreadable = Vec::new();
        for path in paths {
            let relative = relative(&self.root, path);
            // An unreadable or non-UTF-8 file is recorded and skipped. Reading
            // it as an empty string silently removed every one of its symbols
            // while `index_inputs` still hashed it as healthy and current.
            let Some(text) = read_source_text(path) else {
                unreadable.push(relative);
                continue;
            };
            if let Some(ra) = &self.ra
                && let Ok(mut ra) = ra.lock()
            {
                ra.did_change(&format!("file://{}", path.display()), &text);
            }
            let lines: Vec<_> = text.lines().collect();
            let crate_name = crate_for_file(packages, path);
            let parsed = parse_rust_symbols(&text).unwrap_or_else(|| regex_symbols(&lines));
            for symbol in parsed {
                let start = symbol.start_line.max(1).min(lines.len().max(1));
                let end = symbol.end_line.max(start).min(lines.len());
                let mut canonical_parts = vec![relative.trim_end_matches(".rs").replace('/', "::")];
                canonical_parts.extend(symbol.scope);
                canonical_parts.push(symbol.name);
                let canonical_name = canonical_parts.join("::");
                let hash = format!(
                    "b3:{}",
                    blake3::hash(lines[start.saturating_sub(1)..end].join("\n").as_bytes())
                        .to_hex()
                );
                self.db.execute("INSERT INTO nodes(kind, canonical_name, crate_name, file, start_line, end_line, visibility, content_hash, metadata) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)", params![symbol.kind, canonical_name, crate_name, relative, start, end, symbol.visibility, hash, json!({"parser":symbol.parser}).to_string()])?;
                let id = self.db.last_insert_rowid();
                nodes.push(Node {
                    id,
                    kind: symbol.kind,
                    canonical_name,
                    crate_name: crate_name.clone(),
                    file: relative.clone(),
                    start_line: start,
                    end_line: end,
                    visibility: symbol.visibility,
                    content_hash: hash,
                });
            }
        }
        // Surfaced by `status()` as a degraded area rather than being silent.
        self.db.execute(
            "INSERT OR REPLACE INTO metadata(key, value) VALUES ('unreadable_inputs', ?1)",
            [serde_json::to_string(&unreadable)?],
        )?;
        Ok(nodes)
    }

    fn index_static_references(&self, nodes: &[Node], revision: &str) -> Result<()> {
        let mut targets_by_name: HashMap<&str, Vec<&Node>> = HashMap::new();
        for node in nodes {
            targets_by_name
                .entry(short_name(&node.canonical_name))
                .or_default()
                .push(node);
        }
        self.db.execute("DELETE FROM unresolved_references", [])?;
        for file in rust_files(&self.root) {
            let Some(text) = read_source_text(&file) else {
                continue;
            };
            let rel = relative(&self.root, &file);
            let file_nodes: Vec<&Node> = nodes.iter().filter(|node| node.file == rel).collect();
            let mut edges = BTreeSet::new();
            let Some(references) = syntax_references(&text) else {
                continue;
            };
            for reference in references {
                let line_number = reference.line;
                let source = file_nodes
                    .iter()
                    .copied()
                    .filter(|node| node.start_line <= line_number && line_number <= node.end_line)
                    .min_by_key(|node| node.end_line.saturating_sub(node.start_line));
                let Some(source) = source else { continue };
                let Some(name) = reference.path.last() else {
                    continue;
                };
                let Some(targets) = targets_by_name.get(name.as_str()) else {
                    continue;
                };
                if let Some((target, confidence, resolution)) =
                    resolve_syntax_reference(source, &reference.path, targets)
                {
                    if source.id != target.id {
                        let confidence = if reference.kind == "MAY_CALL_DYNAMIC" {
                            confidence.min(0.55)
                        } else {
                            confidence
                        };
                        edges.insert((
                            source.id,
                            target.id,
                            line_number,
                            reference.kind,
                            confidence.to_bits(),
                            resolution,
                        ));
                    }
                } else if targets.iter().any(|target| target.id != source.id) {
                    self.db.execute(
                        "INSERT INTO unresolved_references(source,file,line,name,kind,candidate_count,reason,revision) VALUES (?1,?2,?3,?4,?5,?6,'ambiguous static name; omitted from impact graph',?7)",
                        params![source.id,rel,line_number,reference.path.join("::"),reference.kind,targets.iter().filter(|target| target.id != source.id).count(),revision],
                    )?;
                }
            }
            for (source, target, line, kind, confidence, resolution) in edges {
                // One relationship per (src, dst, kind, provenance). The same
                // call appearing on several lines used to insert a duplicate row
                // each time, and the table had no uniqueness constraint to stop
                // it. On a repeat, the strongest evidence wins; exact call sites
                // remain the job of `repo.search mode=exact`.
                self.db.execute(
                    "INSERT INTO edges(src, dst, kind, context_json, confidence, provenance, revision, metadata) VALUES (?1, ?2, ?3, '{}', ?4, 'Syntax', ?5, ?6) \
                     ON CONFLICT(src,dst,kind,provenance) DO UPDATE SET \
                       confidence=MAX(confidence,excluded.confidence), \
                       revision=excluded.revision, \
                       metadata=CASE WHEN excluded.confidence>confidence THEN excluded.metadata ELSE metadata END",
                    params![source, target, kind, f64::from_bits(confidence), revision, json!({"file":rel,"line":line,"resolution":resolution}).to_string()],
                )?;
            }
        }
        Ok(())
    }

    fn index_structural_edges(&self, nodes: &[Node], revision: &str) -> Result<()> {
        for child in nodes {
            if let Some(parent) = nodes
                .iter()
                .filter(|parent| {
                    parent.id != child.id
                        && parent.file == child.file
                        && parent.start_line <= child.start_line
                        && parent.end_line >= child.end_line
                })
                .min_by_key(|parent| parent.end_line.saturating_sub(parent.start_line))
            {
                self.insert_edge(EdgeRecord {
                    source: parent.id,
                    target: child.id,
                    kind: "CONTAINS",
                    confidence: 0.95,
                    provenance: "Syntax",
                    revision,
                    metadata: json!({"file": child.file}),
                })?;
            }
        }
        let impl_pattern = Regex::new(
            r"(?m)^\s*impl(?:\s*<[^>{}]*>)?\s+([A-Za-z_][A-Za-z0-9_:]*)\s+for\s+([A-Za-z_][A-Za-z0-9_:]*)",
        )?;
        let use_pattern = Regex::new(r"(?m)^\s*(?:pub\s+)?use\s+([^;]+);")?;
        for file in rust_files(&self.root) {
            let Some(text) = read_source_text(&file) else {
                continue;
            };
            let rel = relative(&self.root, &file);
            for capture in impl_pattern.captures_iter(&text) {
                let trait_name = capture.get(1).map(|value| short_name(value.as_str()));
                let type_name = capture.get(2).map(|value| short_name(value.as_str()));
                let (Some(trait_name), Some(type_name)) = (trait_name, type_name) else {
                    continue;
                };
                let source = nodes.iter().find(|node| {
                    short_name(&node.canonical_name) == type_name
                        && matches!(node.kind.as_str(), "struct" | "enum" | "type")
                });
                let target = nodes.iter().find(|node| {
                    short_name(&node.canonical_name) == trait_name && node.kind == "trait"
                });
                if let (Some(source), Some(target)) = (source, target) {
                    self.insert_edge(EdgeRecord {
                        source: source.id,
                        target: target.id,
                        kind: "IMPLEMENTS",
                        confidence: 0.85,
                        provenance: "Syntax",
                        revision,
                        metadata: json!({"file":rel}),
                    })?;
                }
            }
            for capture in use_pattern.captures_iter(&text) {
                let Some(path) = capture.get(1).map(|value| value.as_str()) else {
                    continue;
                };
                let imported = path
                    .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
                    .rfind(|part| !part.is_empty() && *part != "self");
                let Some(imported) = imported else { continue };
                let target = nodes
                    .iter()
                    .find(|node| short_name(&node.canonical_name) == imported);
                let source = nodes
                    .iter()
                    .filter(|node| node.file == rel)
                    .min_by_key(|node| node.start_line);
                if let (Some(source), Some(target)) = (source, target)
                    && source.id != target.id
                {
                    self.insert_edge(EdgeRecord {
                        source: source.id,
                        target: target.id,
                        kind: "IMPORTS",
                        confidence: 0.75,
                        provenance: "Syntax",
                        revision,
                        metadata: json!({"file":rel,"path":path}),
                    })?;
                }
            }
        }
        Ok(())
    }

    fn insert_edge(&self, edge: EdgeRecord<'_>) -> Result<()> {
        self.db.execute(
            "INSERT OR IGNORE INTO edges(src,dst,kind,context_json,confidence,provenance,revision,metadata) VALUES (?1,?2,?3,'{}',?4,?5,?6,?7)",
            params![edge.source,edge.target,edge.kind,edge.confidence,edge.provenance,edge.revision,edge.metadata.to_string()],
        )?;
        Ok(())
    }

    fn index_cargo_matrix(&self, metadata: &Value, revision: &str) -> Result<()> {
        let Some(packages) = metadata.get("packages").and_then(Value::as_array) else {
            return Ok(());
        };
        let workspace_members = workspace_member_ids(metadata);
        for package in packages.iter().filter(|package| {
            package
                .get("id")
                .and_then(Value::as_str)
                .is_some_and(|id| workspace_members.contains(id))
        }) {
            let Some(name) = package.get("name").and_then(Value::as_str) else {
                continue;
            };
            let edition = package.get("edition").and_then(Value::as_str).unwrap_or("");
            if let Some(targets) = package.get("targets").and_then(Value::as_array) {
                for target in targets {
                    self.db.execute(
                        "INSERT OR REPLACE INTO package_targets(package,target_name,kind,crate_types,required_features,edition,doc,doctest,test,bench,revision) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
                        params![name,
                            target.get("name").and_then(Value::as_str).unwrap_or(""),
                            target.get("kind").cloned().unwrap_or_else(|| json!([])).to_string(),
                            target.get("crate_types").cloned().unwrap_or_else(|| json!([])).to_string(),
                            target.get("required-features").cloned().unwrap_or_else(|| json!([])).to_string(),
                            edition,
                            target.get("doc").and_then(Value::as_bool).unwrap_or(false),
                            target.get("doctest").and_then(Value::as_bool).unwrap_or(false),
                            target.get("test").and_then(Value::as_bool).unwrap_or(false),
                            target.get("bench").and_then(Value::as_bool).unwrap_or(false),
                            revision],
                    )?;
                }
            }
            if let Some(features) = package.get("features").and_then(Value::as_object) {
                for (feature, dependencies) in features {
                    self.db.execute(
                        "INSERT OR REPLACE INTO package_features(package,feature,dependencies,revision) VALUES (?1,?2,?3,?4)",
                        params![name, feature, dependencies.to_string(), revision],
                    )?;
                }
            }
        }
        Ok(())
    }

    /// Index only explicit lifecycle statements. Naming conventions create a
    /// low-confidence *candidate*, never a claim that deletion is safe.
    fn index_lifecycle_evidence(&self, nodes: &[Node], revision: &str) -> Result<()> {
        self.db.execute("DELETE FROM lifecycle_edges", [])?;
        let explicit = Regex::new(
            r"(?i)(?:deprecated|legacy|compat(?:ibility)?|fallback|superseded|replaced)\D{0,80}(?:use|with|by|replace(?:d)?\s+(?:with\s+)?)\s*`?([A-Za-z_][A-Za-z0-9_]*)`?",
        )?;
        // Anchored on word boundaries and matched against the symbol name only.
        // The previous unanchored pattern also scanned a ±28-line excerpt, so
        // any constant near the string "symbol-card-v1" and any function near
        // the word "placeholders" was reported as suspected legacy — 59 of them
        // in this repository alone. `v1` and `old` are dropped entirely: they
        // carry almost no signal and produced most of the noise.
        let lifecycle_name = Regex::new(
            r"(?i)\b(legacy|deprecated|obsolete|superseded|compat|compatibility|fallback|shim)\b",
        )?;
        // An excerpt only contributes when it carries an explicit marker.
        let explicit_marker = Regex::new(r"#\[deprecated|#\[allow\(deprecated\)\]")?;
        for node in nodes {
            let Some(text) = read_source_text(&self.root.join(&node.file)) else {
                continue;
            };
            let start = node.start_line.saturating_sub(5);
            let excerpt = text
                .lines()
                .skip(start)
                .take(28)
                .collect::<Vec<_>>()
                .join("\n");
            let replacement = explicit
                .captures(&excerpt)
                .and_then(|capture| capture.get(1))
                .map(|capture| capture.as_str())
                .and_then(|name| {
                    nodes
                        .iter()
                        .find(|other| short_name(&other.canonical_name) == name)
                });
            let inferred_lifecycle = lifecycle_name.is_match(short_name(&node.canonical_name))
                || explicit_marker.is_match(&excerpt);
            if replacement.is_none() && !inferred_lifecycle {
                continue;
            }
            let (canonical_node, kind, confidence, provenance) = match replacement {
                Some(target) => (Some(target.id), "supersedes", 0.95, "SourceDoc"),
                None => (None, "suspected_legacy", 0.45, "Heuristic"),
            };
            self.db.execute(
                "INSERT INTO lifecycle_edges(legacy_node, canonical_node, kind, evidence, provenance, confidence, revision) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![node.id, canonical_node, kind, excerpt, provenance, confidence, revision],
            )?;
        }
        Ok(())
    }

    fn index_proposed_work(&self, revision: &str) -> Result<()> {
        let marker = Regex::new(r"(?i)\b(TODO|FIXME|XXX)\b\s*[:\-]?\s*(.+)")?;
        let indexed_at = Utc::now().to_rfc3339();
        // Preserve the item for architectural history, but make disappearing
        // automatic evidence visibly stale instead of silently trusting it.
        self.db.execute(
            "UPDATE work_items SET evidence_json=?1, confidence=0.20, last_validated_snapshot=?2, updated_at=?3 \
             WHERE provenance='SourceDoc' AND discovered_from='TODO/FIXME scanner' AND status='proposed'",
            params![json!(["Source marker absent at the current snapshot; review or remove this proposed item."]).to_string(), revision, indexed_at],
        )?;
        for path in all_text_files(&self.root) {
            let relative_path = relative(&self.root, &path);
            let Some(text) = read_source_text(&path) else {
                continue;
            };
            for (offset, line) in text.lines().enumerate() {
                let Some(actionable_text) = actionable_marker_text(&path, line) else {
                    continue;
                };
                let Some(capture) = marker.captures(actionable_text) else {
                    continue;
                };
                let title = capture
                    .get(2)
                    .map(|value| value.as_str().trim())
                    .unwrap_or("Repository follow-up");
                let id = format!(
                    "work_{}",
                    &blake3::hash(format!("{relative_path}:{offset}:{title}").as_bytes()).to_hex()
                        [..12]
                );
                self.db.execute(
                    "INSERT INTO work_items(id,title,status,priority,kind,scope_json,evidence_json,depends_json,blocked_json,acceptance_json,verification_json,discovered_from,provenance,confidence,last_validated_snapshot,created_at,updated_at) \
                     VALUES (?1,?2,'proposed','normal','documentation',?3,?4,'[]','[]','[]','[]',?5,'SourceDoc',0.70,?6,?7,?7) \
                     ON CONFLICT(id) DO UPDATE SET evidence_json=excluded.evidence_json,confidence=excluded.confidence,last_validated_snapshot=excluded.last_validated_snapshot,updated_at=excluded.updated_at",
                    params![id, title, json!([relative_path]).to_string(), json!([format!("{relative_path}:{}: {}", offset + 1, line.trim())]).to_string(), "TODO/FIXME scanner", revision, indexed_at],
                )?;
            }
        }
        Ok(())
    }

    fn index_git(&self, revision: &str) -> Result<()> {
        let Some(log) = git_output(
            &self.root,
            &[
                "log",
                "--format=%H%x1f%aI%x1f%an%x1f%s",
                "--name-only",
                "-z",
                "-n",
                "250",
            ],
        ) else {
            return Ok(());
        };
        let mut files_by_commit: Vec<Vec<String>> = Vec::new();
        let mut current_hash: Option<String> = None;
        let mut current_files = BTreeSet::new();
        for record in log.split(|byte| *byte == 0) {
            let value = String::from_utf8_lossy(record);
            let value = value.trim_matches(['\n', '\r']);
            if value.is_empty() {
                continue;
            }
            let bits: Vec<_> = value.split('\x1f').collect();
            if bits.len() == 4 {
                if current_hash.is_some() {
                    files_by_commit.push(current_files.iter().cloned().collect());
                    current_files.clear();
                }
                current_hash = Some(bits[0].to_owned());
                self.db.execute("INSERT OR REPLACE INTO commits(hash, timestamp, author, subject, revision) VALUES (?1, ?2, ?3, ?4, ?5)", params![bits[0], bits[1], bits[2], bits[3], revision])?;
                continue;
            }
            let Some(hash) = current_hash.as_deref() else {
                continue;
            };
            for file in value.lines().map(str::trim).filter(|line| !line.is_empty()) {
                current_files.insert(file.to_owned());
                self.db.execute(
                    "INSERT OR REPLACE INTO commit_files(commit_hash, file) VALUES (?1, ?2)",
                    params![hash, file],
                )?;
            }
        }
        if current_hash.is_some() {
            files_by_commit.push(current_files.iter().cloned().collect());
        }
        let mut pairs: HashMap<(String, String), i64> = HashMap::new();
        let mut file_frequency: HashMap<String, i64> = HashMap::new();
        for files in &files_by_commit {
            for file in files {
                *file_frequency.entry(file.clone()).or_default() += 1;
            }
            for (i, a) in files.iter().enumerate() {
                for b in files.iter().skip(i + 1) {
                    let key = if a < b {
                        (a.clone(), b.clone())
                    } else {
                        (b.clone(), a.clone())
                    };
                    *pairs.entry(key).or_default() += 1;
                }
            }
        }
        for ((a, b), count) in pairs {
            let denominator = ((file_frequency[&a] * file_frequency[&b]) as f64).sqrt();
            let score = if denominator == 0.0 {
                0.0
            } else {
                count as f64 / denominator
            };
            self.db.execute("INSERT OR REPLACE INTO co_changes(node_a, node_b, count, score) VALUES (?1, ?2, ?3, ?4)", params![a, b, count, score])?;
        }
        Ok(())
    }

    fn index_documents_and_use_cases(&self, nodes: &[Node], revision: &str) -> Result<()> {
        for path in all_text_files(&self.root) {
            let Some(input_kind) = input_kind(&path) else {
                continue;
            };
            if input_kind == "source" {
                continue;
            }
            let kind = match input_kind {
                "contract" => "runtime_contract",
                "configuration" | "cargo" => "configuration",
                _ => "source_doc",
            };
            let Some(text) = read_source_text(&path) else {
                continue;
            };
            // Documents are stored whole; cap them so one checked-in dump
            // cannot put an unbounded blob into the index.
            let text = trim_text(&text, MAX_DOCUMENT_BYTES);
            self.db.execute(
                "INSERT INTO documents(kind, path, text, revision) VALUES (?1, ?2, ?3, ?4)",
                params![kind, relative(&self.root, &path), text, revision],
            )?;
        }
        for node in nodes.iter().filter(|node| {
            node.file.contains("test") || short_name(&node.canonical_name).starts_with("test_")
        }) {
            let name = short_name(&node.canonical_name).replace('_', " ");
            let id = format!("usecase:{}", slug(&format!("{}-{}", node.file, name)));
            self.db.execute(
                "INSERT OR REPLACE INTO use_cases(id, name, description, provenance, confidence, revision) VALUES (?1, ?2, ?3, 'AgentInference', 0.60, ?4)",
                params![id, name, format!("Behavior exercised by {}", node.canonical_name), revision],
            )?;
            self.db.execute(
                "INSERT OR REPLACE INTO use_case_nodes(use_case_id, node_id, relationship, provenance, confidence, revision) VALUES (?1, ?2, 'TESTS', 'AgentInference', 0.60, ?3)",
                params![id, node.id, revision],
            )?;
        }
        Ok(())
    }

    pub fn orient(&self, intent: &str) -> Result<Value> {
        let automatic_problem_capture = self.automatic_problem_capture(intent, "repo.orient")?;
        let terms = terms(intent);
        let nodes = self.search_nodes(&terms, 12)?;
        let crates: Vec<String> = self
            .db
            .prepare("SELECT name FROM packages ORDER BY name")?
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let dependency_count: i64 =
            self.db
                .query_row("SELECT COUNT(*) FROM package_dependencies", [], |row| {
                    row.get(0)
                })?;
        Ok(
            json!({"intent":intent,"revision":self.revision(),"automatic_problem_capture":automatic_problem_capture,"architecture":{"crates":crates,"resolved_cargo_dependency_edges":dependency_count,"likely_symbols":nodes,"index_status":self.index_status()},"next":"Call repo.prepare_change before modifying source."}),
        )
    }

    pub fn prepare_change(
        &self,
        intent: &str,
        targets: &[String],
        depth: usize,
        budget: Option<usize>,
    ) -> Result<Value> {
        let automatic_problem_capture = self.automatic_problem_capture(intent, "change.prepare")?;
        let context_id = format!(
            "ctx_{}",
            &blake3::hash(format!("{}:{:?}:{}", intent, targets, Utc::now()).as_bytes()).to_hex()
                [..12]
        );
        let mut target_nodes = Vec::new();
        for target in targets {
            if target.contains("::")
                && let Some(exact) = self.node_by_canonical_name(target)?
            {
                target_nodes.push(exact);
                continue;
            }
            let candidates = self.search_nodes(&terms(target), 8)?;
            target_nodes.extend(candidates);
        }
        if target_nodes.is_empty() {
            target_nodes = self
                .hybrid_nodes(intent, 8)?
                .into_iter()
                .map(|hit| hit.node)
                .collect();
        }
        dedupe_nodes(&mut target_nodes);
        let ids: Vec<i64> = target_nodes.iter().map(|n| n.id).collect();
        let references = self.references_for(&ids, depth)?;
        let semantic_references = self.semantic_references(&target_nodes);
        let tests = self.test_nodes(&target_nodes)?;
        let use_cases = self.use_cases_for(&tests)?;
        let decisions = self.decisions_for(&targets.join(" "))?;
        let steerings = self.steering_list(Some(&targets.join(" ")), 12)?["steerings"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(steering_is_active)
            .collect::<Vec<_>>();
        let lifecycle = self.lifecycle_for(&target_nodes)?;
        let uncertain_static_references = self.unresolved_for_targets(&target_nodes)?;
        let runtime_contracts = self.search_contract_artifacts(intent, 8)?;
        let obsolete = self.obsolete_candidates(Some(&targets.join(" ")), 12)?;
        let work = self.work_list(Some(&targets.join(" ")), 12)?;
        let likely_surface = files_for_nodes(&target_nodes, &references);
        let validation_queue = self.activate_quality_constraints(
            &context_id,
            intent,
            targets,
            &target_nodes,
            &references,
        )?;
        let token_budget = budget.unwrap_or(3_000).clamp(250, 10_000);
        let max = token_budget.saturating_mul(4);
        let mut slices = Vec::new();
        let mut used = 0;
        let mut seen_slices = BTreeSet::new();
        for (node, reason, relation) in target_nodes
            .iter()
            .map(|n| (n, "target", "DEFINES"))
            .chain(references.iter().map(|n| (n, "reference", "REFERENCES")))
            .chain(tests.iter().map(|n| (n, "behavioral test", "TESTS")))
        {
            if !seen_slices.insert(node.id) {
                continue;
            }
            let mut slice = self.source_slice(node, reason, relation)?;
            let remaining = max.saturating_sub(used);
            if remaining == 0 {
                break;
            }
            if slice.source.len() > remaining {
                slice.source = trim_text(&slice.source, remaining);
            }
            used += slice.source.len();
            slices.push(slice);
        }
        let payload = json!({"context_id":context_id,"intent":intent,"revision":self.revision(),"snapshot":self.snapshot(),"automatic_problem_capture":automatic_problem_capture,"primary_symbols":target_nodes,"references":references,"reference_provenance":{"semantic":"RustAnalyzer (confidence 1.0), cached by semantic snapshot","fallback":"Syntax-derived, ambiguity-suppressed relationships (confidence labelled)"},"semantic_references":semantic_references,"uncertain_static_references":uncertain_static_references,"runtime_contracts":runtime_contracts,"tests":tests,"use_cases":use_cases,"decisions":decisions,"steerings":steerings,"lifecycle":lifecycle,"obsolete_candidates":obsolete.get("candidates"),"work_items":work.get("items"),"likely_change_surface":likely_surface,"source_slices":slices,"context_budget":{"tokens":token_budget,"estimated_tokens":used.div_ceil(4)},"generation":self.active_generation(),"semantic_snapshot":self.semantic_snapshot_resource(),"risk":risk(&target_nodes, &references),"validation_queue":validation_queue,"unresolved_edges":["Ambiguous static references are reported separately and excluded from likely_change_surface.","Runtime registration, generated code, inactive feature/target profiles, external consumers, and deployment state require profile-specific or runtime verification."],"verification_plan":["cargo check --all-targets","cargo test","Use repo.matrix for no-default, individual-feature, and all-feature checks.","Validate changed GTK UI/Blueprint files with the project GTK tooling.","Compare changed D-Bus XML with runtime introspection and external consumer expectations."],"semantic_note":"rust-analyzer facts are resolved on demand and persisted for the active semantic snapshot. Syntax relationships are AST-derived; unresolved ambiguous names are retained as uncertainty, not impact edges."});
        self.db.execute("INSERT INTO change_contexts(id, intent, payload, revision, created_at) VALUES (?1, ?2, ?3, ?4, ?5)", params![context_id, intent, payload.to_string(), self.revision().workspace_digest, Utc::now().to_rfc3339()])?;
        Ok(payload)
    }

    pub fn expand_context(&self, id: &str, around: &str, relation: &str) -> Result<Value> {
        let context: String = self
            .db
            .query_row(
                "SELECT payload FROM change_contexts WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .optional()?
            .context("unknown change context")?;
        let nodes = self.search_nodes(&terms(around), 20)?;
        if relation == "implementations" {
            let mut slices = self.semantic_locations(&nodes, "implementations");
            if slices.is_empty() {
                slices = nodes
                    .iter()
                    .take(8)
                    .map(|node| {
                        self.source_slice(node, "static implementation candidate", relation)
                    })
                    .collect::<Result<Vec<_>>>()?;
            }
            return Ok(
                json!({"context_id":id,"around":around,"relation":relation,"parent_context":serde_json::from_str::<Value>(&context)?,"source_slices":slices,"revision":self.revision()}),
            );
        }
        let related = match relation {
            "callers" | "references" => {
                self.references_for(&nodes.iter().map(|n| n.id).collect::<Vec<_>>(), 2)?
            }
            _ => nodes.clone(),
        };
        let slices: Result<Vec<_>> = related
            .iter()
            .take(12)
            .map(|n| self.source_slice(n, "expanded context", relation))
            .collect();
        Ok(
            json!({"context_id":id,"around":around,"relation":relation,"parent_context":serde_json::from_str::<Value>(&context)?,"source_slices":slices?,"revision":self.revision()}),
        )
    }

    /// Answers "who calls this", "who implements this", and "where is this
    /// defined" without first preparing a change.
    ///
    /// `expand_context` already implemented these relations but required a
    /// prepared change context, so the refactor and trait-implementor workflows
    /// had no entry point at all and dropped straight to `grep`.
    pub fn symbol_relations(&self, symbol: &str, relation: &str, limit: usize) -> Result<Value> {
        let limit = limit.clamp(1, 100);
        // Validate the relation before searching, so a misspelled relation is
        // reported even when the symbol itself is unknown.
        ensure!(
            matches!(
                relation,
                "definition" | "callers" | "references" | "implementations"
            ),
            "unsupported relation `{relation}`; expected definition, callers, references, or implementations"
        );
        let nodes = self.search_nodes(&terms(symbol), limit.min(20))?;
        if nodes.is_empty() {
            return Ok(json!({
                "symbol": symbol,
                "relation": relation,
                "definitions": [],
                "results": [],
                "freshness": self.snapshot(),
                "note": "No indexed symbol matched. The published snapshot may be stale; use repo.search mode=exact for live text, or index.refresh to republish."
            }));
        }
        let definitions = nodes
            .iter()
            .take(limit.min(8))
            .map(node_location)
            .collect::<Vec<_>>();
        let (results, channel) = match relation {
            "definition" => (definitions.clone(), "indexed definition"),
            "implementations" => {
                let semantic = self.semantic_locations(&nodes, "implementations");
                if semantic.is_empty() {
                    let ids = nodes.iter().map(|node| node.id).collect::<Vec<_>>();
                    let neighbors = self.graph_neighbors(&ids, true, limit)?;
                    (
                        neighbors
                            .iter()
                            .filter(|(_, kind, _)| kind == "IMPLEMENTS")
                            .map(|(node, kind, confidence)| {
                                let mut located = node_location(node);
                                located["edge"] = json!(kind);
                                located["confidence"] = json!(confidence);
                                located
                            })
                            .collect::<Vec<_>>(),
                        "syntax-derived IMPLEMENTS edges",
                    )
                } else {
                    (
                        semantic
                            .iter()
                            .map(serde_json::to_value)
                            .collect::<serde_json::Result<Vec<_>>>()?,
                        "rust-analyzer",
                    )
                }
            }
            "callers" | "references" => {
                let ids = nodes.iter().map(|node| node.id).collect::<Vec<_>>();
                let wanted: &[&str] = if relation == "callers" {
                    &["CALLS_DIRECT"]
                } else {
                    &["CALLS_DIRECT", "REFERENCES", "IMPORTS", "IMPLEMENTS"]
                };
                let neighbors = self.graph_neighbors(&ids, true, limit)?;
                (
                    neighbors
                        .iter()
                        .filter(|(_, kind, _)| wanted.contains(&kind.as_str()))
                        .map(|(node, kind, confidence)| {
                            let mut located = node_location(node);
                            located["edge"] = json!(kind);
                            located["confidence"] = json!(confidence);
                            located
                        })
                        .collect::<Vec<_>>(),
                    "indexed relationship graph",
                )
            }
            other => bail!(
                "unsupported relation `{other}`; expected definition, callers, references, or implementations"
            ),
        };
        Ok(json!({
            "symbol": symbol,
            "relation": relation,
            "definitions": definitions,
            "results": results,
            "channel": channel,
            "freshness": self.snapshot(),
            "authority": "Indexed relationships are guidance. Confirm call sites with repo.search mode=exact and the compiler before relying on them."
        }))
    }

    pub fn history(&self, symbol: &str) -> Result<Value> {
        let nodes = self.search_nodes(&terms(symbol), 8)?;
        let mut commits = Vec::new();
        for node in &nodes {
            if let Some(output) = command_text(
                &self.root,
                &["log", "--format=%H%x1f%s", "-n", "12", "--", &node.file],
            ) {
                commits.extend(output.lines().filter_map(|l| {
                    let p: Vec<_> = l.split('\x1f').collect();
                    (p.len() == 2).then(|| json!({"hash":p[0],"subject":p[1],"file":node.file}))
                }));
            }
        }
        let mut co_changes = Vec::new();
        for node in &nodes {
            let mut statement = self.db.prepare(
                "SELECT node_a, node_b, count, score FROM co_changes \
                 WHERE node_a=?1 OR node_b=?1 ORDER BY count DESC LIMIT 5",
            )?;
            let matches = statement.query_map([&node.file], |row| {
                let first: String = row.get(0)?;
                let second: String = row.get(1)?;
                Ok(json!({
                    "file": if first == node.file { second } else { first },
                    "count": row.get::<_, i64>(2)?,
                    "score": row.get::<_, f64>(3)?,
                }))
            })?;
            co_changes.extend(matches.filter_map(Result::ok));
        }
        Ok(
            json!({"symbol":symbol,"nodes":nodes,"commits":commits,"frequent_co_change":co_changes,"revision":self.revision()}),
        )
    }

    pub fn checkpoint_create(&self, label: &str, include_untracked: bool) -> Result<Value> {
        let head = command_text(&self.root, &["rev-parse", "HEAD"])
            .context("checkpoints require a repository with an initial commit")?;
        let revision = self.revision();
        let timestamp = Utc::now();
        let id = format!(
            "cp_{}",
            &blake3::hash(
                format!(
                    "{label}:{}:{}",
                    timestamp.to_rfc3339(),
                    revision.workspace_digest
                )
                .as_bytes()
            )
            .to_hex()[..12]
        );
        let reference = format!("{}/{}/{}", CHECKPOINT_REF_PREFIX, ref_component(label), id);
        let temporary_index = self
            .root
            .join(INDEX_DIRECTORY)
            .join(format!("checkpoint-{}-{id}.index", std::process::id()));
        let git_index = command_text(&self.root, &["rev-parse", "--git-path", "index"])
            .map(PathBuf::from)
            .map(|path| {
                if path.is_absolute() {
                    path
                } else {
                    self.root.join(path)
                }
            });
        if let Some(index) = git_index.filter(|path| path.exists()) {
            fs::copy(index, &temporary_index).context("copying Git index for checkpoint")?;
        } else {
            run_git_with_index(&self.root, &temporary_index, &["read-tree", &head])?;
        }
        let result = (|| -> Result<String> {
            if include_untracked {
                run_git_with_index(&self.root, &temporary_index, &["add", "-A", "--"])?;
            } else {
                run_git_with_index(&self.root, &temporary_index, &["add", "-u", "--"])?;
            }
            let tree = run_git_with_index(&self.root, &temporary_index, &["write-tree"])?;
            let message = format!(
                "Codex checkpoint: {label}\n\nCheckpoint-Id: {id}\nBase-Head: {head}\nWorkspace-Digest: {}\nInclude-Untracked: {include_untracked}",
                revision.workspace_digest
            );
            let commit = run_git_with_index_env(
                &self.root,
                &temporary_index,
                &["commit-tree", &tree, "-p", &head, "-m", &message],
            )?;
            let output = Command::new("git")
                .args(["update-ref", &reference, &commit])
                .current_dir(&self.root)
                .output()
                .context("creating checkpoint ref")?;
            ensure!(
                output.status.success(),
                "creating checkpoint ref failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
            Ok(commit)
        })();
        let _ = fs::remove_file(&temporary_index);
        let commit = result?;
        Ok(
            json!({"id":id,"label":label,"reference":reference,"commit":commit,"base_head":head,"include_untracked":include_untracked,"revision":revision,"created_at":timestamp.to_rfc3339()}),
        )
    }

    pub fn checkpoint_list(&self, limit: usize) -> Result<Value> {
        let format = "%(refname)%00%(objectname)%00%(creatordate:iso-strict)%00%(subject)";
        let output = command_text(
            &self.root,
            &[
                "for-each-ref",
                &format!("--format={format}"),
                "--sort=-creatordate",
                CHECKPOINT_REF_PREFIX,
            ],
        )
        .unwrap_or_default();
        let checkpoints = output
            .lines()
            .filter_map(|line| {
                let fields: Vec<_> = line.split('\0').collect();
                (fields.len() == 4).then(|| {
                    json!({"reference":fields[0],"commit":fields[1],"created_at":fields[2],"subject":fields[3]})
                })
            })
            .take(limit)
            .collect::<Vec<_>>();
        Ok(json!({"checkpoints":checkpoints,"count":checkpoints.len()}))
    }

    pub fn checkpoint_diff(&self, reference: &str, max_bytes: usize) -> Result<Value> {
        validate_checkpoint_ref(reference)?;
        let output = Command::new("git")
            .args(["diff", "--binary", reference, "--"])
            .current_dir(&self.root)
            .output()
            .context("diffing checkpoint")?;
        ensure!(
            output.status.success(),
            "diffing checkpoint failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        let diff = String::from_utf8_lossy(&output.stdout);
        Ok(
            json!({"reference":reference,"diff":trim_text(&diff,max_bytes),"truncated":diff.len()>max_bytes,"current_revision":self.revision()}),
        )
    }

    pub fn checkpoint_restore_branch(
        &self,
        reference: &str,
        branch: Option<&str>,
    ) -> Result<Value> {
        validate_checkpoint_ref(reference)?;
        let branch = branch
            .map(ref_component)
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| {
                format!(
                    "restore-{}",
                    &blake3::hash(reference.as_bytes()).to_hex()[..8]
                )
            });
        let branch = format!("codex/{branch}");
        let output = Command::new("git")
            .args(["branch", &branch, reference])
            .current_dir(&self.root)
            .output()
            .context("creating checkpoint restore branch")?;
        ensure!(
            output.status.success(),
            "creating restore branch failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        Ok(
            json!({"reference":reference,"branch":branch,"checked_out":false,"note":"The working tree and current branch were not modified."}),
        )
    }

    pub fn record_decision(&self, input: RecordDecision) -> Result<Value> {
        if let Some(superseded) = input.supersedes.as_deref() {
            let exists: bool = self.db.query_row(
                "SELECT EXISTS(SELECT 1 FROM decisions WHERE id=?1)",
                [superseded],
                |row| row.get(0),
            )?;
            ensure!(exists, "cannot supersede unknown decision `{superseded}`");
        }
        let next: i64 = self.db.query_row(
            "SELECT COALESCE(MAX(sequence), 0) + 1 FROM decisions",
            [],
            |r| r.get(0),
        )?;
        let id = format!("DEC-{:04}", next);
        self.db.execute("INSERT INTO decisions(id, sequence, status, title, rationale, applies_to, consequences, supersedes, revision, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)", params![id, next, input.status, input.title, input.reason, serde_json::to_string(&input.applies_to)?, serde_json::to_string(&input.consequences)?, input.supersedes, self.revision().workspace_digest, Utc::now().to_rfc3339()])?;
        if let Some(superseded) = input.supersedes.as_deref() {
            self.db.execute(
                "UPDATE decisions SET status='superseded' WHERE id=?1",
                [superseded],
            )?;
        }
        for target in &input.applies_to {
            let node_id = self
                .search_nodes(&terms(target), 1)?
                .into_iter()
                .next()
                .map(|node| node.id);
            self.db.execute(
                "INSERT OR REPLACE INTO decision_targets(decision_id, node_id, target_ref) VALUES (?1, ?2, ?3)",
                params![id, node_id, target],
            )?;
        }
        self.db.execute(
            "INSERT OR REPLACE INTO metadata(key, value) VALUES ('revision', ?1)",
            [serde_json::to_string(&self.revision())?],
        )?;
        let path = if input.materialize {
            let dir = self.root.join("docs/decisions");
            fs::create_dir_all(&dir)?;
            let path = dir.join(format!("{:04}-{}.md", next, slug(&input.title)));
            fs::write(&path, decision_markdown(&id, &input))?;
            Some(relative(&self.root, &path))
        } else {
            None
        };
        self.rebuild_search_index()?;
        Ok(
            json!({"id":id,"status":input.status,"resource_uri":format!("rustrepo://decision/{id}"),"materialized_path":path,"revision":self.revision()}),
        )
    }

    pub fn record_steering(&self, input: RecordSteering) -> Result<Value> {
        let next: i64 = self.db.query_row(
            "SELECT COALESCE(MAX(sequence),0)+1 FROM steerings",
            [],
            |row| row.get(0),
        )?;
        let id = format!("STR-{next:04}");
        let revision = self.revision();
        self.db.execute(
            "INSERT INTO steerings(id,sequence,status,priority,title,instruction,scope,expires_at,revision,created_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![id,next,input.status,input.priority,input.title,input.instruction,serde_json::to_string(&input.scope)?,input.expires_at,revision.workspace_digest,Utc::now().to_rfc3339()],
        )?;
        self.rebuild_search_index()?;
        Ok(
            json!({"id":id,"status":input.status,"priority":input.priority,"scope":input.scope,"revision":revision}),
        )
    }

    pub fn steering_list(&self, query: Option<&str>, limit: usize) -> Result<Value> {
        let query = query.unwrap_or("");
        let ids: Vec<String> = if query.is_empty() {
            self.db
                .prepare("SELECT id FROM steerings ORDER BY sequence DESC LIMIT ?1")?
                .query_map([limit as i64], |row| row.get(0))?
                .collect::<rusqlite::Result<_>>()?
        } else {
            self.search_hits(&terms(query), Some("steering"), limit)?
                .into_iter()
                .filter_map(|hit| hit["entity_id"].as_str().map(str::to_owned))
                .collect()
        };
        let mut output = Vec::new();
        for id in ids {
            output.push(self.db.query_row(
                "SELECT id,status,priority,title,instruction,scope,expires_at,revision,created_at FROM steerings WHERE id=?1",
                [id],
                |row| Ok(json!({"id":row.get::<_,String>(0)?,"status":row.get::<_,String>(1)?,"priority":row.get::<_,String>(2)?,"title":row.get::<_,String>(3)?,"instruction":row.get::<_,String>(4)?,"scope":serde_json::from_str::<Value>(&row.get::<_,String>(5)?).unwrap_or_else(|_|json!([])),"expires_at":row.get::<_,Option<String>>(6)?,"revision":row.get::<_,String>(7)?,"created_at":row.get::<_,String>(8)?})),
            )?);
        }
        Ok(json!({"query":query,"steerings":output,"snapshot":self.snapshot()}))
    }

    pub fn validate_change(
        &self,
        context_id: &str,
        diff: Option<&str>,
        run_checks: bool,
    ) -> Result<Value> {
        let payload: String = self
            .db
            .query_row(
                "SELECT payload FROM change_contexts WHERE id=?1",
                [context_id],
                |r| r.get(0),
            )
            .optional()?
            .context("unknown change context")?;
        let context: Value = serde_json::from_str(&payload)?;
        // `git diff --` reports only unstaged work. An agent that staged its
        // edits before validating used to get an empty diff and a clean-looking
        // report; `HEAD` covers staged and unstaged changes alike.
        let diff = diff
            .map(str::to_string)
            .or_else(|| command_text(&self.root, &["diff", "HEAD", "--"]))
            .or_else(|| command_text(&self.root, &["diff", "--"]))
            .unwrap_or_default();
        let changed_files = changed_files_in_diff(&diff);
        let mut validation_queue =
            self.activate_quality_constraints_for_diff(context_id, &diff, &changed_files)?;
        let expected: BTreeSet<String> = context
            .get("likely_change_surface")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
        let unmodified: Vec<_> = expected.difference(&changed_files).cloned().collect();
        let checks = if run_checks {
            vec![
                check(&self.root, &["fmt", "--check"]),
                check(&self.root, &["check", "--all-targets"]),
                check(
                    &self.root,
                    &["clippy", "--all-targets", "--", "-D", "warnings"],
                ),
                check(&self.root, &["test", "--all-targets"]),
            ]
        } else {
            Vec::new()
        };
        let artifact_checks = if run_checks {
            changed_files
                .iter()
                .filter_map(|path| artifact_validator(Path::new(path)))
                .map(|(program, arguments)| {
                    run_optional_command_check(&self.root, program, &arguments)
                })
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        if run_checks {
            self.record_change_inferred_results(context_id, &artifact_checks)?;
            validation_queue = self.quality_validation_queue(context_id)?;
        }
        let learned_checks = if run_checks {
            self.execute_quality_obligations(context_id)?
        } else {
            Vec::new()
        };
        let lifecycle = context
            .get("lifecycle")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let legacy_left = lifecycle
            .into_iter()
            .filter_map(|entry| {
                entry
                    .get("symbol")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .filter(|symbol| diff.contains(symbol))
            .collect::<Vec<_>>();
        let blocking = self.blocking_obligation_status(context_id, run_checks)?;
        Ok(
            json!({"context_id":context_id,"revision_before":context.get("revision"),"revision_after":self.revision(),"snapshot":self.snapshot(),"changed_files":changed_files,"expected_affected_files":expected,"unmodified_expected_callers":unmodified,"new_references":"Re-run change.prepare after signature changes to refresh static candidates.","architectural_violations":self.decision_conflicts(&diff)?,"legacy_paths_touched":legacy_left,"unresolved_edges":context.get("unresolved_edges"),"recommended_tests":context.get("tests"),"recommended_commands":["cargo fmt --check","cargo check --all-targets","cargo clippy --all-targets -- -D warnings","cargo test","gtk4-builder-tool validate <changed.ui>","blueprint-compiler compile <changed.blp>","xmllint --noout <changed.xml>"],"validation_queue":validation_queue,"checks":checks,"artifact_checks":artifact_checks,"learned_checks":learned_checks,"blocking":blocking,"uncertainty":"Artifact validators can detect local syntax/schema issues, but not runtime registration, generated-code drift, external-consumer compatibility, or deployment behavior."}),
        )
    }

    /// Reports whether any obligation a human marked `enforcement: "block"` is
    /// unsatisfied for this context.
    ///
    /// `blocking` was previously written, ordered on, and serialised, but never
    /// read, so `enforcement: "block"` gated nothing at all. Validation still
    /// returns a result rather than an error — Crusty reports, the human and the
    /// compiler decide — but the verdict is now explicit and machine-readable.
    fn blocking_obligation_status(&self, context_id: &str, run_checks: bool) -> Result<Value> {
        let mut statement = self.db.prepare(
            "SELECT id,status,expected,selected_reason FROM validation_obligations WHERE context_id=?1 AND blocking=1 AND status!='cancelled' ORDER BY id",
        )?;
        let obligations = statement
            .query_map([context_id], |row| {
                Ok(json!({"obligation_id":row.get::<_,String>(0)?,"status":row.get::<_,String>(1)?,"expected":row.get::<_,String>(2)?,"selected_reason":row.get::<_,String>(3)?}))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let unsatisfied = obligations
            .iter()
            .filter(|item| item["status"] != "passed")
            .cloned()
            .collect::<Vec<_>>();
        let note = if obligations.is_empty() {
            "No blocking quality constraint matched this change."
        } else if !run_checks {
            "Blocking obligations were not executed because run_checks was false; their status is unresolved."
        } else if unsatisfied.is_empty() {
            "Every blocking obligation for this change passed."
        } else {
            "A human-approved blocking constraint is unsatisfied for this change; resolve it or record an explicit outcome before completing the change."
        };
        Ok(
            json!({"blocked":!unsatisfied.is_empty(),"checked":run_checks,"unsatisfied":unsatisfied,"total":obligations.len(),"note":note}),
        )
    }

    pub fn cleanup_candidates(&self, scope: Option<&str>) -> Result<Value> {
        let scope = scope.unwrap_or("");
        let nodes = self.search_nodes(&terms(scope), 500)?;
        let mut candidates = Vec::new();
        for node in nodes {
            if node.visibility == "private" {
                let inbound: i64 = self.db.query_row(
                    "SELECT COUNT(*) FROM edges WHERE dst=?1",
                    [node.id],
                    |r| r.get(0),
                )?;
                if inbound == 0 {
                    candidates.push(json!({"symbol":node.canonical_name,"file":node.file,"kind":node.kind,"cleanup_score":0.45,"reason":"private symbol has no indexed inbound references","confidence":0.55,"provenance":"StaticIndex"}));
                }
            }
        }
        Ok(
            json!({"scope":scope,"candidates":candidates,"note":"Candidates must be confirmed with rust-analyzer/compiler before deletion.","revision":self.revision()}),
        )
    }

    pub fn locate(&self, concept: &str, limit: usize, include_source: bool) -> Result<Value> {
        let query_terms = terms(concept);
        let hybrid = self.hybrid_nodes(concept, limit)?;
        let nodes = hybrid
            .iter()
            .map(|hit| hit.node.clone())
            .collect::<Vec<_>>();
        let full_text_hits = self.search_hits(&query_terms, None, limit)?;
        let docs = self.search_hits(&query_terms, Some("document"), limit.min(3))?;
        let runtime_contracts = self.search_contract_artifacts(concept, limit.min(8))?;
        let mut implementations = Vec::new();
        let mut tests = Vec::new();
        let mut canonical = Vec::new();
        for (node, hit) in nodes.iter().zip(&hybrid) {
            let item = json!({
                "id":node.id,
                "symbol":node.canonical_name,
                "file":node.file,
                "kind":node.kind,
                "visibility":node.visibility,
                "provenance":"HybridRRF",
                "score":hit.score,
                "channels":hit.channels,
                "exact_match":hit.exact_match,
                "embedding_evidence":hit.embedding,
            });
            if node.file.contains("test") || short_name(&node.canonical_name).starts_with("test_") {
                tests.push(item);
            } else if node.visibility == "public" {
                canonical.push(item);
            } else {
                implementations.push(item);
            }
        }
        let source_slices = if include_source {
            nodes
                .iter()
                .take(limit.min(8))
                .map(|node| self.source_slice(node, "concept candidate", "LOCATES"))
                .collect::<Result<Vec<_>>>()?
        } else {
            Vec::new()
        };
        let work_items = self.work_list(Some(concept), limit)?["items"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        Ok(
            json!({"concept":concept,"snapshot":self.snapshot(),"retrieval":"HybridRRF","retrieval_provenance":self.retrieval_provenance(),"canonical_candidates":canonical,"implementation_candidates":implementations,"tests":tests,"documentation":docs,"runtime_contracts":runtime_contracts,"full_text_hits":full_text_hits,"governing_decisions":self.decisions_for(concept)?,"known_work":work_items,"source_slices":source_slices,"blind_spots":["Indexed GTK/D-Bus artifacts are searchable but runtime registration, generated code, external consumers, and configuration-selected implementations still require confirmation."],"context_budget":{"items":limit,"source_included":include_source}}),
        )
    }

    pub fn explain(&self, target: &str, budget: usize) -> Result<Value> {
        let nodes = self.search_nodes(&terms(target), budget.min(20))?;
        let lifecycle = self.lifecycle_for(&nodes)?;
        Ok(
            json!({"target":target,"snapshot":self.snapshot(),"nodes":nodes,"lifecycle":lifecycle,"decisions":self.decisions_for(target)?,"work":self.work_list(Some(target), budget.min(10))?,"source_slices":nodes.iter().take(4).map(|node|self.source_slice(node,"explanation","DEFINES")).collect::<Result<Vec<_>>>()?,"confidence_note":"Resolved rust-analyzer locations are compiler-backed; lifecycle and text-derived results carry their own provenance and confidence."}),
        )
    }

    pub fn why(&self, target: &str) -> Result<Value> {
        let nodes = self.search_nodes(&terms(target), 12)?;
        Ok(
            json!({"target":target,"snapshot":self.snapshot(),"decisions":self.decisions_for(target)?,"lifecycle":self.lifecycle_for(&nodes)?,"history":self.history(target)?,"evidence_policy":"Only human-authored decisions and source/Git evidence are confirmed. Heuristics remain labeled inferences."}),
        )
    }

    pub fn constraints(&self, change: &str) -> Result<Value> {
        Ok(
            json!({"change":change,"snapshot":self.snapshot(),"decisions":self.decisions_for(change)?,"steerings":self.steering_list(Some(change),20)?["steerings"],"learned_quality_constraints":self.relevant_quality_constraints(change)?,"lifecycle_risks":self.obsolete_candidates(Some(change), 20)?,"runtime_contracts":self.search_contract_artifacts(change,12)?,"unresolved_edges":["Ambiguous syntax references are preserved as uncertainty rather than promoted to affected files.","Dynamic dispatch, macros, runtime registration, generated bindings, external API consumers, and deployment state need external verification."],"required_verification":["cargo check --all-targets","cargo test","Run the bounded feature-profile plan from repo.matrix.","Validate changed GTK markup with project GTK tooling.","Compare D-Bus XML changes with runtime introspection and external consumer expectations.","Run a deployment smoke test in the target environment when packaging or service configuration changes."]}),
        )
    }

    pub fn obsolete_candidates(&self, scope: Option<&str>, limit: usize) -> Result<Value> {
        let scope = scope.unwrap_or("");
        let mut statement = self.db.prepare("SELECT l.legacy_node,l.canonical_node,l.kind,l.evidence,l.provenance,l.confidence,n.kind,n.canonical_name,n.file,n.visibility,c.canonical_name FROM lifecycle_edges l JOIN nodes n ON n.id=l.legacy_node LEFT JOIN nodes c ON c.id=l.canonical_node WHERE lower(n.canonical_name) LIKE ?1 OR lower(n.file) LIKE ?1 OR ?1='%%' ORDER BY l.confidence DESC LIMIT ?2")?;
        let needle = if scope.is_empty() {
            "%%".to_owned()
        } else {
            format!("%{}%", scope.to_lowercase())
        };
        let records: Vec<LifecycleRecord> = statement
            .query_map(params![needle, limit as i64], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    Node {
                        id: row.get(0)?,
                        kind: row.get(6)?,
                        canonical_name: row.get(7)?,
                        crate_name: None,
                        file: row.get(8)?,
                        start_line: 0,
                        end_line: 0,
                        visibility: row.get(9)?,
                        content_hash: String::new(),
                    },
                    row.get(10)?,
                ))
            })?
            .filter_map(Result::ok)
            .collect();
        let mut candidates = Vec::new();
        for (
            legacy_id,
            canonical_id,
            kind,
            evidence,
            provenance,
            confidence,
            node,
            canonical_name,
        ) in records
        {
            let callers = self.references_for(&[legacy_id], 1)?;
            let classification = if canonical_id.is_some() {
                "architecturally-superseded"
            } else {
                "suspected-stale-fallback"
            };
            candidates.push(json!({"classification":classification,"symbol":node.canonical_name,"file":node.file,"replacement":canonical_name,"lifecycle_kind":kind,"evidence":trim_text(&evidence,900),"provenance":provenance,"confidence":confidence,"current_callers":callers,"complete_removal_slice":{"remove":[node.canonical_name],"update_callers":callers.iter().map(|caller|caller.canonical_name.clone()).collect::<Vec<_>>(),"expected_affected_systems":[node.file],"verification_boundary":["cargo check --all-targets","cargo test","Confirm no external/public consumer or configuration path requires the compatibility boundary."]},"unresolved_questions":["Are there external consumers, runtime-selected paths, generated code, or persisted historical formats not represented by the repository index?"]}));
        }
        Ok(
            json!({"scope":scope,"snapshot":self.snapshot(),"candidates":candidates,"safety_note":"Referenced code is not presumed required or removable. Only explicit replacement evidence produces an architecturally-superseded candidate; removal remains blocked on the stated verification boundary."}),
        )
    }

    pub fn work_list(&self, query: Option<&str>, limit: usize) -> Result<Value> {
        let query = query.unwrap_or("").to_lowercase();
        let mut statement = self.db.prepare("SELECT id,title,status,priority,kind,scope_json,evidence_json,depends_json,blocked_json,acceptance_json,verification_json,discovered_from,provenance,confidence,last_validated_snapshot,updated_at FROM work_items WHERE (lower(title) LIKE ?1 OR lower(scope_json) LIKE ?1 OR ?1='%%') AND (?3=1 OR provenance!='SourceDoc' OR status!='proposed' OR confidence>=0.5) ORDER BY CASE priority WHEN 'critical' THEN 0 WHEN 'high' THEN 1 WHEN 'normal' THEN 2 ELSE 3 END, updated_at DESC LIMIT ?2")?;
        let items = statement
            .query_map(
                params![
                    if query.is_empty() {
                        "%%".to_owned()
                    } else {
                        format!("%{query}%")
                    },
                    limit as i64,
                    i64::from(!query.is_empty()),
                ],
                work_row,
            )?
            .filter_map(Result::ok)
            .collect::<Vec<_>>();
        Ok(json!({"query":query,"snapshot":self.snapshot(),"items":items}))
    }

    pub fn work_next(&self, query: Option<&str>) -> Result<Value> {
        let items = self.work_list(query, 50)?["items"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let next = items.into_iter().find(|item| {
            item["blocked_by"].as_array().is_none_or(Vec::is_empty)
                && (item["status"] == "accepted"
                    || item["status"] == "in_progress"
                    || item["status"] == "proposed")
        });
        Ok(
            json!({"snapshot":self.snapshot(),"next":next,"selection_note":"Blocked, completed, and obsolete work is excluded. Proposed work still requires confirmation before becoming an accepted plan."}),
        )
    }

    pub fn work_propose(&self, input: WorkItemInput) -> Result<Value> {
        let revision = self.revision();
        let identity = format!("{}:{:?}", input.title, input.scope);
        let id = format!("work_{}", &blake3::hash(identity.as_bytes()).to_hex()[..12]);
        self.db.execute("INSERT INTO work_items(id,title,status,priority,kind,scope_json,evidence_json,depends_json,blocked_json,acceptance_json,verification_json,discovered_from,provenance,confidence,last_validated_snapshot,created_at,updated_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,'explicit MCP proposal','HumanDecision',1.0,?12,?13,?13) ON CONFLICT(id) DO UPDATE SET title=excluded.title,status=excluded.status,priority=excluded.priority,kind=excluded.kind,scope_json=excluded.scope_json,evidence_json=excluded.evidence_json,depends_json=excluded.depends_json,blocked_json=excluded.blocked_json,acceptance_json=excluded.acceptance_json,verification_json=excluded.verification_json,last_validated_snapshot=excluded.last_validated_snapshot,updated_at=excluded.updated_at", params![id,input.title,input.status,input.priority,input.kind,serde_json::to_string(&input.scope)?,serde_json::to_string(&input.evidence)?,serde_json::to_string(&input.depends_on)?,serde_json::to_string(&input.blocked_by)?,serde_json::to_string(&input.acceptance_criteria)?,serde_json::to_string(&input.verification)?,revision.workspace_digest,Utc::now().to_rfc3339()])?;
        self.rebuild_search_index()?;
        Ok(
            json!({"id":id,"snapshot":self.snapshot(),"status":input.status,"provenance":"HumanDecision"}),
        )
    }

    pub fn work_update(&self, id: &str, patch: &Value) -> Result<Value> {
        let current = self.db.query_row("SELECT status,priority,kind,scope_json,evidence_json,depends_json,blocked_json,acceptance_json,verification_json FROM work_items WHERE id=?1",[id],|row|Ok(json!({"status":row.get::<_,String>(0)?,"priority":row.get::<_,String>(1)?,"kind":row.get::<_,String>(2)?,"scope":serde_json::from_str::<Value>(&row.get::<_,String>(3)?).unwrap_or(json!([])),"evidence":serde_json::from_str::<Value>(&row.get::<_,String>(4)?).unwrap_or(json!([])),"depends_on":serde_json::from_str::<Value>(&row.get::<_,String>(5)?).unwrap_or(json!([])),"blocked_by":serde_json::from_str::<Value>(&row.get::<_,String>(6)?).unwrap_or(json!([])),"acceptance_criteria":serde_json::from_str::<Value>(&row.get::<_,String>(7)?).unwrap_or(json!([])),"verification":serde_json::from_str::<Value>(&row.get::<_,String>(8)?).unwrap_or(json!([]))})))?;
        let field = |name: &str| {
            patch
                .get(name)
                .cloned()
                .unwrap_or_else(|| current[name].clone())
        };
        self.db.execute("UPDATE work_items SET status=?1,priority=?2,kind=?3,scope_json=?4,evidence_json=?5,depends_json=?6,blocked_json=?7,acceptance_json=?8,verification_json=?9,last_validated_snapshot=?10,updated_at=?11 WHERE id=?12",params![field("status").as_str(),field("priority").as_str(),field("kind").as_str(),field("scope").to_string(),field("evidence").to_string(),field("depends_on").to_string(),field("blocked_by").to_string(),field("acceptance_criteria").to_string(),field("verification").to_string(),self.revision().workspace_digest,Utc::now().to_rfc3339(),id])?;
        self.rebuild_search_index()?;
        Ok(json!({"id":id,"snapshot":self.snapshot(),"updated":true}))
    }

    pub fn status(&self) -> Result<Value> {
        let current_revision = self.revision();
        let indexed_revision = self.indexed_revision();
        let stale = indexed_revision.as_ref() != Some(&current_revision);
        let nodes: i64 = self
            .db
            .query_row("SELECT COUNT(*) FROM nodes", [], |row| row.get(0))?;
        let edges: i64 = self
            .db
            .query_row("SELECT COUNT(*) FROM edges", [], |row| row.get(0))?;
        let semantic_edges: i64 = self.db.query_row(
            "SELECT COUNT(*) FROM edges WHERE provenance='RustAnalyzer'",
            [],
            |row| row.get(0),
        )?;
        let embeddings: i64 =
            self.db
                .query_row("SELECT COUNT(*) FROM symbol_embeddings", [], |row| {
                    row.get(0)
                })?;
        let lifecycle: i64 =
            self.db
                .query_row("SELECT COUNT(*) FROM lifecycle_edges", [], |row| row.get(0))?;
        let work: i64 = self
            .db
            .query_row("SELECT COUNT(*) FROM work_items", [], |row| row.get(0))?;
        let actionable_work: i64 = self.db.query_row(
            "SELECT COUNT(*) FROM work_items WHERE provenance!='SourceDoc' OR status!='proposed' OR confidence>=0.5",
            [],
            |row| row.get(0),
        )?;
        let package_targets: i64 =
            self.db
                .query_row("SELECT COUNT(*) FROM package_targets", [], |row| row.get(0))?;
        let package_features: i64 =
            self.db
                .query_row("SELECT COUNT(*) FROM package_features", [], |row| {
                    row.get(0)
                })?;
        let unresolved_static_references: i64 =
            self.db
                .query_row("SELECT COUNT(*) FROM unresolved_references", [], |row| {
                    row.get(0)
                })?;
        let runtime_contracts: i64 = self.db.query_row(
            "SELECT COUNT(*) FROM documents WHERE kind='runtime_contract'",
            [],
            |row| row.get(0),
        )?;
        let quality_memory = self.quality_counts()?;
        // Files that could not be read are reported rather than silently
        // indexed as empty, so a permissions or encoding problem is visible.
        let unreadable: Vec<String> = self
            .db
            .query_row(
                "SELECT value FROM metadata WHERE key='unreadable_inputs'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .and_then(|value| serde_json::from_str(&value).ok())
            .unwrap_or_default();
        let mut degraded = vec![Value::from(
            "Runtime registration, generated code, external consumers, and profiles other than the indexed target/features require external verification; repo.matrix returns a bounded Cargo profile plan.",
        )];
        if !unreadable.is_empty() {
            degraded.push(Value::from(format!(
                "{} source file(s) could not be read or are not UTF-8 and were skipped; their symbols are absent from this snapshot.",
                unreadable.len()
            )));
        }
        Ok(
            json!({"snapshot":self.snapshot(),"current_revision":current_revision,"stale":stale,"index":self.index_status(),"counts":{"nodes":nodes,"edges":edges,"semantic_edges":semantic_edges,"unresolved_static_references":unresolved_static_references,"runtime_contract_artifacts":runtime_contracts,"embeddings":embeddings,"lifecycle_evidence":lifecycle,"work_items":work,"actionable_work_items":actionable_work,"package_targets":package_targets,"package_features":package_features},"quality_memory":quality_memory,"unreadable_inputs":unreadable,"degraded_areas":degraded,"cache":"rebuildable SQLite cache with atomic published generations; no repository files are changed by indexing."}),
        )
    }

    pub fn matrix(&self) -> Result<Value> {
        let targets = self
            .db
            .prepare("SELECT package,target_name,kind,crate_types,required_features,edition,doc,doctest,test,bench FROM package_targets ORDER BY package,target_name")?
            .query_map([], |row| {
                Ok(json!({
                    "package": row.get::<_, String>(0)?,
                    "target": row.get::<_, String>(1)?,
                    "kind": serde_json::from_str::<Value>(&row.get::<_, String>(2)?).unwrap_or_else(|_| json!([])),
                    "crate_types": serde_json::from_str::<Value>(&row.get::<_, String>(3)?).unwrap_or_else(|_| json!([])),
                    "required_features": serde_json::from_str::<Value>(&row.get::<_, String>(4)?).unwrap_or_else(|_| json!([])),
                    "edition": row.get::<_, String>(5)?,
                    "doc": row.get::<_, bool>(6)?,
                    "doctest": row.get::<_, bool>(7)?,
                    "test": row.get::<_, bool>(8)?,
                    "bench": row.get::<_, bool>(9)?,
                }))
            })?
            .filter_map(Result::ok)
            .collect::<Vec<_>>();
        let features = self
            .db
            .prepare("SELECT package,feature,dependencies FROM package_features ORDER BY package,feature")?
            .query_map([], |row| {
                Ok(json!({
                    "package": row.get::<_, String>(0)?,
                    "feature": row.get::<_, String>(1)?,
                    "dependencies": serde_json::from_str::<Value>(&row.get::<_, String>(2)?).unwrap_or_else(|_| json!([])),
                }))
            })?
            .filter_map(Result::ok)
            .collect::<Vec<_>>();
        let packages = targets
            .iter()
            .filter_map(|target| target["package"].as_str())
            .collect::<BTreeSet<_>>();
        let mut verification_commands = Vec::new();
        for package in packages {
            verification_commands.push(format!("cargo check -p {package}"));
            verification_commands.push(format!("cargo check -p {package} --no-default-features"));
            verification_commands.push(format!("cargo check -p {package} --all-features"));
            verification_commands.extend(
                features
                    .iter()
                    .filter(|feature| {
                        feature["package"] == package && feature["feature"] != "default"
                    })
                    .filter_map(|feature| feature["feature"].as_str())
                    .take(12)
                    .map(|feature| {
                        format!(
                            "cargo check -p {package} --no-default-features --features {feature}"
                        )
                    }),
            );
        }
        Ok(
            json!({"targets":targets,"features":features,"revision":self.revision(),"verification_plan":{"commands":verification_commands,"executed":false,"scope":"default, no-default, all-features, and up to 12 individual features per package","limitations":"Feature interactions can be combinatorial; target-specific, mutually exclusive, and deployment profiles still need project-specific CI/runtime coverage."}}),
        )
    }

    pub fn refresh(&mut self, scope: Option<&str>) -> Result<Value> {
        match scope.unwrap_or("workspace") {
            "workspace" | "cargo" => self.reindex()?,
            "git" => {
                let _publisher_lease = self.acquire_publisher_lease()?;
                let revision = self.revision();
                self.with_savepoint("git_only_refresh", |service| {
                    service.refresh_git(&revision.workspace_digest)?;
                    service.rebuild_search_index()?;
                    service.db.execute(
                        "INSERT OR REPLACE INTO metadata(key,value) VALUES ('git_indexed_head',?1)",
                        [revision.head.clone().unwrap_or_default()],
                    )?;
                    Ok(())
                })?;
            }
            "incremental" => self.refresh_if_stale()?,
            other => bail!(
                "unsupported refresh scope `{other}`; use workspace, cargo, git, or incremental"
            ),
        }
        self.status()
    }

    fn acquire_publisher_lease(&self) -> Result<PublisherLease> {
        let path = self.root.join(INDEX_DIRECTORY).join("index.lock");
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        FileExt::try_lock_exclusive(&lock)
            .context("another Crusty process owns the index publisher lease")?;
        Ok(PublisherLease(lock))
    }

    pub fn resource(&self, uri: &str) -> Result<Value> {
        let path = uri
            .strip_prefix("rustrepo://")
            .context("unsupported resource URI")?;
        if let Some(value) = self.quality_resource(path)? {
            return Ok(value);
        }
        let value = match path {
            "workspace/overview" => self.orient("workspace overview")?,
            _ if path.starts_with("symbol/") => {
                json!({"symbol":&path[7..],"nodes":self.search_nodes(&terms(&path[7..]),20)?,"revision":self.revision()})
            }
            _ if path.starts_with("change/") => {
                let id = &path[7..];
                let s: String = self.db.query_row(
                    "SELECT payload FROM change_contexts WHERE id=?1",
                    [id],
                    |r| r.get(0),
                )?;
                serde_json::from_str(&s)?
            }
            _ if path.starts_with("decision/") => self.decision_resource(&path[9..])?,
            _ if path.starts_with("history/") => self.history(&path[8..])?,
            _ if path.starts_with("work/") => self.work_list(Some(&path[5..]), 50)?,
            _ => bail!("unknown resource"),
        };
        Ok(value)
    }

    pub fn index_status(&self) -> Value {
        json!({"semantic_engine":"rust-analyzer LSP companion (opt-in, on-demand, persisted by semantic snapshot)","rust_analyzer_enabled":self.ra_enabled,"rust_analyzer_running":self.ra.is_some(),"rust_analyzer_start_attempted":self.ra_start_attempted,"rust_analyzer_program":self.ra_program.to_string_lossy(),"rust_analyzer_error":self.ra_start_error,"filesystem_watcher":self.watcher.is_some(),"fallback":"AST Syntax index with ambiguity suppression; unresolved candidates are reported separately","embedding_model":EMBEDDING_MODEL,"embedding_dimensions":EMBEDDING_DIMENSIONS,"embedding_card_version":SYMBOL_CARD_VERSION,"embedding":self.embedding_index_status(),"retrieval":"BM25 + embedding + typed graph RRF","store":"SQLite FTS5 + graph + vectors"})
    }

    fn embedding_index_status(&self) -> Value {
        let metadata = |key: &str| {
            self.db
                .query_row("SELECT value FROM metadata WHERE key=?1", [key], |row| {
                    row.get::<_, String>(0)
                })
                .optional()
                .ok()
                .flatten()
        };
        json!({
            "model":EMBEDDING_MODEL,
            "dimensions":EMBEDDING_DIMENSIONS,
            "card_version":SYMBOL_CARD_VERSION,
            "vectors":self.db.query_row("SELECT COUNT(*) FROM symbol_embeddings WHERE model=?1 AND dimensions=?2",params![EMBEDDING_MODEL,EMBEDDING_DIMENSIONS],|row|row.get::<_,i64>(0)).unwrap_or(0),
            "semantic_snapshot":self.active_semantic_snapshot_id(),
            "last_build":{"recomputed":metadata("embedding_recomputed").and_then(|value|value.parse::<usize>().ok()),"reused":metadata("embedding_reused").and_then(|value|value.parse::<usize>().ok()),"updated_at":metadata("embedding_updated_at")},
            "storage":"SQLite BLOB; bounded brute-force cosine ranking",
            "qualification":"Local feature embedding for identifier and vocabulary discovery; typed graph/compiler/runtime evidence remains authoritative."
        })
    }

    fn retrieval_provenance(&self) -> Value {
        json!({
            "exact_identifier":{"source":"published symbol index","precedence":"always before fused non-exact candidates"},
            "lexical":{"source":"SQLite FTS5","ranking":"BM25"},
            "embedding":self.embedding_index_status(),
            "graph":{"source":"typed edges","provenance":"RustAnalyzer, Syntax, or StaticIndex per edge","role":"expansion evidence, never proof of runtime behavior"},
            "fusion":{"algorithm":"weighted reciprocal-rank fusion","deterministic":true}
        })
    }

    fn snapshot(&self) -> Value {
        let semantic = self.semantic_snapshot_resource();
        let feature_profile = semantic
            .get("feature_profile")
            .cloned()
            .unwrap_or(Value::Null);
        let target_triple = semantic
            .get("target_triple")
            .cloned()
            .unwrap_or(Value::Null);
        json!({"repository":self.root.to_string_lossy(),"branch":command_text(&self.root,&["branch","--show-current"]),"revision":self.indexed_revision(),"generation":self.active_generation(),"semantic_snapshot":semantic,"indexing_timestamp":self.db.query_row("SELECT value FROM metadata WHERE key='indexed_at'",[],|row|row.get::<_,String>(0)).optional().ok().flatten(),"indexer_version":self.db.query_row("SELECT value FROM metadata WHERE key='indexer_version'",[],|row|row.get::<_,String>(0)).optional().ok().flatten(),"cargo_assumptions":{"features":feature_profile,"target_triple":target_triple,"package_targets":self.db.query_row("SELECT COUNT(*) FROM package_targets",[],|row|row.get::<_,i64>(0)).unwrap_or(0),"package_features":self.db.query_row("SELECT COUNT(*) FROM package_features",[],|row|row.get::<_,i64>(0)).unwrap_or(0)},"index":self.index_status()})
    }

    fn indexed_revision(&self) -> Option<Revision> {
        self.db
            .query_row(
                "SELECT value FROM metadata WHERE key='revision'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .ok()
            .flatten()
            .and_then(|value| serde_json::from_str(&value).ok())
    }

    fn lifecycle_for(&self, nodes: &[Node]) -> Result<Vec<Value>> {
        let mut output = Vec::new();
        for node in nodes {
            let mut statement = self.db.prepare("SELECT l.kind,l.evidence,l.provenance,l.confidence,c.canonical_name FROM lifecycle_edges l LEFT JOIN nodes c ON c.id=l.canonical_node WHERE l.legacy_node=?1 OR l.canonical_node=?1")?;
            let rows = statement.query_map([node.id], |row| Ok(json!({"symbol":node.canonical_name,"kind":row.get::<_,String>(0)?,"evidence":trim_text(&row.get::<_,String>(1)?,800),"provenance":row.get::<_,String>(2)?,"confidence":row.get::<_,f64>(3)?,"related_symbol":row.get::<_,Option<String>>(4)?})))?;
            output.extend(rows.filter_map(Result::ok));
        }
        Ok(output)
    }

    fn search_nodes(&self, query: &[String], limit: usize) -> Result<Vec<Node>> {
        if query.iter().all(String::is_empty) {
            let mut statement = self.db.prepare("SELECT id,kind,canonical_name,crate_name,file,start_line,end_line,visibility,content_hash FROM nodes ORDER BY visibility DESC,canonical_name LIMIT ?1")?;
            return Ok(statement
                .query_map([limit as i64], node_from_row)?
                .collect::<rusqlite::Result<_>>()?);
        }
        let last = query.last().map(String::as_str).unwrap_or("");
        let mut statement = self.db.prepare(
            "SELECT n.id,n.kind,n.canonical_name,n.crate_name,n.file,n.start_line,n.end_line,n.visibility,n.content_hash \
             FROM search_index s JOIN nodes n ON s.entity_type='node' AND s.entity_id=CAST(n.id AS TEXT) \
             WHERE search_index MATCH ?1 \
             ORDER BY CASE WHEN lower(n.canonical_name)=lower(?2) THEN -1000.0 \
                           WHEN lower(n.canonical_name) LIKE '%::' || lower(?2) THEN -500.0 \
                           ELSE bm25(search_index,0.0,0.0,10.0,4.0,1.0) END, \
                      n.visibility DESC,n.canonical_name LIMIT ?3",
        )?;
        let nodes = statement
            .query_map(params![fts_query(query), last, limit as i64], node_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if !nodes.is_empty() {
            return Ok(nodes);
        }
        let mut fallback = self.db.prepare("SELECT id,kind,canonical_name,crate_name,file,start_line,end_line,visibility,content_hash FROM nodes WHERE lower(canonical_name) LIKE ?1 OR lower(file) LIKE ?1 ORDER BY visibility DESC,canonical_name LIMIT ?2")?;
        Ok(fallback
            .query_map(
                params![format!("%{}%", last.to_lowercase()), limit as i64],
                node_from_row,
            )?
            .collect::<rusqlite::Result<_>>()?)
    }
    fn references_for(&self, ids: &[i64], depth: usize) -> Result<Vec<Node>> {
        if ids.is_empty() {
            return Ok(vec![]);
        }
        let mut seen = BTreeSet::new();
        let mut current = ids.to_vec();
        for _ in 0..depth.max(1) {
            let q = std::iter::repeat_n("?", current.len())
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                "SELECT DISTINCT n.id,n.kind,n.canonical_name,n.crate_name,n.file,n.start_line,n.end_line,n.visibility,n.content_hash FROM edges e JOIN nodes n ON e.src=n.id WHERE e.dst IN ({q}) AND ((e.provenance='RustAnalyzer' AND e.confidence>=0.9) OR (e.provenance='Syntax' AND e.kind IN ('CALLS_DIRECT','REFERENCES','IMPLEMENTS','IMPORTS') AND e.confidence>=0.7)) ORDER BY e.confidence DESC,n.canonical_name LIMIT 80"
            );
            let mut stmt = self.db.prepare(&sql)?;
            let found: Vec<Node> = stmt
                .query_map(rusqlite::params_from_iter(current.iter()), node_from_row)?
                .filter_map(Result::ok)
                .collect();
            current = found
                .iter()
                .map(|n| n.id)
                .filter(|id| seen.insert(*id))
                .collect();
        }
        let mut out = Vec::new();
        for id in seen {
            out.push(self.node(id)?);
        }
        Ok(out)
    }
    fn node(&self, id: i64) -> Result<Node> {
        Ok(self.db.query_row("SELECT id,kind,canonical_name,crate_name,file,start_line,end_line,visibility,content_hash FROM nodes WHERE id=?1",[id],node_from_row)?)
    }
    fn node_by_canonical_name(&self, canonical_name: &str) -> Result<Option<Node>> {
        Ok(self
            .db
            .query_row(
                "SELECT id,kind,canonical_name,crate_name,file,start_line,end_line,visibility,content_hash FROM nodes WHERE lower(canonical_name)=lower(?1) LIMIT 1",
                [canonical_name],
                node_from_row,
            )
            .optional()?)
    }
    fn test_nodes(&self, nodes: &[Node]) -> Result<Vec<Node>> {
        let names: Vec<String> = nodes
            .iter()
            .map(|n| short_name(&n.canonical_name).to_lowercase().to_string())
            .collect();
        let all = self.search_nodes(&["".into()], 500)?;
        Ok(all
            .into_iter()
            .filter(|n| {
                n.file.contains("test")
                    || names
                        .iter()
                        .any(|s| n.canonical_name.to_lowercase().contains(s))
            })
            .take(20)
            .collect())
    }
    fn semantic_references(&self, targets: &[Node]) -> Vec<SourceSlice> {
        self.semantic_locations(targets, "references")
    }
    fn semantic_locations(&self, targets: &[Node], relation: &str) -> Vec<SourceSlice> {
        let Some(snapshot) = self.active_semantic_snapshot_id() else {
            return Vec::new();
        };
        let mut slices = Vec::new();
        for target in targets.iter().take(8) {
            if let Some(cached) = self.cached_semantic_slices(target, relation, &snapshot) {
                slices.extend(cached);
                continue;
            }
            let path = self.root.join(&target.file);
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            let Some(line) = text.lines().nth(target.start_line.saturating_sub(1)) else {
                continue;
            };
            let Some(byte) = line.find(short_name(&target.canonical_name)) else {
                continue;
            };
            let character = line[..byte].encode_utf16().count();
            let uri = format!("file://{}", path.display());
            let locations = {
                let Some(client) = &self.ra else { continue };
                let Ok(mut client) = client.lock() else {
                    continue;
                };
                if relation == "implementations" {
                    client.implementations(&uri, target.start_line - 1, character)
                } else {
                    client.references(&uri, target.start_line - 1, character)
                }
            };
            let Some(locations) = locations else {
                continue;
            };
            let mut persisted = 0usize;
            for location in locations.into_iter().take(30) {
                let Some(uri) = location
                    .get("uri")
                    .or_else(|| location.get("targetUri"))
                    .and_then(Value::as_str)
                else {
                    continue;
                };
                let Some(path) = uri.strip_prefix("file://").map(PathBuf::from) else {
                    continue;
                };
                if !path.starts_with(&self.root) {
                    continue;
                }
                let Some(start) = location
                    .get("range")
                    .or_else(|| location.get("targetSelectionRange"))
                    .and_then(|range| range.get("start"))
                    .and_then(|start| start.get("line"))
                    .and_then(Value::as_u64)
                else {
                    continue;
                };
                if let Some(source) = self.node_at_location(&path, start as usize)
                    && source.id != target.id
                {
                    let kind = if relation == "implementations" {
                        "IMPLEMENTS"
                    } else {
                        "REFERENCES"
                    };
                    if self
                        .insert_edge(EdgeRecord {
                            source: source.id,
                            target: target.id,
                            kind,
                            confidence: 1.0,
                            provenance: "RustAnalyzer",
                            revision: &snapshot,
                            metadata: json!({"uri":&uri,"line":start,"relation":relation}),
                        })
                        .is_ok()
                    {
                        persisted += 1;
                    }
                }
                if let Ok(slice) =
                    self.location_slice(&path, start as usize, &target.canonical_name, relation)
                {
                    slices.push(slice);
                }
            }
            let _ = self.db.execute(
                "INSERT OR REPLACE INTO semantic_queries(node_id,relation,semantic_snapshot,result_count,queried_at) VALUES (?1,?2,?3,?4,?5)",
                params![target.id,relation,snapshot,persisted as i64,Utc::now().to_rfc3339()],
            );
        }
        slices
    }

    fn cached_semantic_slices(
        &self,
        target: &Node,
        relation: &str,
        snapshot: &str,
    ) -> Option<Vec<SourceSlice>> {
        let queried = self
            .db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM semantic_queries WHERE node_id=?1 AND relation=?2 AND semantic_snapshot=?3)",
                params![target.id,relation,snapshot],
                |row| row.get::<_, bool>(0),
            )
            .ok()?;
        if !queried {
            return None;
        }
        let kind = if relation == "implementations" {
            "IMPLEMENTS"
        } else {
            "REFERENCES"
        };
        let mut statement = self
            .db
            .prepare("SELECT source.id,source.kind,source.canonical_name,source.crate_name,source.file,source.start_line,source.end_line,source.visibility,source.content_hash FROM edges edge JOIN nodes source ON source.id=edge.src WHERE edge.dst=?1 AND edge.kind=?2 AND edge.provenance='RustAnalyzer' AND edge.revision=?3 ORDER BY source.canonical_name LIMIT 30")
            .ok()?;
        Some(
            statement
                .query_map(params![target.id, kind, snapshot], node_from_row)
                .ok()?
                .filter_map(Result::ok)
                .filter_map(|node| {
                    self.source_slice(&node, "cached rust-analyzer result", relation)
                        .ok()
                })
                .collect(),
        )
    }

    fn node_at_location(&self, path: &Path, zero_based_line: usize) -> Option<Node> {
        self.db
            .query_row(
                "SELECT id,kind,canonical_name,crate_name,file,start_line,end_line,visibility,content_hash FROM nodes WHERE file=?1 AND start_line<=?2 AND end_line>=?2 ORDER BY (end_line-start_line) ASC LIMIT 1",
                params![relative(&self.root,path),(zero_based_line + 1) as i64],
                node_from_row,
            )
            .optional()
            .ok()
            .flatten()
    }
    fn location_slice(
        &self,
        path: &Path,
        line: usize,
        symbol: &str,
        relation: &str,
    ) -> Result<SourceSlice> {
        let text = fs::read_to_string(path)?;
        let lines: Vec<_> = text.lines().collect();
        let start = line.min(lines.len());
        let end = (start + 16).min(lines.len());
        let stale = start == end;
        let mut source = lines[start..end].join("\n");
        if source.len() > MAX_SLICE_BYTES {
            source = trim_text(&source, MAX_SLICE_BYTES);
        }
        Ok(SourceSlice {
            symbol: symbol.to_owned(),
            file: relative(&self.root, path),
            range: if stale { [0, 0] } else { [start + 1, end] },
            content_hash: format!("b3:{}", blake3::hash(source.as_bytes()).to_hex()),
            reason: format!("rust-analyzer resolved {relation}"),
            semantic_relationship: relation.to_ascii_uppercase(),
            provenance: "RustAnalyzer".into(),
            confidence: if stale { 0.5 } else { 1.0 },
            stale,
            source,
        })
    }
    fn use_cases_for(&self, tests: &[Node]) -> Result<Vec<Value>> {
        let mut output = Vec::new();
        for test in tests {
            let mut statement = self.db.prepare(
                "SELECT u.id, u.name, u.description FROM use_cases u \
                 JOIN use_case_nodes links ON links.use_case_id=u.id WHERE links.node_id=?1",
            )?;
            let rows = statement.query_map([test.id], |row| {
                Ok(json!({"id":row.get::<_, String>(0)?,"name":row.get::<_, String>(1)?,"description":row.get::<_, String>(2)?,"test_symbol":test.canonical_name}))
            })?;
            output.extend(rows.filter_map(Result::ok));
        }
        Ok(output)
    }
    fn source_slice(&self, node: &Node, reason: &str, relation: &str) -> Result<SourceSlice> {
        let path = self.root.join(&node.file);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(SourceSlice {
                    symbol: node.canonical_name.clone(),
                    file: node.file.clone(),
                    range: [0, 0],
                    content_hash: node.content_hash.clone(),
                    reason: format!("{reason}; indexed source is unavailable in the live worktree"),
                    semantic_relationship: relation.into(),
                    provenance: "StaticIndexStale".into(),
                    confidence: 0.25,
                    stale: true,
                    source: String::new(),
                });
            }
            Err(error) => return Err(error.into()),
        };
        let lines: Vec<_> = text.lines().collect();
        let indexed_start = node.start_line.saturating_sub(1);
        let end = node.end_line.min(lines.len());
        let start = indexed_start.min(end);
        let stale = indexed_start >= lines.len() || node.end_line > lines.len();
        let mut source = lines[start..end].join("\n");
        if source.len() > MAX_SLICE_BYTES {
            source = trim_text(&source, MAX_SLICE_BYTES)
        }
        Ok(SourceSlice {
            symbol: node.canonical_name.clone(),
            file: node.file.clone(),
            range: if start == end {
                [0, 0]
            } else {
                [start + 1, end]
            },
            content_hash: node.content_hash.clone(),
            reason: if stale {
                format!("{reason}; indexed line range exceeds the live file")
            } else {
                reason.into()
            },
            semantic_relationship: relation.into(),
            provenance: if stale {
                "StaticIndexStale"
            } else {
                "StaticIndex"
            }
            .into(),
            confidence: if stale { 0.4 } else { 0.9 },
            stale,
            source,
        })
    }
    fn decisions_for(&self, text: &str) -> Result<Vec<Value>> {
        let hits = self.search_hits(&terms(text), Some("decision"), 20)?;
        hits.into_iter()
            .filter_map(|hit| hit["entity_id"].as_str().map(str::to_owned))
            .map(|id| self.decision_resource(&id))
            .collect()
    }
    fn decision_resource(&self, id: &str) -> Result<Value> {
        Ok(self.db.query_row("SELECT id,status,title,rationale,applies_to,consequences,supersedes,revision,created_at FROM decisions WHERE id=?1",[id],|r|Ok(json!({"id":r.get::<_,String>(0)?,"status":r.get::<_,String>(1)?,"title":r.get::<_,String>(2)?,"rationale":r.get::<_,String>(3)?,"applies_to":serde_json::from_str::<Value>(&r.get::<_,String>(4)?).unwrap_or(Value::Null),"consequences":serde_json::from_str::<Value>(&r.get::<_,String>(5)?).unwrap_or(Value::Null),"supersedes":r.get::<_,Option<String>>(6)?,"revision":r.get::<_,String>(7)?,"created_at":r.get::<_,String>(8)?})))?)
    }
    fn decision_conflicts(&self, diff: &str) -> Result<Vec<Value>> {
        let mut s = self.db.prepare(
            "SELECT id,title,applies_to,consequences FROM decisions WHERE status='accepted'",
        )?;
        Ok(s.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?
        .filter_map(Result::ok)
        .filter_map(|(id, title, scope, cs)| {
            let scope: Vec<String> = serde_json::from_str(&scope).unwrap_or_default();
            let cs: Vec<String> = serde_json::from_str(&cs).unwrap_or_default();
            let diff = diff.to_lowercase();
            scope
                .iter()
                .any(|target| diff.contains(&target.to_lowercase()))
                .then(|| json!({"decision":id,"title":title,"classification":"review-required","matched_scope":scope,"consequences":cs,"note":"Scope overlap is evidence for review, not proof of a violation."}))
        })
        .collect())
    }
}

fn actionable_marker_text<'a>(path: &Path, line: &'a str) -> Option<&'a str> {
    let trimmed = line.trim_start();
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("rs") => trimmed
            .strip_prefix("//")
            .or_else(|| trimmed.strip_prefix("/*"))
            .or_else(|| trimmed.strip_prefix('*')),
        Some("md" | "txt") => {
            if trimmed.starts_with("<!--")
                || trimmed.starts_with("TODO")
                || trimmed.starts_with("FIXME")
                || trimmed.starts_with("XXX")
                || trimmed.starts_with("- [ ]")
            {
                Some(trimmed)
            } else {
                None
            }
        }
        Some("toml" | "yaml" | "yml") => trimmed.strip_prefix('#'),
        Some("json") => trimmed.strip_prefix("//"),
        _ => None,
    }
}

const SCHEMA: &str = "
PRAGMA journal_mode=WAL;
PRAGMA foreign_keys=ON;
CREATE TABLE IF NOT EXISTS metadata(key TEXT PRIMARY KEY,value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS packages(name TEXT PRIMARY KEY,manifest_path TEXT,metadata TEXT,revision TEXT);
CREATE TABLE IF NOT EXISTS package_dependencies(source TEXT,target TEXT,context_json TEXT,revision TEXT,PRIMARY KEY(source,target,context_json));
CREATE TABLE IF NOT EXISTS package_targets(package TEXT NOT NULL,target_name TEXT NOT NULL,kind TEXT NOT NULL,crate_types TEXT NOT NULL,required_features TEXT NOT NULL,edition TEXT NOT NULL,doc INTEGER NOT NULL,doctest INTEGER NOT NULL,test INTEGER NOT NULL,bench INTEGER NOT NULL,revision TEXT NOT NULL,PRIMARY KEY(package,target_name));
CREATE TABLE IF NOT EXISTS package_features(package TEXT NOT NULL,feature TEXT NOT NULL,dependencies TEXT NOT NULL,revision TEXT NOT NULL,PRIMARY KEY(package,feature));
CREATE TABLE IF NOT EXISTS nodes(id INTEGER PRIMARY KEY,kind TEXT NOT NULL,canonical_name TEXT NOT NULL,crate_name TEXT,file TEXT NOT NULL,start_line INTEGER,end_line INTEGER,visibility TEXT,content_hash TEXT,metadata TEXT);
CREATE INDEX IF NOT EXISTS nodes_name ON nodes(canonical_name);
CREATE INDEX IF NOT EXISTS nodes_file ON nodes(file);
CREATE TABLE IF NOT EXISTS edges(id INTEGER PRIMARY KEY,src INTEGER REFERENCES nodes(id) ON DELETE CASCADE,dst INTEGER REFERENCES nodes(id) ON DELETE CASCADE,kind TEXT,context_json TEXT,confidence REAL,provenance TEXT,revision TEXT,metadata TEXT);
CREATE INDEX IF NOT EXISTS edges_src ON edges(src);
CREATE INDEX IF NOT EXISTS edges_dst ON edges(dst);
CREATE UNIQUE INDEX IF NOT EXISTS edges_identity ON edges(src,dst,kind,provenance);
CREATE INDEX IF NOT EXISTS edges_semantic ON edges(provenance,revision,kind);
CREATE TABLE IF NOT EXISTS unresolved_references(id INTEGER PRIMARY KEY,source INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,file TEXT NOT NULL,line INTEGER NOT NULL,name TEXT NOT NULL,kind TEXT NOT NULL,candidate_count INTEGER NOT NULL,reason TEXT NOT NULL,revision TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS unresolved_references_source ON unresolved_references(source);
CREATE TABLE IF NOT EXISTS semantic_snapshots(id TEXT PRIMARY KEY,workspace_digest TEXT NOT NULL,cargo_lock_hash TEXT,target_triple TEXT NOT NULL,feature_profile TEXT NOT NULL,build_environment_fingerprint TEXT NOT NULL,rust_analyzer_version TEXT,created_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS index_generations(id INTEGER PRIMARY KEY AUTOINCREMENT,workspace_digest TEXT NOT NULL,semantic_snapshot TEXT NOT NULL REFERENCES semantic_snapshots(id),status TEXT NOT NULL,created_at TEXT NOT NULL,published_at TEXT);
CREATE INDEX IF NOT EXISTS index_generations_status ON index_generations(status);
CREATE TABLE IF NOT EXISTS semantic_queries(node_id INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,relation TEXT NOT NULL,semantic_snapshot TEXT NOT NULL REFERENCES semantic_snapshots(id),result_count INTEGER NOT NULL,queried_at TEXT NOT NULL,PRIMARY KEY(node_id,relation,semantic_snapshot));
CREATE TABLE IF NOT EXISTS symbol_embeddings(node_id INTEGER PRIMARY KEY REFERENCES nodes(id) ON DELETE CASCADE,model TEXT NOT NULL,dimensions INTEGER NOT NULL,vector BLOB NOT NULL,content_hash TEXT NOT NULL,semantic_snapshot TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS symbol_embeddings_snapshot ON symbol_embeddings(model,semantic_snapshot);
CREATE TABLE IF NOT EXISTS decisions(id TEXT PRIMARY KEY,sequence INTEGER,status TEXT,title TEXT,rationale TEXT,applies_to TEXT,consequences TEXT,supersedes TEXT,revision TEXT,created_at TEXT);
CREATE TABLE IF NOT EXISTS decision_targets(decision_id TEXT NOT NULL REFERENCES decisions(id) ON DELETE CASCADE,node_id INTEGER REFERENCES nodes(id) ON DELETE SET NULL,target_ref TEXT NOT NULL,PRIMARY KEY(decision_id,target_ref));
CREATE TABLE IF NOT EXISTS steerings(id TEXT PRIMARY KEY,sequence INTEGER NOT NULL,status TEXT NOT NULL,priority TEXT NOT NULL,title TEXT NOT NULL,instruction TEXT NOT NULL,scope TEXT NOT NULL,expires_at TEXT,revision TEXT NOT NULL,created_at TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS documents(id INTEGER PRIMARY KEY,kind TEXT,path TEXT,text TEXT,revision TEXT);
CREATE VIRTUAL TABLE IF NOT EXISTS search_index USING fts5(entity_type UNINDEXED,entity_id UNINDEXED,title,path,body,tokenize='unicode61 remove_diacritics 2 tokenchars ''_''');
CREATE TABLE IF NOT EXISTS use_cases(id TEXT PRIMARY KEY,name TEXT,description TEXT,provenance TEXT NOT NULL,confidence REAL NOT NULL,revision TEXT);
CREATE TABLE IF NOT EXISTS use_case_nodes(use_case_id TEXT REFERENCES use_cases(id) ON DELETE CASCADE,node_id INTEGER REFERENCES nodes(id) ON DELETE CASCADE,relationship TEXT,provenance TEXT NOT NULL,confidence REAL NOT NULL,revision TEXT,PRIMARY KEY(use_case_id,node_id));
CREATE TABLE IF NOT EXISTS commits(hash TEXT PRIMARY KEY,timestamp TEXT,author TEXT,subject TEXT,revision TEXT);
CREATE TABLE IF NOT EXISTS commit_files(commit_hash TEXT NOT NULL REFERENCES commits(hash) ON DELETE CASCADE,file TEXT NOT NULL,PRIMARY KEY(commit_hash,file));
CREATE INDEX IF NOT EXISTS commit_files_file ON commit_files(file);
CREATE TABLE IF NOT EXISTS co_changes(node_a TEXT,node_b TEXT,count INTEGER,score REAL,PRIMARY KEY(node_a,node_b));
CREATE INDEX IF NOT EXISTS co_changes_pair ON co_changes(node_a,node_b);
CREATE TABLE IF NOT EXISTS lifecycle_edges(id INTEGER PRIMARY KEY,legacy_node INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,canonical_node INTEGER REFERENCES nodes(id) ON DELETE SET NULL,kind TEXT NOT NULL,evidence TEXT NOT NULL,provenance TEXT NOT NULL,confidence REAL NOT NULL,revision TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS lifecycle_legacy ON lifecycle_edges(legacy_node);
CREATE INDEX IF NOT EXISTS lifecycle_canonical ON lifecycle_edges(canonical_node);
CREATE TABLE IF NOT EXISTS work_items(id TEXT PRIMARY KEY,title TEXT NOT NULL,status TEXT NOT NULL,priority TEXT NOT NULL,kind TEXT NOT NULL,scope_json TEXT NOT NULL,evidence_json TEXT NOT NULL,depends_json TEXT NOT NULL,blocked_json TEXT NOT NULL,acceptance_json TEXT NOT NULL,verification_json TEXT NOT NULL,discovered_from TEXT NOT NULL,provenance TEXT NOT NULL,confidence REAL NOT NULL,last_validated_snapshot TEXT NOT NULL,created_at TEXT NOT NULL,updated_at TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS work_status_priority ON work_items(status,priority);
CREATE TABLE IF NOT EXISTS change_contexts(id TEXT PRIMARY KEY,intent TEXT,payload TEXT,revision TEXT,created_at TEXT);
CREATE TABLE IF NOT EXISTS input_state(path TEXT PRIMARY KEY,kind TEXT NOT NULL,content_hash TEXT NOT NULL,revision TEXT NOT NULL);
";

fn initialize_schema(db: &Connection) -> Result<()> {
    db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
    let metadata_exists: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='metadata')",
        [],
        |row| row.get(0),
    )?;
    if metadata_exists {
        let version: Option<String> = db
            .query_row(
                "SELECT value FROM metadata WHERE key='schema_version'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        let stored = version.as_deref().unwrap_or("0");
        let stored_number = stored.parse::<u32>().unwrap_or(0);
        let current_number = SCHEMA_VERSION.parse::<u32>().unwrap_or(0);
        // A database written by a newer Crusty is never rewritten. Downgrading
        // used to fall through to the rebuild batch below and destroy every
        // human-authored decision, problem record, and quality constraint.
        ensure!(
            stored_number <= current_number,
            "index schema version {stored} was written by a newer Crusty than this build (schema {SCHEMA_VERSION}); \
             upgrade Crusty or point --workspace at a different repository. No data was modified."
        );
        if stored != SCHEMA_VERSION && !matches!(stored, "6" | "5" | "4") {
            // Derived data is rebuildable, so an incompatible older generation is
            // discarded and reindexed. Human-authored tables are deliberately
            // absent from this batch: decisions, steerings, work_items, the
            // problem records, and the quality/validation lifecycle survive.
            db.execute_batch("PRAGMA foreign_keys=OFF;
                DROP TABLE IF EXISTS search_index;
                DROP TABLE IF EXISTS symbol_embeddings; DROP TABLE IF EXISTS semantic_queries; DROP TABLE IF EXISTS index_generations; DROP TABLE IF EXISTS semantic_snapshots;
                DROP TABLE IF EXISTS input_state; DROP TABLE IF EXISTS change_contexts;
                DROP TABLE IF EXISTS co_changes; DROP TABLE IF EXISTS commit_files; DROP TABLE IF EXISTS commits;
                DROP TABLE IF EXISTS use_case_nodes; DROP TABLE IF EXISTS use_cases; DROP TABLE IF EXISTS documents;
                DROP TABLE IF EXISTS lifecycle_edges;
                DROP TABLE IF EXISTS edges;
                DROP TABLE IF EXISTS unresolved_references;
                DROP TABLE IF EXISTS nodes; DROP TABLE IF EXISTS package_features; DROP TABLE IF EXISTS package_targets; DROP TABLE IF EXISTS package_dependencies; DROP TABLE IF EXISTS packages;
                PRAGMA foreign_keys=ON;")?;
        }
    }
    // `INSERT OR IGNORE INTO edges` could never ignore anything, because the
    // table carried no uniqueness constraint. Existing databases therefore hold
    // duplicate edges that must be collapsed before the index below can be
    // created; the surviving row is the lowest id of each identity group.
    let edges_exist: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='edges')",
        [],
        |row| row.get(0),
    )?;
    if edges_exist {
        db.execute(
            "DELETE FROM edges WHERE id NOT IN (SELECT MIN(id) FROM edges GROUP BY src,dst,kind,provenance)",
            [],
        )?;
    }
    db.execute_batch(SCHEMA)?;
    quality::initialize(db)?;
    db.execute(
        "INSERT OR REPLACE INTO metadata(key, value) VALUES ('schema_version', ?1)",
        [SCHEMA_VERSION],
    )?;
    Ok(())
}

/// Collects every repository path a unified diff touches.
///
/// This replaces a single `^\+\+\+ b/(.+)$` regex that missed deletions
/// (`+++ /dev/null`), pure renames (no `+++` line at all), and `--no-prefix`
/// output, kept CRLF carriage returns and POSIX `\t<timestamp>` suffixes in the
/// captured path, and matched `+++ b/...` text appearing inside added content.
/// Every one of those failures was silent: the file simply never appeared in
/// `changed_files`, and validation reported the change as clean.
fn changed_files_in_diff(diff: &str) -> BTreeSet<String> {
    let mut files = BTreeSet::new();
    // Header directives are only meaningful outside a hunk body, where a line
    // beginning `+++` is added content rather than a file header.
    let mut in_hunk = false;
    let mut previous_old_path: Option<String> = None;
    for line in diff.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.starts_with("diff --git ") || line.starts_with("diff --cc ") {
            in_hunk = false;
            previous_old_path = None;
            continue;
        }
        if line.starts_with("@@") {
            in_hunk = true;
            continue;
        }
        if in_hunk {
            continue;
        }
        if let Some(rest) = line.strip_prefix("rename from ") {
            files.extend(diff_path(rest));
        } else if let Some(rest) = line.strip_prefix("rename to ") {
            files.extend(diff_path(rest));
        } else if let Some(rest) = line.strip_prefix("--- ") {
            previous_old_path = diff_path(rest);
        } else if let Some(rest) = line.strip_prefix("+++ ") {
            match diff_path(rest) {
                // `+++ /dev/null` marks a deletion; the path lives on the
                // preceding `---` line.
                None => files.extend(previous_old_path.take()),
                Some(path) => {
                    files.insert(path);
                    previous_old_path = None;
                }
            }
        }
    }
    files
}

/// Normalises one path out of a diff file header, or `None` for `/dev/null`.
fn diff_path(raw: &str) -> Option<String> {
    // POSIX diff appends a tab and a timestamp to the header path.
    let path = raw.split('\t').next().unwrap_or(raw).trim_end();
    // Git quotes paths containing unusual bytes; leave those to the caller's
    // exact-match logic rather than mis-unescaping them.
    let path = path.trim_matches('"');
    if path.is_empty() || path == "/dev/null" {
        return None;
    }
    // `a/` and `b/` are git's default prefixes; `--no-prefix` output has none.
    let path = path
        .strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .unwrap_or(path);
    if path.is_empty() {
        None
    } else {
        Some(path.to_owned())
    }
}

/// Renders a symbol as an actionable location.
///
/// `start_line`/`end_line` were indexed but dropped from every search and
/// context response, so an agent got a filename and had to grep for the line.
fn node_location(node: &Node) -> Value {
    json!({
        "id": node.id,
        "symbol": node.canonical_name,
        "kind": node.kind,
        "file": node.file,
        "line": node.start_line,
        "end_line": node.end_line,
        "location": format!("{}:{}", node.file, node.start_line),
        "crate": node.crate_name,
        "visibility": node.visibility,
    })
}

fn node_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Node> {
    Ok(Node {
        id: row.get(0)?,
        kind: row.get(1)?,
        canonical_name: row.get(2)?,
        crate_name: row.get(3)?,
        file: row.get(4)?,
        start_line: row.get(5)?,
        end_line: row.get(6)?,
        visibility: row.get(7)?,
        content_hash: row.get(8)?,
    })
}
fn work_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    let array = |index| -> rusqlite::Result<Value> {
        Ok(serde_json::from_str::<Value>(&row.get::<_, String>(index)?)
            .unwrap_or_else(|_| json!([])))
    };
    Ok(
        json!({"id":row.get::<_,String>(0)?,"title":row.get::<_,String>(1)?,"status":row.get::<_,String>(2)?,"priority":row.get::<_,String>(3)?,"kind":row.get::<_,String>(4)?,"scope":array(5)?,"evidence":array(6)?,"depends_on":array(7)?,"blocked_by":array(8)?,"acceptance_criteria":array(9)?,"verification":array(10)?,"discovered_from":row.get::<_,String>(11)?,"provenance":row.get::<_,String>(12)?,"confidence":row.get::<_,f64>(13)?,"last_validated_snapshot":row.get::<_,String>(14)?,"updated_at":row.get::<_,String>(15)?}),
    )
}
fn trim_text(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}
fn rust_files(root: &Path) -> Vec<PathBuf> {
    workspace_files(root)
        .into_iter()
        .filter(|path| path.extension().is_some_and(|extension| extension == "rs"))
        .collect()
}
fn all_text_files(root: &Path) -> Vec<PathBuf> {
    workspace_files(root)
        .into_iter()
        .filter(|path| input_kind(path).is_some())
        .collect()
}
fn index_inputs(root: &Path) -> BTreeMap<String, (String, String)> {
    let blob_ids = git_tracked_blob_ids(root);
    let dirty_paths = git_dirty_paths(root);
    workspace_files(root)
        .into_iter()
        .filter_map(|path| {
            let kind = input_kind(&path)?;
            let relative_path = relative(root, &path);
            let hash = blob_ids
                .get(&relative_path)
                .filter(|_| !dirty_paths.contains(&relative_path))
                .map(|oid| format!("git:{oid}"))
                .or_else(|| {
                    fs::read(&path)
                        .ok()
                        .map(|bytes| format!("b3:{}", blake3::hash(&bytes).to_hex()))
                })?;
            Some((relative_path, (kind.to_owned(), hash)))
        })
        .collect()
}

fn input_kind(path: &Path) -> Option<&'static str> {
    let name = path.file_name()?.to_str()?;
    if matches!(
        name,
        "Cargo.toml"
            | "Cargo.lock"
            | "rust-toolchain"
            | "rust-toolchain.toml"
            | "config"
            | "config.toml"
            | "build.rs"
    ) {
        Some("cargo")
    } else if path.extension().is_some_and(|extension| extension == "rs") {
        Some("source")
    } else {
        match path.extension()?.to_string_lossy().as_ref() {
            "md" | "txt" => Some("document"),
            "ui" | "blp" | "xml" => Some("contract"),
            "toml" | "yaml" | "yml" | "json" | "css" | "scss" | "desktop" | "service" => {
                Some("configuration")
            }
            _ => None,
        }
    }
}

fn watch_path_relevant(root: &Path, path: &Path) -> bool {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    let Ok(relative_path) = absolute.strip_prefix(root) else {
        return false;
    };
    !relative_path.starts_with(INDEX_DIRECTORY)
        && !relative_path.starts_with(".git")
        && !relative_path.starts_with("target")
        && (absolute.is_dir() || input_kind(&absolute).is_some() || !absolute.exists())
}

fn inputs_from_watch_paths(
    root: &Path,
    previous: &BTreeMap<String, (String, String)>,
    paths: &BTreeSet<PathBuf>,
) -> BTreeMap<String, (String, String)> {
    let mut inputs = previous.clone();
    for path in paths {
        let absolute = if path.is_absolute() {
            path.clone()
        } else {
            root.join(path)
        };
        let Ok(relative_path) = absolute.strip_prefix(root) else {
            continue;
        };
        if relative_path.starts_with(INDEX_DIRECTORY)
            || relative_path.starts_with(".git")
            || relative_path.starts_with("target")
        {
            continue;
        }
        let relative_path = relative(root, &absolute);
        if absolute.is_dir() {
            for nested in workspace_files(&absolute) {
                let Some(kind) = input_kind(&nested) else {
                    continue;
                };
                if let Ok(bytes) = fs::read(&nested) {
                    inputs.insert(
                        relative(root, &nested),
                        (kind.into(), format!("b3:{}", blake3::hash(&bytes).to_hex())),
                    );
                }
            }
        } else if absolute.exists() {
            let Some(kind) = input_kind(&absolute) else {
                continue;
            };
            if let Ok(bytes) = fs::read(&absolute) {
                inputs.insert(
                    relative_path,
                    (kind.into(), format!("b3:{}", blake3::hash(&bytes).to_hex())),
                );
            }
        } else {
            inputs.retain(|candidate, _| {
                candidate != &relative_path
                    && !Path::new(candidate).starts_with(Path::new(&relative_path))
            });
        }
    }
    inputs
}

fn changed_inputs_still_match(
    root: &Path,
    inputs: &BTreeMap<String, (String, String)>,
    changed: &[(String, String)],
) -> bool {
    changed.iter().all(|(path, _)| {
        let absolute = root.join(path);
        let Some((_, expected)) = inputs.get(path) else {
            return !absolute.exists();
        };
        if let Some(expected) = expected.strip_prefix("b3:") {
            return fs::read(&absolute)
                .ok()
                .is_some_and(|bytes| blake3::hash(&bytes).to_hex().as_str() == expected);
        }
        if let Some(expected) = expected.strip_prefix("git:") {
            return command_text(root, &["hash-object", "--", path])
                .is_some_and(|actual| actual == expected);
        }
        false
    })
}

fn git_tracked_blob_ids(root: &Path) -> HashMap<String, String> {
    let Some(output) = git_output(root, &["ls-files", "-s", "-z"]) else {
        return HashMap::new();
    };
    output
        .split(|byte| *byte == 0)
        .filter_map(|record| {
            let record = String::from_utf8_lossy(record);
            let (metadata, path) = record.split_once('\t')?;
            let mut fields = metadata.split_whitespace();
            let _mode = fields.next()?;
            let oid = fields.next()?;
            let stage = fields.next()?;
            (stage == "0").then(|| (path.to_owned(), oid.to_owned()))
        })
        .collect()
}

fn git_dirty_paths(root: &Path) -> BTreeSet<String> {
    let Some(output) = git_output(
        root,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    ) else {
        return BTreeSet::new();
    };
    let mut dirty = BTreeSet::new();
    let mut expect_rename_source = false;
    for record in output
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        let record = String::from_utf8_lossy(record);
        if expect_rename_source {
            dirty.insert(record.to_string());
            expect_rename_source = false;
            continue;
        }
        if record.len() < 4 {
            continue;
        }
        let status = &record[..2];
        dirty.insert(record[3..].to_owned());
        expect_rename_source = status.contains('R') || status.contains('C');
    }
    dirty
}

fn workspace_files(root: &Path) -> Vec<PathBuf> {
    if let Some(output) = git_output(
        root,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
    ) {
        return output
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            .map(|path| root.join(String::from_utf8_lossy(path).as_ref()))
            .filter(|path| path.is_file())
            .collect();
    }
    WalkDir::new(root)
        .into_iter()
        .filter_entry(|entry| {
            let name = entry.file_name().to_string_lossy();
            !matches!(name.as_ref(), "target" | ".git" | INDEX_DIRECTORY)
        })
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| entry.into_path())
        .collect()
}
fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}
fn command_text(root: &Path, args: &[&str]) -> Option<String> {
    git_output(root, args).map(|output| String::from_utf8_lossy(&output).trim().to_owned())
}

fn rustc_host_triple() -> Option<String> {
    let output = Command::new("rustc").arg("-vV").output().ok()?;
    output.status.success().then_some(())?;
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix("host: ").map(str::to_owned))
}

fn rust_analyzer_version(program: &Path) -> Option<String> {
    let output = Command::new(program).arg("--version").output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn git_output(root: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    output.status.success().then_some(output.stdout)
}
fn run_git_with_index(root: &Path, index: &Path, args: &[&str]) -> Result<String> {
    run_git_with_index_env(root, index, args)
}
fn run_git_with_index_env(root: &Path, index: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_INDEX_FILE", index)
        .env("GIT_AUTHOR_NAME", "Codex Checkpoint")
        .env("GIT_AUTHOR_EMAIL", "codex-checkpoint@localhost")
        .env("GIT_COMMITTER_NAME", "Codex Checkpoint")
        .env("GIT_COMMITTER_EMAIL", "codex-checkpoint@localhost")
        .output()
        .with_context(|| format!("running git {}", args.join(" ")))?;
    ensure!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}
fn ref_component(value: &str) -> String {
    let value = slug(value);
    let value = if value.is_empty() {
        "checkpoint".to_owned()
    } else {
        value
    };
    value.chars().take(48).collect()
}
fn validate_checkpoint_ref(reference: &str) -> Result<()> {
    ensure!(
        reference.starts_with(&format!("{CHECKPOINT_REF_PREFIX}/"))
            && reference
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || "/-_.".contains(character))
            // `.` is legal in a ref name, but `..` walks out of the checkpoint
            // namespace: `refs/codex/checkpoints/../../HEAD` otherwise passed
            // validation and was handed straight to `git`.
            && !reference.split('/').any(|segment| segment == ".." || segment == "."),
        "invalid checkpoint reference"
    );
    Ok(())
}
fn cargo_packages(root: &Path) -> Vec<(String, String, String)> {
    cargo_metadata(root)
        .map(|metadata| cargo_packages_from_metadata(&metadata))
        .unwrap_or_default()
}
fn cargo_metadata(root: &Path) -> Result<Value> {
    let output = Command::new("cargo")
        .args(["metadata", "--format-version=1"])
        .current_dir(root)
        .output()
        .context("running cargo metadata")?;
    ensure!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    serde_json::from_slice(&output.stdout).context("parsing cargo metadata")
}
fn cargo_packages_from_metadata(metadata: &Value) -> Vec<(String, String, String)> {
    let workspace_members = workspace_member_ids(metadata);
    metadata
        .get("packages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter(|package| {
            package
                .get("id")
                .and_then(Value::as_str)
                .is_some_and(|id| workspace_members.contains(id))
        })
        .filter_map(|p| {
            Some((
                p.get("name")?.as_str()?.to_owned(),
                p.get("manifest_path")?.as_str()?.to_owned(),
                p.to_string(),
            ))
        })
        .collect()
}
fn workspace_member_ids(metadata: &Value) -> BTreeSet<String> {
    metadata
        .get("workspace_members")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}
fn cargo_dependency_edges_from_metadata(metadata: &Value) -> Vec<(String, String, String)> {
    let names: HashMap<String, String> = metadata
        .get("packages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|package| {
            Some((
                package.get("id")?.as_str()?.to_owned(),
                package.get("name")?.as_str()?.to_owned(),
            ))
        })
        .collect();
    let workspace_members = workspace_member_ids(metadata);
    metadata
        .get("resolve")
        .and_then(|resolve| resolve.get("nodes"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|node| {
            node.get("id")
                .and_then(Value::as_str)
                .is_some_and(|id| workspace_members.contains(id))
        })
        .flat_map(|node| {
            let Some(source) = node
                .get("id")
                .and_then(Value::as_str)
                .and_then(|id| names.get(id))
                .cloned()
            else {
                return Vec::new();
            };
            let names = names.clone();
            let features = node.get("features").cloned().unwrap_or_else(|| json!([]));
            node.get("deps")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(move |dependency| {
                    let target = names.get(dependency.get("pkg")?.as_str()?)?.clone();
                    let context = json!({
                        "package": source,
                        "feature_set": features,
                        "cfg_context": dependency.get("dep_kinds").cloned().unwrap_or_else(|| json!([])),
                        "target_triple": null,
                        "dependency": dependency,
                    });
                    Some((source.clone(), target, context.to_string()))
                })
                .collect::<Vec<_>>()
        })
        .collect()
}
fn crate_for_file(packages: &[(String, String, String)], file: &Path) -> Option<String> {
    packages
        .iter()
        .find(|(_, manifest, _)| {
            file.starts_with(Path::new(manifest).parent().unwrap_or(Path::new("")))
        })
        .map(|p| p.0.clone())
}

#[derive(Default)]
struct SyntaxReferenceVisitor {
    references: BTreeSet<SyntaxReference>,
}

impl SyntaxReferenceVisitor {
    fn record(&mut self, path: &syn::Path, line: usize, kind: &'static str) {
        let path = path
            .segments
            .iter()
            .map(|segment| segment.ident.to_string())
            .collect::<Vec<_>>();
        if !path.is_empty() {
            self.references.insert(SyntaxReference {
                path,
                line: line.max(1),
                kind,
            });
        }
    }
}

impl<'ast> Visit<'ast> for SyntaxReferenceVisitor {
    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        if let syn::Expr::Path(path) = call.func.as_ref() {
            self.record(&path.path, path.span().start().line, "CALLS_DIRECT");
        } else {
            self.visit_expr(call.func.as_ref());
        }
        for argument in &call.args {
            self.visit_expr(argument);
        }
    }

    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        self.references.insert(SyntaxReference {
            path: vec![call.method.to_string()],
            line: call.method.span().start().line.max(1),
            kind: "MAY_CALL_DYNAMIC",
        });
        self.visit_expr(call.receiver.as_ref());
        for argument in &call.args {
            self.visit_expr(argument);
        }
    }

    fn visit_expr_path(&mut self, path: &'ast syn::ExprPath) {
        self.record(&path.path, path.span().start().line, "REFERENCES");
        syn::visit::visit_expr_path(self, path);
    }

    fn visit_type_path(&mut self, path: &'ast syn::TypePath) {
        self.record(&path.path, path.span().start().line, "REFERENCES");
        syn::visit::visit_type_path(self, path);
    }
}

fn syntax_references(text: &str) -> Option<Vec<SyntaxReference>> {
    let file = parse_rust_file(text)?;
    let mut visitor = SyntaxReferenceVisitor::default();
    visitor.visit_file(&file);
    Some(visitor.references.into_iter().collect())
}

fn resolve_syntax_reference<'a>(
    source: &Node,
    path: &[String],
    candidates: &[&'a Node],
) -> Option<(&'a Node, f64, &'static str)> {
    let candidates = candidates
        .iter()
        .copied()
        .filter(|candidate| candidate.id != source.id)
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        return None;
    }
    let normalized = path
        .iter()
        .skip_while(|segment| matches!(segment.as_str(), "crate" | "self" | "super"))
        .cloned()
        .collect::<Vec<_>>();
    if normalized.len() > 1 {
        let suffix = format!("::{}", normalized.join("::"));
        let qualified = candidates
            .iter()
            .copied()
            .filter(|candidate| candidate.canonical_name.ends_with(&suffix))
            .collect::<Vec<_>>();
        if qualified.len() == 1 {
            return Some((qualified[0], 0.9, "qualified-path"));
        }
        return None;
    }
    let local = candidates
        .iter()
        .copied()
        .filter(|candidate| candidate.file == source.file)
        .collect::<Vec<_>>();
    if local.len() == 1 {
        return Some((local[0], 0.8, "unique-in-file"));
    }
    if candidates.len() == 1 {
        return Some((candidates[0], 0.75, "unique-in-workspace"));
    }
    None
}

fn parse_rust_symbols(text: &str) -> Option<Vec<ParsedSymbol>> {
    let file = parse_rust_file(text)?;
    let mut output = Vec::new();
    collect_syn_items(&file.items, &mut Vec::new(), &mut output);
    Some(output)
}

fn collect_syn_items(items: &[syn::Item], scope: &mut Vec<String>, output: &mut Vec<ParsedSymbol>) {
    for item in items {
        match item {
            syn::Item::Fn(value) => add_syn_symbol(
                output,
                "fn",
                &value.sig.ident,
                scope,
                &value.vis,
                value.span(),
            ),
            syn::Item::Struct(value) => add_syn_symbol(
                output,
                "struct",
                &value.ident,
                scope,
                &value.vis,
                value.span(),
            ),
            syn::Item::Enum(value) => add_syn_symbol(
                output,
                "enum",
                &value.ident,
                scope,
                &value.vis,
                value.span(),
            ),
            syn::Item::Type(value) => add_syn_symbol(
                output,
                "type",
                &value.ident,
                scope,
                &value.vis,
                value.span(),
            ),
            syn::Item::Const(value) => add_syn_symbol(
                output,
                "const",
                &value.ident,
                scope,
                &value.vis,
                value.span(),
            ),
            syn::Item::Static(value) => add_syn_symbol(
                output,
                "static",
                &value.ident,
                scope,
                &value.vis,
                value.span(),
            ),
            syn::Item::Mod(value) => {
                add_syn_symbol(output, "mod", &value.ident, scope, &value.vis, value.span());
                if let Some((_, nested)) = &value.content {
                    scope.push(value.ident.to_string());
                    collect_syn_items(nested, scope, output);
                    scope.pop();
                }
            }
            syn::Item::Trait(value) => {
                add_syn_symbol(
                    output,
                    "trait",
                    &value.ident,
                    scope,
                    &value.vis,
                    value.span(),
                );
                scope.push(value.ident.to_string());
                for child in &value.items {
                    match child {
                        syn::TraitItem::Fn(method) => add_syn_symbol(
                            output,
                            "fn",
                            &method.sig.ident,
                            scope,
                            &value.vis,
                            method.span(),
                        ),
                        syn::TraitItem::Const(constant) => add_syn_symbol(
                            output,
                            "const",
                            &constant.ident,
                            scope,
                            &value.vis,
                            constant.span(),
                        ),
                        syn::TraitItem::Type(associated) => add_syn_symbol(
                            output,
                            "type",
                            &associated.ident,
                            scope,
                            &value.vis,
                            associated.span(),
                        ),
                        _ => {}
                    }
                }
                scope.pop();
            }
            syn::Item::Impl(value) => {
                scope.push(impl_type_name(&value.self_ty));
                for child in &value.items {
                    match child {
                        syn::ImplItem::Fn(method) => add_syn_symbol(
                            output,
                            "fn",
                            &method.sig.ident,
                            scope,
                            &method.vis,
                            method.span(),
                        ),
                        syn::ImplItem::Const(constant) => add_syn_symbol(
                            output,
                            "const",
                            &constant.ident,
                            scope,
                            &constant.vis,
                            constant.span(),
                        ),
                        syn::ImplItem::Type(associated) => add_syn_symbol(
                            output,
                            "type",
                            &associated.ident,
                            scope,
                            &associated.vis,
                            associated.span(),
                        ),
                        _ => {}
                    }
                }
                scope.pop();
            }
            syn::Item::Macro(value) => {
                if let Some(ident) = &value.ident {
                    add_syn_symbol(
                        output,
                        "macro_rules!",
                        ident,
                        scope,
                        &syn::Visibility::Inherited,
                        value.span(),
                    );
                }
            }
            _ => {}
        }
    }
}

fn add_syn_symbol(
    output: &mut Vec<ParsedSymbol>,
    kind: &str,
    ident: &syn::Ident,
    scope: &[String],
    visibility: &syn::Visibility,
    span: proc_macro2::Span,
) {
    let start = span.start().line.max(1);
    let end = span.end().line.max(start);
    output.push(ParsedSymbol {
        kind: kind.to_owned(),
        name: ident.to_string(),
        scope: scope.to_vec(),
        start_line: start,
        end_line: end,
        visibility: if matches!(visibility, syn::Visibility::Public(_)) {
            "public"
        } else {
            "private"
        }
        .to_owned(),
        parser: "syn",
    });
}

fn impl_type_name(value: &syn::Type) -> String {
    if let syn::Type::Path(path) = value
        && let Some(segment) = path.path.segments.last()
    {
        return segment.ident.to_string();
    }
    "impl".to_owned()
}

fn regex_symbols(lines: &[&str]) -> Vec<ParsedSymbol> {
    let symbol_re = Regex::new(
        r"^\s*(pub(?:\([^)]*\))?\s+)?(?:(?:async)\s+)?(fn|struct|enum|trait|type|const|static|mod|macro_rules!)\s+([A-Za-z_][A-Za-z0-9_]*)",
    )
    .expect("static symbol regex");
    lines
        .iter()
        .enumerate()
        .filter_map(|(offset, line)| {
            let capture = symbol_re.captures(line)?;
            Some(ParsedSymbol {
                kind: capture.get(2)?.as_str().to_owned(),
                name: capture.get(3)?.as_str().to_owned(),
                scope: Vec::new(),
                start_line: offset + 1,
                end_line: item_end(lines, offset),
                visibility: if capture.get(1).is_some() {
                    "public"
                } else {
                    "private"
                }
                .to_owned(),
                parser: "regex-fallback",
            })
        })
        .collect()
}

fn item_end(lines: &[&str], start: usize) -> usize {
    let mut depth = 0i32;
    let mut seen = false;
    for (i, line) in lines.iter().enumerate().skip(start) {
        for c in line.chars() {
            if c == '{' {
                depth += 1;
                seen = true
            } else if c == '}' {
                depth -= 1
            }
        }
        if (seen && depth <= 0) || (!seen && i > start && line.trim().ends_with(';')) {
            return i + 1;
        }
    }
    (start + 1).min(lines.len())
}
fn short_name(name: &str) -> &str {
    name.rsplit("::").next().unwrap_or(name)
}
/// Reads a source file, returning `None` when it cannot be read or is not UTF-8.
///
/// Callers previously used `fs::read_to_string(..).unwrap_or_default()`, which
/// turned an unreadable or non-UTF-8 file into an *empty* one: zero symbols
/// indexed, no error, no diagnostic, and downstream dead-code recommendations
/// derived from the silence. `None` lets a caller skip the file instead of
/// treating it as genuinely empty.
/// Parses Rust source, refusing input nested deeply enough to overflow the stack.
///
/// `syn::parse_file` is recursive descent with no depth limit, so a generated
/// file with a few thousand nested delimiters aborts the whole process — a
/// stack overflow is not a catchable error. Callers already treat `None` as
/// "fall back to the regex scanner", which is the right degradation here.
fn parse_rust_file(text: &str) -> Option<syn::File> {
    const MAX_NESTING: usize = 128;
    let mut depth = 0usize;
    let mut deepest = 0usize;
    for byte in text.bytes() {
        match byte {
            b'(' | b'[' | b'{' => {
                depth += 1;
                deepest = deepest.max(depth);
                if deepest > MAX_NESTING {
                    return None;
                }
            }
            b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    syn::parse_file(text).ok()
}

fn read_source_text(path: &Path) -> Option<String> {
    match fs::read(path) {
        Ok(bytes) => String::from_utf8(bytes).ok(),
        Err(_) => None,
    }
}

fn terms(input: &str) -> Vec<String> {
    // The ASCII-only class silently produced zero terms for CJK, Cyrillic, and
    // Greek input, which became an empty FTS MATCH and an empty result with no
    // error. The Unicode classes are a strict superset of the old behaviour.
    let re = Regex::new(r"[\p{Alphabetic}_][\p{Alphabetic}\p{Nd}_]*").unwrap();
    let values: Vec<_> = re
        .find_iter(input)
        .map(|m| m.as_str().to_string())
        .collect();
    if values.is_empty() {
        vec!["".into()]
    } else {
        values
    }
}

fn embed_text(input: &str) -> Vec<f32> {
    let mut vector = vec![0.0f32; EMBEDDING_DIMENSIONS];
    for token in embedding_tokens(input) {
        let hash = blake3::hash(token.as_bytes());
        let bytes = hash.as_bytes();
        let bucket = u16::from_le_bytes([bytes[0], bytes[1]]) as usize % EMBEDDING_DIMENSIONS;
        let sign = if bytes[2] & 1 == 0 { 1.0 } else { -1.0 };
        let weight = if token.starts_with('^') { 0.35 } else { 1.0 };
        vector[bucket] += sign * weight;
    }
    let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
    if norm > f32::EPSILON {
        for value in &mut vector {
            *value /= norm;
        }
    }
    vector
}

fn embedding_tokens(input: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    for character in input.chars() {
        // Mirrors `terms`: an ASCII-only test left non-Latin identifiers out of
        // the feature vector entirely, so semantic search was blind to them.
        if character.is_alphanumeric() || character == '_' {
            if character.is_uppercase() && current.chars().last().is_some_and(char::is_lowercase) {
                words.push(current.to_lowercase());
                current.clear();
            }
            current.push(character);
        } else if !current.is_empty() {
            words.push(current.to_lowercase());
            current.clear();
        }
    }
    if !current.is_empty() {
        words.push(current.to_lowercase());
    }
    let mut tokens = Vec::new();
    for word in words.into_iter().filter(|word| word.len() > 1) {
        tokens.push(word.clone());
        let padded = format!("^{word}$");
        let characters = padded.chars().collect::<Vec<_>>();
        for trigram in characters.windows(3) {
            tokens.push(trigram.iter().collect());
        }
    }
    tokens
}

fn encode_vector(vector: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(vector.len() * 4);
    for value in vector {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

fn decode_vector(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect()
}

fn dot_product(left: &[f32], right: &[f32]) -> f64 {
    left.iter()
        .zip(right)
        .map(|(left, right)| f64::from(*left) * f64::from(*right))
        .sum()
}

fn is_exact_identifier_match(query: &str, node: &Node) -> bool {
    let query = query.trim().trim_matches('`');
    if query.eq_ignore_ascii_case(&node.canonical_name)
        || query.eq_ignore_ascii_case(short_name(&node.canonical_name))
    {
        return true;
    }
    terms(query).into_iter().any(|term| {
        let looks_intentional =
            term.contains('_') || term.chars().any(char::is_uppercase) || query.contains("::");
        looks_intentional && term.eq_ignore_ascii_case(short_name(&node.canonical_name))
    })
}

fn add_rrf_hit(
    ranked: &mut HashMap<i64, HybridHit>,
    node: Node,
    channel: &str,
    rank: usize,
    weight: f64,
) {
    let hit = ranked.entry(node.id).or_insert_with(|| HybridHit {
        node,
        score: 0.0,
        channels: BTreeSet::new(),
        exact_match: false,
        embedding: None,
    });
    hit.score += weight / (RRF_K + rank as f64 + 1.0);
    hit.channels.insert(channel.to_owned());
}

fn budget_values(values: Vec<Value>, used: &mut usize, max_bytes: usize) -> Vec<Value> {
    let mut output = Vec::new();
    for value in values {
        let size = value.to_string().len();
        if used.saturating_add(size) > max_bytes {
            break;
        }
        *used += size;
        output.push(value);
    }
    output
}
fn fts_query(query: &[String]) -> String {
    let mut seen = BTreeSet::new();
    query
        .iter()
        .filter(|term| !term.is_empty())
        .map(|term| term.to_lowercase())
        .filter(|term| seen.insert(term.clone()))
        .map(|term| format!("\"{}\"*", term.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" OR ")
}
fn steering_is_active(steering: &Value) -> bool {
    if steering["status"] != "active" {
        return false;
    }
    steering["expires_at"]
        .as_str()
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
        .is_none_or(|expires_at| expires_at > Utc::now())
}
fn dedupe_nodes(nodes: &mut Vec<Node>) {
    let mut seen = BTreeSet::new();
    nodes.retain(|n| seen.insert(n.id));
}
fn files_for_nodes(a: &[Node], b: &[Node]) -> Vec<String> {
    let files: BTreeSet<String> = a.iter().chain(b).map(|n| n.file.clone()).collect();
    files.into_iter().collect()
}
fn risk(targets: &[Node], references: &[Node]) -> &'static str {
    if targets.iter().any(|n| n.visibility == "public") || references.len() > 12 {
        "high"
    } else if references.len() > 3 {
        "medium"
    } else {
        "low"
    }
}
fn slug(value: &str) -> String {
    value
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|x| !x.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}
fn decision_markdown(id: &str, d: &RecordDecision) -> String {
    format!(
        "# {id}: {}\n\nStatus: {}\n\n## Rationale\n\n{}\n\n## Applies to\n\n{}\n\n## Consequences\n\n{}\n",
        d.title,
        d.status,
        d.reason,
        d.applies_to
            .iter()
            .map(|x| format!("- {x}"))
            .collect::<Vec<_>>()
            .join("\n"),
        d.consequences
            .iter()
            .map(|x| format!("- {x}"))
            .collect::<Vec<_>>()
            .join("\n")
    )
}
/// Runs one cargo command and returns a bounded, structured result.
///
/// This previously returned the entire uncapped `stderr` as prose, so a failing
/// `cargo test --all-targets` on a real workspace dumped its whole output into
/// the MCP response and the compiler-error workflow got text where it needed
/// file/line diagnostics.
fn check(root: &Path, args: &[&str]) -> Value {
    let command = format!("cargo {}", args.join(" "));
    // `cargo fmt` does not understand `--message-format`, and the JSON stream is
    // only useful for the compiler-driven commands.
    let structured = matches!(args.first(), Some(&"check" | &"clippy" | &"test"));
    let mut invocation = Command::new("cargo");
    invocation.args(args).current_dir(root);
    if structured {
        invocation.arg("--message-format=json-diagnostic-rendered-ansi");
    }
    match invocation.output() {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let diagnostics = if structured {
                cargo_diagnostics(&stdout)
            } else {
                Vec::new()
            };
            json!({
                "command": command,
                "success": output.status.success(),
                "diagnostics": diagnostics,
                "truncated_output": trim_text(&stderr, MAX_CHECK_OUTPUT_BYTES),
                "output_truncated": stderr.len() > MAX_CHECK_OUTPUT_BYTES,
            })
        }
        Err(error) => {
            json!({"command":command,"success":false,"error":error.to_string()})
        }
    }
}

/// Extracts bounded `file:line` diagnostics from a cargo JSON message stream.
fn cargo_diagnostics(stdout: &str) -> Vec<Value> {
    let mut diagnostics = Vec::new();
    for line in stdout.lines() {
        if diagnostics.len() >= MAX_CHECK_DIAGNOSTICS {
            break;
        }
        let Ok(message) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if message["reason"] != "compiler-message" {
            continue;
        }
        let diagnostic = &message["message"];
        let level = diagnostic["level"].as_str().unwrap_or("");
        if !matches!(level, "error" | "warning") {
            continue;
        }
        let span = diagnostic["spans"]
            .as_array()
            .and_then(|spans| spans.iter().find(|span| span["is_primary"] == true));
        diagnostics.push(json!({
            "level": level,
            "code": diagnostic["code"]["code"].as_str(),
            "message": trim_text(diagnostic["message"].as_str().unwrap_or(""), 1_000),
            "file": span.and_then(|span| span["file_name"].as_str()),
            "line": span.and_then(|span| span["line_start"].as_u64()),
            "column": span.and_then(|span| span["column_start"].as_u64()),
            "location": span.and_then(|span| {
                Some(format!(
                    "{}:{}",
                    span["file_name"].as_str()?,
                    span["line_start"].as_u64()?
                ))
            }),
        }));
    }
    diagnostics
}

fn artifact_validator(path: &Path) -> Option<(&'static str, Vec<OsString>)> {
    let extension = path.extension()?.to_string_lossy();
    let (program, first_argument) = match extension.as_ref() {
        "ui" => ("gtk4-builder-tool", "validate"),
        "blp" => ("blueprint-compiler", "compile"),
        "xml" => ("xmllint", "--noout"),
        _ => return None,
    };
    Some((
        program,
        vec![OsString::from(first_argument), path.as_os_str().to_owned()],
    ))
}

fn run_optional_command_check(root: &Path, program: &str, args: &[OsString]) -> Value {
    let command = std::iter::once(program.to_owned())
        .chain(
            args.iter()
                .map(|argument| argument.to_string_lossy().into_owned()),
        )
        .collect::<Vec<_>>()
        .join(" ");
    if !program_available(program) {
        return json!({
            "command": command,
            "success": Value::Null,
            "skipped": true,
            "reason": format!("{program} is not available on PATH"),
        });
    }
    match Command::new(program).args(args).current_dir(root).output() {
        Ok(output) => json!({
            "command": command,
            "success": output.status.success(),
            "skipped": false,
            "stdout": trim_text(&String::from_utf8_lossy(&output.stdout), 4_000),
            "stderr": trim_text(&String::from_utf8_lossy(&output.stderr), 4_000),
        }),
        Err(error) => json!({
            "command": command,
            "success": false,
            "skipped": false,
            "error": error.to_string(),
        }),
    }
}

fn program_available(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|directory| directory.join(program).is_file())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    fn fixture() -> tempfile::TempDir {
        let d = tempdir().unwrap();
        fs::write(
            d.path().join("Cargo.toml"),
            "[package]\nname='demo'\nversion='0.1.0'\nedition='2024'\n",
        )
        .unwrap();
        fs::create_dir(d.path().join("src")).unwrap();
        fs::write(
            d.path().join("src/lib.rs"),
            "pub trait Store { fn load(&self); }\nfn uses(s: &dyn Store) { s.load(); }\n",
        )
        .unwrap();
        d
    }

    fn git(root: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_fixture() -> tempfile::TempDir {
        let d = fixture();
        git(d.path(), &["init", "-q"]);
        git(d.path(), &["config", "user.name", "Test User"]);
        git(d.path(), &["config", "user.email", "test@example.com"]);
        git(d.path(), &["add", "Cargo.toml", "src/lib.rs"]);
        git(d.path(), &["commit", "-q", "-m", "initial"]);
        d
    }

    #[test]
    fn rust_analyzer_uses_bounded_background_resources() {
        let options = rust_analyzer_initialization_options();
        assert_eq!(options["numThreads"], RUST_ANALYZER_THREADS);
        assert_eq!(options["cachePriming"]["enable"], false);
        assert_eq!(
            options["cargo"]["extraEnv"]["CARGO_BUILD_JOBS"],
            RUST_ANALYZER_THREADS.to_string()
        );
        assert_eq!(options["checkOnSave"], false);
    }

    #[test]
    fn rust_analyzer_requires_explicit_opt_in() {
        assert!(!rust_analyzer_enabled(None));
        assert!(!rust_analyzer_enabled(Some("0")));
        assert!(!rust_analyzer_enabled(Some("false")));
        assert!(rust_analyzer_enabled(Some("1")));
        assert!(rust_analyzer_enabled(Some("TRUE")));
        assert!(rust_analyzer_enabled(Some(" yes ")));
    }

    #[test]
    fn opening_service_does_not_start_rust_analyzer() {
        let d = fixture();
        let service = Service::open(d.path()).unwrap();
        assert_eq!(service.index_status()["rust_analyzer_running"], false);
        assert_eq!(
            service.index_status()["rust_analyzer_start_attempted"],
            false
        );
        assert!(service.index_status()["rust_analyzer_program"].is_string());
    }

    #[test]
    fn rust_analyzer_start_failure_is_actionable_status() {
        let d = fixture();
        let mut service = Service::open(d.path()).unwrap();
        service.ra_enabled = true;
        service.ra_program = PathBuf::from("/definitely/missing/rust-analyzer");
        service.start_rust_analyzer_if_enabled();
        let status = service.index_status();
        assert_eq!(status["rust_analyzer_start_attempted"], true);
        assert_eq!(status["rust_analyzer_running"], false);
        assert!(
            status["rust_analyzer_error"]
                .as_str()
                .unwrap()
                .contains("starting rust-analyzer")
        );
        assert_eq!(
            rust_analyzer_program(Some(OsString::from("/custom/rust-analyzer"))),
            PathBuf::from("/custom/rust-analyzer")
        );
    }

    #[test]
    fn indexes_and_prepares_context() {
        let d = fixture();
        let mut service = Service::open(d.path()).unwrap();
        service.refresh_if_stale().unwrap();
        let v = service
            .prepare_change("change Store", &["Store".into()], 2, None)
            .unwrap();
        assert!(!v["primary_symbols"].as_array().unwrap().is_empty());
    }

    #[test]
    fn decisions_and_change_contexts_are_persistent_resources() {
        let d = fixture();
        let mut service = Service::open(d.path()).unwrap();
        service.refresh_if_stale().unwrap();
        let decision = service
            .record_decision(RecordDecision {
                title: "Keep Store injected".into(),
                status: "accepted".into(),
                reason: "Permits isolated testing".into(),
                applies_to: vec!["Store".into()],
                consequences: vec!["Do not construct stores in handlers".into()],
                supersedes: None,
                materialize: false,
            })
            .unwrap();
        let decision_id = decision["id"].as_str().unwrap();
        assert_eq!(
            service
                .resource(&format!("rustrepo://decision/{decision_id}"))
                .unwrap()["title"],
            "Keep Store injected"
        );
        let context = service
            .prepare_change("change Store", &["Store".into()], 1, Some(1_000))
            .unwrap();
        let context_id = context["context_id"].as_str().unwrap();
        assert_eq!(
            service
                .resource(&format!("rustrepo://change/{context_id}"))
                .unwrap()["context_id"],
            context_id
        );
    }

    #[test]
    fn source_changes_refresh_symbols_without_rebuilding_decisions() {
        let d = fixture();
        let mut service = Service::open(d.path()).unwrap();
        let decision = service
            .record_decision(RecordDecision {
                title: "Keep Store injected".into(),
                status: "accepted".into(),
                reason: "Permits isolated testing".into(),
                applies_to: vec!["Store".into()],
                consequences: vec![],
                supersedes: None,
                materialize: false,
            })
            .unwrap();
        fs::write(
            d.path().join("src/lib.rs"),
            "pub trait Store { fn load(&self); }\npub fn replacement() {}\n",
        )
        .unwrap();
        service.refresh_if_stale().unwrap();
        assert!(
            !service
                .search_nodes(&terms("replacement"), 1)
                .unwrap()
                .is_empty()
        );
        assert!(
            service
                .resource(&format!(
                    "rustrepo://decision/{}",
                    decision["id"].as_str().unwrap()
                ))
                .is_ok()
        );
        let linked_node: Option<i64> = service
            .db
            .query_row(
                "SELECT node_id FROM decision_targets WHERE decision_id=?1 AND target_ref='Store'",
                [decision["id"].as_str().unwrap()],
                |row| row.get(0),
            )
            .unwrap();
        assert!(linked_node.is_some());
    }

    #[test]
    fn cache_schema_enables_foreign_keys_and_required_tables() {
        let d = fixture();
        let service = Service::open(d.path()).unwrap();
        let foreign_keys: i64 = service
            .db
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .unwrap();
        assert_eq!(foreign_keys, 1);
        let tables: BTreeSet<String> = service
            .db
            .prepare("SELECT name FROM sqlite_master WHERE type='table'")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        for table in [
            "metadata",
            "packages",
            "package_dependencies",
            "package_targets",
            "package_features",
            "nodes",
            "edges",
            "unresolved_references",
            "semantic_snapshots",
            "index_generations",
            "semantic_queries",
            "symbol_embeddings",
            "search_index",
            "decisions",
            "decision_targets",
            "steerings",
            "commits",
            "commit_files",
            "co_changes",
            "use_cases",
            "use_case_nodes",
            "change_contexts",
        ] {
            assert!(tables.contains(table), "missing table: {table}");
        }
    }

    #[test]
    fn additive_schema_upgrade_preserves_decisions_and_steerings() {
        let d = fixture();
        {
            let service = Service::open(d.path()).unwrap();
            service
                .record_decision(RecordDecision {
                    title: "Preserve project memory".into(),
                    status: "accepted".into(),
                    reason: "Additive cache upgrades are not conceptual resets".into(),
                    applies_to: vec![],
                    consequences: vec![],
                    supersedes: None,
                    materialize: false,
                })
                .unwrap();
            service
                .record_steering(RecordSteering {
                    title: "Keep the memory".into(),
                    instruction: "Do not erase it during routine upgrades".into(),
                    scope: vec![],
                    priority: "high".into(),
                    status: "active".into(),
                    expires_at: None,
                })
                .unwrap();
            service
                .db
                .execute_batch(
                    "DROP TABLE symbol_embeddings; DROP TABLE semantic_queries; DROP TABLE index_generations; DROP TABLE semantic_snapshots; UPDATE metadata SET value='4' WHERE key='schema_version';",
                )
                .unwrap();
        }
        let service = Service::open(d.path()).unwrap();
        let decisions: i64 = service
            .db
            .query_row("SELECT COUNT(*) FROM decisions", [], |row| row.get(0))
            .unwrap();
        let steerings: i64 = service
            .db
            .query_row("SELECT COUNT(*) FROM steerings", [], |row| row.get(0))
            .unwrap();
        assert_eq!(decisions, 1);
        assert_eq!(steerings, 1);
    }

    #[test]
    fn indexes_cargo_feature_target_matrix() {
        let d = fixture();
        let mut service = Service::open(d.path()).unwrap();
        service.refresh_if_stale().unwrap();
        let matrix = service.matrix().unwrap();
        assert!(!matrix["targets"].as_array().unwrap().is_empty());
        assert!(matrix["targets"][0]["package"].is_string());
        assert!(
            service.status().unwrap()["counts"]["package_targets"]
                .as_i64()
                .unwrap()
                > 0
        );
    }

    #[test]
    fn finds_referenced_but_explicitly_superseded_compatibility_path() {
        let d = fixture();
        fs::write(
            d.path().join("src/lib.rs"),
            "pub fn canonical_store() {}\n\n/// Legacy adapter replaced with canonical_store.\npub fn legacy_store() { canonical_store(); }\n\npub fn consumer() { legacy_store(); }\n// TODO: remove the compatibility adapter after external clients migrate\n",
        )
        .unwrap();
        let mut service = Service::open(d.path()).unwrap();
        service.refresh_if_stale().unwrap();
        let candidates = service
            .obsolete_candidates(Some("legacy_store"), 10)
            .unwrap();
        let candidate = candidates["candidates"]
            .as_array()
            .unwrap()
            .first()
            .unwrap();
        assert_eq!(candidate["classification"], "architecturally-superseded");
        assert_eq!(candidate["replacement"], "src::lib::canonical_store");
        assert!(!candidate["current_callers"].as_array().unwrap().is_empty());
        let work = service
            .work_list(Some("compatibility adapter"), 10)
            .unwrap();
        assert_eq!(work["items"][0]["status"], "proposed");
        assert_eq!(work["items"][0]["provenance"], "SourceDoc");
    }

    #[test]
    fn explicit_work_is_editable_and_snapshot_aware() {
        let d = fixture();
        let mut service = Service::open(d.path()).unwrap();
        service.refresh_if_stale().unwrap();
        let proposed = service
            .work_propose(WorkItemInput {
                title: "Audit public Store callers".into(),
                status: "accepted".into(),
                priority: "high".into(),
                kind: "test_gap".into(),
                scope: vec!["Store".into()],
                evidence: vec!["integration test is missing".into()],
                depends_on: vec![],
                blocked_by: vec![],
                acceptance_criteria: vec!["public callers are covered".into()],
                verification: vec!["cargo test".into()],
            })
            .unwrap();
        let id = proposed["id"].as_str().unwrap();
        service
            .work_update(id, &json!({"status":"in_progress"}))
            .unwrap();
        assert_eq!(service.work_next(None).unwrap()["next"]["id"], id);
        let snapshot = &service.status().unwrap()["snapshot"];
        assert!(snapshot["revision"].is_object());
        assert!(snapshot["indexing_timestamp"].is_string());
    }

    #[test]
    fn validation_reads_all_unified_diff_file_headers() {
        let d = fixture();
        let mut service = Service::open(d.path()).unwrap();
        service.refresh_if_stale().unwrap();
        let context = service
            .prepare_change("change Store", &["Store".into()], 1, Some(1_000))
            .unwrap();
        let validated = service
            .validate_change(
                context["context_id"].as_str().unwrap(),
                Some("+++ b/src/lib.rs\n+++ b/Cargo.toml\n"),
                false,
            )
            .unwrap();
        let changed = validated["changed_files"].as_array().unwrap();
        assert!(changed.iter().any(|path| path == "src/lib.rs"));
        assert!(changed.iter().any(|path| path == "Cargo.toml"));
    }

    #[test]
    fn removed_todo_evidence_is_retained_as_stale_proposed_work() {
        let d = fixture();
        fs::write(
            d.path().join("src/lib.rs"),
            "pub trait Store { fn load(&self); }\n// TODO: support a third storage backend\n",
        )
        .unwrap();
        let mut service = Service::open(d.path()).unwrap();
        service.refresh_if_stale().unwrap();
        let before = service
            .work_list(Some("support a third storage"), 10)
            .unwrap();
        assert_eq!(before["items"][0]["confidence"], 0.7);
        fs::write(
            d.path().join("src/lib.rs"),
            "pub trait Store { fn load(&self); }\n",
        )
        .unwrap();
        service.refresh_if_stale().unwrap();
        let after = service
            .work_list(Some("support a third storage"), 10)
            .unwrap();
        assert_eq!(after["items"][0]["status"], "proposed");
        assert_eq!(after["items"][0]["confidence"], 0.2);
        assert!(
            after["items"][0]["evidence"][0]
                .as_str()
                .unwrap()
                .contains("absent")
        );
    }

    #[test]
    fn full_text_search_ranks_exact_symbols_and_document_terms() {
        let d = fixture();
        fs::write(
            d.path().join("README.md"),
            "Atomic refresh recovery keeps repository snapshots consistent.\n",
        )
        .unwrap();
        let mut service = Service::open(d.path()).unwrap();
        service.refresh_if_stale().unwrap();
        let nodes = service.search_nodes(&terms("src::lib::uses"), 5).unwrap();
        assert_eq!(nodes[0].canonical_name, "src::lib::uses");
        let located = service
            .locate("atomic recovery snapshot", 10, false)
            .unwrap();
        assert!(
            located["documentation"]
                .as_array()
                .unwrap()
                .iter()
                .any(|hit| hit["path"] == "README.md")
        );
    }

    #[test]
    fn static_references_are_attributed_to_enclosing_symbols() {
        let d = fixture();
        fs::write(
            d.path().join("src/lib.rs"),
            "pub fn target() {}\npub fn first() {}\npub fn caller() { target(); }\n",
        )
        .unwrap();
        let mut service = Service::open(d.path()).unwrap();
        service.refresh_if_stale().unwrap();
        let caller: String = service
            .db
            .query_row(
                "SELECT source.canonical_name FROM edges e JOIN nodes source ON source.id=e.src JOIN nodes target ON target.id=e.dst WHERE target.canonical_name='src::lib::target'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(caller, "src::lib::caller");
    }

    #[test]
    fn static_references_do_not_fan_out_across_ambiguous_short_names() {
        let d = fixture();
        fs::write(d.path().join("src/a.rs"), "pub fn load() {}\n").unwrap();
        fs::write(d.path().join("src/b.rs"), "pub fn load() {}\n").unwrap();
        fs::write(
            d.path().join("src/lib.rs"),
            "mod a;\nmod b;\npub fn caller() { a::load(); let _ = \"b::load()\"; /* b::load() */ }\n",
        )
        .unwrap();
        let mut service = Service::open(d.path()).unwrap();
        service.refresh_if_stale().unwrap();
        let targets: Vec<String> = service
            .db
            .prepare("SELECT target.canonical_name FROM edges e JOIN nodes source ON source.id=e.src JOIN nodes target ON target.id=e.dst WHERE source.canonical_name='src::lib::caller' AND target.canonical_name LIKE '%::load' ORDER BY target.canonical_name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(targets, vec!["src::a::load"]);
    }

    #[test]
    fn ambiguous_static_references_are_reported_but_not_promoted() {
        let d = fixture();
        fs::write(d.path().join("src/a.rs"), "pub fn load() {}\n").unwrap();
        fs::write(d.path().join("src/b.rs"), "pub fn load() {}\n").unwrap();
        fs::write(
            d.path().join("src/lib.rs"),
            "mod a;\nmod b;\npub fn caller() { load(); }\n",
        )
        .unwrap();
        let mut service = Service::open(d.path()).unwrap();
        service.refresh_if_stale().unwrap();
        let promoted: i64 = service
            .db
            .query_row(
                "SELECT COUNT(*) FROM edges e JOIN nodes source ON source.id=e.src WHERE source.canonical_name='src::lib::caller'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let unresolved: i64 = service
            .db
            .query_row("SELECT COUNT(*) FROM unresolved_references", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(promoted, 0);
        assert_eq!(unresolved, 1);
    }

    #[test]
    fn prepare_change_excludes_unrelated_ambiguous_neighbors() {
        let d = fixture();
        fs::write(d.path().join("src/a.rs"), "pub fn load() {}\n").unwrap();
        fs::write(d.path().join("src/b.rs"), "pub fn load() {}\n").unwrap();
        fs::write(
            d.path().join("src/lib.rs"),
            "mod a;\nmod b;\npub fn caller() { a::load(); }\n",
        )
        .unwrap();
        let mut service = Service::open(d.path()).unwrap();
        service.refresh_if_stale().unwrap();
        let context = service
            .prepare_change("change a load", &["src::a::load".into()], 2, Some(1_000))
            .unwrap();
        let surface = context["likely_change_surface"].as_array().unwrap();
        assert!(surface.iter().any(|path| path == "src/a.rs"));
        assert!(surface.iter().any(|path| path == "src/lib.rs"));
        assert!(!surface.iter().any(|path| path == "src/b.rs"));
    }

    #[test]
    fn gtk_and_dbus_contract_artifacts_are_full_text_searchable() {
        let d = fixture();
        fs::create_dir(d.path().join("data")).unwrap();
        fs::write(
            d.path().join("data/window.ui"),
            "<interface><object class=\"GtkDropDown\" id=\"duplicate-warning-marker\"/></interface>\n",
        )
        .unwrap();
        fs::write(
            d.path().join("data/org.example.Demo.xml"),
            "<node><interface name=\"org.example.Demo\"><method name=\"ReloadCatalog\"/></interface></node>\n",
        )
        .unwrap();
        let mut service = Service::open(d.path()).unwrap();
        service.refresh_if_stale().unwrap();
        let gtk = service
            .locate("duplicate warning marker", 10, false)
            .unwrap();
        let dbus = service.locate("ReloadCatalog", 10, false).unwrap();
        assert!(
            gtk["documentation"]
                .as_array()
                .unwrap()
                .iter()
                .any(|hit| hit["path"] == "data/window.ui")
        );
        assert!(
            dbus["documentation"]
                .as_array()
                .unwrap()
                .iter()
                .any(|hit| hit["path"] == "data/org.example.Demo.xml")
        );
    }

    #[test]
    fn changed_runtime_artifacts_select_native_validators() {
        let (program, arguments) = artifact_validator(Path::new("data/window.ui")).unwrap();
        assert_eq!(program, "gtk4-builder-tool");
        assert_eq!(arguments[0], "validate");
        assert_eq!(arguments[1], "data/window.ui");

        let (program, arguments) =
            artifact_validator(Path::new("data/org.example.Demo.xml")).unwrap();
        assert_eq!(program, "xmllint");
        assert_eq!(arguments[0], "--noout");
        assert!(artifact_validator(Path::new("src/lib.rs")).is_none());
    }

    #[test]
    fn git_history_links_commits_to_files() {
        let d = git_fixture();
        let mut service = Service::open(d.path()).unwrap();
        service.refresh_if_stale().unwrap();
        let links: i64 = service
            .db
            .query_row("SELECT COUNT(*) FROM commit_files", [], |row| row.get(0))
            .unwrap();
        assert!(links >= 2);
    }

    #[test]
    fn checkpoints_capture_dirty_tracked_state_without_checkout() {
        let d = git_fixture();
        fs::write(
            d.path().join("src/lib.rs"),
            "pub fn checkpointed() -> u8 { 7 }\n",
        )
        .unwrap();
        let service = Service::open(d.path()).unwrap();
        let before_branch = command_text(d.path(), &["branch", "--show-current"]);
        let checkpoint = service.checkpoint_create("test task", false).unwrap();
        assert!(
            checkpoint["reference"]
                .as_str()
                .unwrap()
                .starts_with(CHECKPOINT_REF_PREFIX)
        );
        assert_eq!(
            fs::read_to_string(d.path().join("src/lib.rs")).unwrap(),
            "pub fn checkpointed() -> u8 { 7 }\n"
        );
        assert_eq!(
            command_text(d.path(), &["branch", "--show-current"]),
            before_branch
        );
        assert_eq!(service.checkpoint_list(10).unwrap()["count"], 1);
    }

    #[test]
    fn todo_discovery_ignores_prose_regexes_and_string_fixtures() {
        let d = fixture();
        fs::write(
            d.path().join("README.md"),
            "This paragraph documents TODO and FIXME scanner behavior.\n",
        )
        .unwrap();
        fs::write(
            d.path().join("src/lib.rs"),
            "pub const SAMPLE: &str = \"// TODO: not executable work\";\n// TODO: real follow-up\n",
        )
        .unwrap();
        let mut service = Service::open(d.path()).unwrap();
        service.refresh_if_stale().unwrap();
        let work = service.work_list(None, 20).unwrap();
        assert_eq!(work["items"].as_array().unwrap().len(), 1);
        assert_eq!(work["items"][0]["title"], "real follow-up");
    }

    #[test]
    fn indexer_version_forces_derived_data_backfill() {
        let d = fixture();
        let mut service = Service::open(d.path()).unwrap();
        service.refresh_if_stale().unwrap();
        service
            .db
            .execute("DELETE FROM package_targets", [])
            .unwrap();
        service
            .db
            .execute(
                "UPDATE metadata SET value='old' WHERE key='indexer_version'",
                [],
            )
            .unwrap();
        service.refresh_if_stale().unwrap();
        assert!(
            !service.matrix().unwrap()["targets"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn savepoint_rolls_back_partial_index_mutation() {
        let d = fixture();
        let mut service = Service::open(d.path()).unwrap();
        service.refresh_if_stale().unwrap();
        let before: i64 = service
            .db
            .query_row("SELECT COUNT(*) FROM nodes", [], |row| row.get(0))
            .unwrap();
        let result: Result<()> = service.with_savepoint("rollback_test", |service| {
            service.db.execute("DELETE FROM nodes", [])?;
            bail!("injected failure")
        });
        assert!(result.is_err());
        let after: i64 = service
            .db
            .query_row("SELECT COUNT(*) FROM nodes", [], |row| row.get(0))
            .unwrap();
        assert_eq!(after, before);
    }

    #[test]
    fn status_distinguishes_live_and_indexed_snapshots() {
        let d = fixture();
        let mut service = Service::open(d.path()).unwrap();
        service.refresh_if_stale().unwrap();
        fs::write(d.path().join("src/lib.rs"), "pub fn changed() {}\n").unwrap();
        assert_eq!(service.status().unwrap()["stale"], true);
        service.refresh_if_stale().unwrap();
        assert_eq!(service.status().unwrap()["stale"], false);
    }

    #[test]
    fn scoped_steerings_are_searchable_and_join_change_contexts() {
        let d = fixture();
        let mut service = Service::open(d.path()).unwrap();
        service.refresh_if_stale().unwrap();
        service
            .record_steering(RecordSteering {
                title: "Keep storage injectable".into(),
                instruction: "Do not construct Store implementations inside handlers".into(),
                scope: vec!["Store".into()],
                priority: "high".into(),
                status: "active".into(),
                expires_at: None,
            })
            .unwrap();
        let listed = service.steering_list(Some("injectable Store"), 10).unwrap();
        assert_eq!(listed["steerings"][0]["priority"], "high");
        let context = service
            .prepare_change("change Store", &["Store".into()], 1, Some(500))
            .unwrap();
        assert_eq!(context["steerings"][0]["title"], "Keep storage injectable");
    }

    #[test]
    fn embeddings_and_rrf_context_pack_cover_the_active_generation() {
        let d = fixture();
        let mut service = Service::open(d.path()).unwrap();
        service.refresh_if_stale().unwrap();
        let status = service.status().unwrap();
        assert_eq!(status["counts"]["embeddings"], status["counts"]["nodes"]);
        assert!(status["snapshot"]["generation"].is_number());
        assert!(status["snapshot"]["semantic_snapshot"]["target_triple"].is_string());
        let context = service
            .context_pack("storage load implementation", 600, 10)
            .unwrap();
        assert!(!context["ranked_symbols"].as_array().unwrap().is_empty());
        assert!(
            context["context_budget"]["estimated_tokens"]
                .as_u64()
                .unwrap()
                <= 600
        );
        assert!(
            context["retrieval"]
                .as_str()
                .unwrap()
                .contains("reciprocal-rank")
        );
        assert_eq!(
            context["retrieval_provenance"]["embedding"]["card_version"],
            SYMBOL_CARD_VERSION
        );
        let embedded = context["ranked_symbols"]
            .as_array()
            .unwrap()
            .iter()
            .find(|hit| hit["embedding_evidence"].is_object())
            .unwrap();
        assert_eq!(embedded["embedding_evidence"]["model"], EMBEDDING_MODEL);
        assert!(
            embedded["embedding_evidence"]["card_hash"]
                .as_str()
                .unwrap()
                .starts_with("b3:")
        );
    }

    #[test]
    fn exact_identifiers_precede_fused_semantic_candidates() {
        let d = fixture();
        fs::write(
            d.path().join("src/lib.rs"),
            "pub fn refresh_if_stale() {}\npub fn refresh_workspace_index() { refresh_if_stale(); }\n",
        )
        .unwrap();
        let mut service = Service::open(d.path()).unwrap();
        service.refresh_if_stale().unwrap();

        let hits = service.hybrid_nodes("refresh_if_stale impact", 10).unwrap();
        assert_eq!(short_name(&hits[0].node.canonical_name), "refresh_if_stale");
        assert!(hits[0].exact_match);
        assert!(hits[0].channels.contains("exact_identifier"));
    }

    #[test]
    fn incremental_refresh_reuses_unchanged_symbol_cards() {
        let d = fixture();
        fs::write(
            d.path().join("src/other.rs"),
            "pub fn unchanged_symbol_card() {}\n",
        )
        .unwrap();
        let mut service = Service::open(d.path()).unwrap();
        service.refresh_if_stale().unwrap();
        let before: (Vec<u8>, String, String) = service
            .db
            .query_row(
                "SELECT e.vector,e.content_hash,e.semantic_snapshot \
                 FROM symbol_embeddings e JOIN nodes n ON n.id=e.node_id \
                 WHERE n.canonical_name LIKE '%::unchanged_symbol_card'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();

        fs::write(
            d.path().join("src/lib.rs"),
            "pub trait Store { fn load(&self); }\nfn uses(s: &dyn Store) { s.load(); }\npub fn newly_indexed() {}\n",
        )
        .unwrap();
        service.watcher_trusted = false;
        service.refresh_if_stale().unwrap();

        let after: (Vec<u8>, String, String) = service
            .db
            .query_row(
                "SELECT e.vector,e.content_hash,e.semantic_snapshot \
                 FROM symbol_embeddings e JOIN nodes n ON n.id=e.node_id \
                 WHERE n.canonical_name LIKE '%::unchanged_symbol_card'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(after.0, before.0);
        assert_eq!(after.1, before.1);
        assert_ne!(after.2, before.2);
        let status = service.embedding_index_status();
        assert!(status["last_build"]["reused"].as_u64().unwrap() > 0);
        assert!(status["last_build"]["recomputed"].as_u64().unwrap() > 0);
    }

    #[test]
    fn structural_edges_distinguish_calls_implementations_and_containment() {
        let d = fixture();
        fs::write(
            d.path().join("src/lib.rs"),
            "pub trait Backend { fn load(&self); }\npub struct Disk;\nimpl Backend for Disk { fn load(&self) {} }\npub fn target() {}\npub fn caller() { target(); }\n",
        )
        .unwrap();
        let mut service = Service::open(d.path()).unwrap();
        service.refresh_if_stale().unwrap();
        let kinds: BTreeSet<String> = service
            .db
            .prepare("SELECT DISTINCT kind FROM edges")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(kinds.contains("CALLS_DIRECT"));
        assert!(kinds.contains("IMPLEMENTS"));
        assert!(kinds.contains("CONTAINS"));
    }

    #[test]
    fn rust_analyzer_edges_are_reused_from_the_semantic_snapshot_cache() {
        let d = fixture();
        let mut service = Service::open(d.path()).unwrap();
        service.refresh_if_stale().unwrap();
        let target = service.search_nodes(&terms("Store"), 1).unwrap().remove(0);
        let source = service.search_nodes(&terms("uses"), 1).unwrap().remove(0);
        let snapshot = service.active_semantic_snapshot_id().unwrap();
        service
            .insert_edge(EdgeRecord {
                source: source.id,
                target: target.id,
                kind: "REFERENCES",
                confidence: 1.0,
                provenance: "RustAnalyzer",
                revision: &snapshot,
                metadata: json!({"fixture":true}),
            })
            .unwrap();
        service
            .db
            .execute(
                "INSERT INTO semantic_queries(node_id,relation,semantic_snapshot,result_count,queried_at) VALUES (?1,'references',?2,1,?3)",
                params![target.id,snapshot,Utc::now().to_rfc3339()],
            )
            .unwrap();
        let cached = service.semantic_locations(&[target], "references");
        assert_eq!(cached.len(), 1);
        assert_eq!(cached[0].symbol, source.canonical_name);
        assert_eq!(cached[0].provenance, "StaticIndex");
    }

    #[test]
    fn watcher_hints_update_only_changed_input_hashes() {
        let d = fixture();
        let previous = index_inputs(d.path());
        fs::write(d.path().join("src/lib.rs"), "pub fn watched() {}\n").unwrap();
        let paths = BTreeSet::from([d.path().join("src/lib.rs")]);
        let updated = inputs_from_watch_paths(d.path(), &previous, &paths);
        assert_ne!(updated["src/lib.rs"].1, previous["src/lib.rs"].1);
        assert!(updated["src/lib.rs"].1.starts_with("b3:"));
        assert_eq!(updated["Cargo.toml"], previous["Cargo.toml"]);
    }

    #[test]
    fn feature_hash_embeddings_are_deterministic_and_normalized() {
        let first = embed_text("checkpointRestoreBranch");
        let second = embed_text("checkpoint restore branch");
        assert_eq!(first.len(), EMBEDDING_DIMENSIONS);
        assert!(dot_product(&first, &first) > 0.99);
        assert!(dot_product(&first, &second) > 0.25);
    }

    /// Records the human-authored rows that must never be destroyed by a
    /// schema-version transition, then reports them after reopening.
    fn human_memory_counts(path: &std::path::Path) -> (i64, i64, i64) {
        let db = Connection::open(path.join(".rust-repo-intelligence/index.sqlite3")).unwrap();
        let count = |table: &str| {
            db.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap()
        };
        (
            count("decisions"),
            count("problem_records"),
            count("quality_constraints"),
        )
    }

    fn seed_human_memory(directory: &std::path::Path) {
        let mut service = Service::open(directory).unwrap();
        service.refresh(None).unwrap();
        service
            .record_decision(RecordDecision {
                title: "Keep the observatory read-only".into(),
                status: "accepted".into(),
                reason: "Crusty reports; humans decide".into(),
                applies_to: vec!["src/lib.rs".into()],
                consequences: vec![],
                supersedes: None,
                materialize: false,
            })
            .unwrap();
        service
            .problem_record(crate::quality::ProblemInput {
                report: "The sidebar padding regressed after the last UI change".into(),
                summary: None,
                defect_family: None,
                status: "reported".into(),
                confidence: 0.8,
                scope: crate::quality::QualityScope {
                    files: vec!["src/lib.rs".into()],
                    ..Default::default()
                },
                reproduction: None,
                diagnostic_signature: Some("padding".into()),
                root_cause: None,
                fix_reference: None,
                evidence: vec![],
                related: vec![],
                provenance: "HumanReport".into(),
            })
            .unwrap();
    }

    #[test]
    fn a_database_from_a_newer_crusty_is_refused_without_touching_human_memory() {
        let directory = fixture();
        seed_human_memory(directory.path());
        let before = human_memory_counts(directory.path());
        assert!(before.0 > 0 && before.1 > 0 && before.2 > 0, "{before:?}");

        let index = directory
            .path()
            .join(".rust-repo-intelligence/index.sqlite3");
        let db = Connection::open(&index).unwrap();
        db.execute(
            "UPDATE metadata SET value='99' WHERE key='schema_version'",
            [],
        )
        .unwrap();
        drop(db);

        let message = match Service::open(directory.path()) {
            Ok(_) => panic!("a newer schema must not be silently rewritten"),
            Err(error) => format!("{error:#}"),
        };
        assert!(
            message.contains("newer Crusty") && message.contains("No data was modified"),
            "unexpected error: {message}"
        );
        assert_eq!(
            human_memory_counts(directory.path()),
            before,
            "refusing to open must leave every human-authored row intact"
        );
    }

    #[test]
    fn an_incompatible_older_schema_rebuilds_derived_data_and_keeps_human_memory() {
        let directory = fixture();
        seed_human_memory(directory.path());
        let before = human_memory_counts(directory.path());

        let index = directory
            .path()
            .join(".rust-repo-intelligence/index.sqlite3");
        let db = Connection::open(&index).unwrap();
        db.execute(
            "UPDATE metadata SET value='2' WHERE key='schema_version'",
            [],
        )
        .unwrap();
        let nodes_before: i64 = db
            .query_row("SELECT COUNT(*) FROM nodes", [], |row| row.get(0))
            .unwrap();
        assert!(nodes_before > 0, "fixture should have indexed symbols");
        drop(db);

        let service = Service::open(directory.path())
            .expect("an older schema is rebuildable, not a hard failure");
        assert_eq!(
            human_memory_counts(directory.path()),
            before,
            "decisions, problem records, and quality constraints are not derived data"
        );
        let nodes_after: i64 = service
            .db
            .query_row("SELECT COUNT(*) FROM nodes", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            nodes_after, 0,
            "derived symbol rows are discarded for reindex"
        );
    }

    #[test]
    fn changed_files_cover_deletions_renames_no_prefix_and_crlf_headers() {
        let deletion = "diff --git a/src/gone.rs b/src/gone.rs\n\
             deleted file mode 100644\n\
             --- a/src/gone.rs\n\
             +++ /dev/null\n\
             @@ -1,2 +0,0 @@\n\
             -pub fn gone() {}\n";
        assert_eq!(
            changed_files_in_diff(deletion),
            BTreeSet::from(["src/gone.rs".to_owned()]),
            "a deleted file is a changed file"
        );

        let addition = "diff --git a/src/new.rs b/src/new.rs\n\
             new file mode 100644\n\
             --- /dev/null\n\
             +++ b/src/new.rs\n\
             @@ -0,0 +1 @@\n\
             +pub fn added() {}\n";
        assert_eq!(
            changed_files_in_diff(addition),
            BTreeSet::from(["src/new.rs".to_owned()])
        );

        let rename = "diff --git a/src/old.rs b/src/new.rs\n\
             similarity index 100%\n\
             rename from src/old.rs\n\
             rename to src/new.rs\n";
        assert_eq!(
            changed_files_in_diff(rename),
            BTreeSet::from(["src/new.rs".to_owned(), "src/old.rs".to_owned()]),
            "a pure rename has no +++ line but touches two paths"
        );

        let no_prefix = "diff --git src/plain.rs src/plain.rs\n\
             --- src/plain.rs\n\
             +++ src/plain.rs\n\
             @@ -1 +1 @@\n\
             +pub fn plain() {}\n";
        assert_eq!(
            changed_files_in_diff(no_prefix),
            BTreeSet::from(["src/plain.rs".to_owned()]),
            "--no-prefix output carries no a/ or b/ prefix"
        );

        let crlf = "diff --git a/src/lib.rs b/src/lib.rs\r\n\
             --- a/src/lib.rs\r\n\
             +++ b/src/lib.rs\r\n\
             @@ -1 +1 @@\r\n";
        assert_eq!(
            changed_files_in_diff(crlf),
            BTreeSet::from(["src/lib.rs".to_owned()]),
            "a carriage return must not survive into the path"
        );

        let timestamped = "--- a/src/lib.rs\t2026-08-27 10:00:00.000000000 +0000\n\
             +++ b/src/lib.rs\t2026-08-27 10:05:00.000000000 +0000\n\
             @@ -1 +1 @@\n";
        assert_eq!(
            changed_files_in_diff(timestamped),
            BTreeSet::from(["src/lib.rs".to_owned()]),
            "POSIX diff appends a tab and a timestamp to the header path"
        );
    }

    #[test]
    fn diff_headers_inside_added_content_are_not_changed_files() {
        // This repository's own tests embed diffs as string fixtures, so added
        // content routinely contains text that looks exactly like a file header.
        let diff = "diff --git a/src/tests.rs b/src/tests.rs\n\
             --- a/src/tests.rs\n\
             +++ b/src/tests.rs\n\
             @@ -1,0 +1,3 @@\n\
             +let fixture = \"\\\n\
             +++ b/src/attacker_controlled.rs\n\
             +\";\n";
        assert_eq!(
            changed_files_in_diff(diff),
            BTreeSet::from(["src/tests.rs".to_owned()]),
            "a header-shaped line inside a hunk body is content, not a file header"
        );
    }
    #[test]
    fn checkpoint_references_cannot_escape_the_checkpoint_namespace() {
        validate_checkpoint_ref("refs/codex/checkpoints/cp_abc123").expect("a normal checkpoint");
        validate_checkpoint_ref("refs/codex/checkpoints/feature.v2")
            .expect("dots are legal in refs");
        // `.` was in the allowed character set, so `..` walked straight out of
        // the namespace and into `git branch`.
        for escape in [
            "refs/codex/checkpoints/../../HEAD",
            "refs/codex/checkpoints/../refs/heads/main",
            "refs/codex/checkpoints/./x",
            "refs/heads/main",
        ] {
            assert!(
                validate_checkpoint_ref(escape).is_err(),
                "{escape} must be rejected"
            );
        }
    }

    #[test]
    fn symbol_relations_answer_callers_and_implementations_with_locations() {
        let directory = tempdir().unwrap();
        fs::write(
            directory.path().join("Cargo.toml"),
            "[package]\nname='relations'\nversion='0.1.0'\nedition='2024'\n",
        )
        .unwrap();
        fs::create_dir(directory.path().join("src")).unwrap();
        fs::write(
            directory.path().join("src/lib.rs"),
            "pub trait Store { fn put(&self); }\n\
             pub struct Disk;\n\
             impl Store for Disk { fn put(&self) {} }\n\
             pub fn caller(disk: &Disk) { disk.put(); }\n",
        )
        .unwrap();
        let mut service = Service::open(directory.path()).unwrap();
        service.refresh(None).unwrap();

        let definition = service
            .symbol_relations("caller", "definition", 10)
            .unwrap();
        let first = &definition["definitions"][0];
        assert!(
            first["line"].as_i64().is_some_and(|line| line > 0),
            "a definition must carry a line number, not just a filename: {first}"
        );
        assert!(
            first["location"]
                .as_str()
                .is_some_and(|value| value.contains("src/lib.rs:")),
            "{first}"
        );

        let implementations = service
            .symbol_relations("Store", "implementations", 10)
            .unwrap();
        assert!(implementations["results"].is_array());
        assert!(implementations["channel"].is_string());

        // A misspelled relation must be an error even when the symbol is
        // unknown, so the agent learns which parameter was wrong.
        let error = service
            .symbol_relations("no_such_symbol", "siblings", 10)
            .expect_err("unknown relation");
        assert!(format!("{error:#}").contains("unsupported relation"));

        // An unknown symbol is not an error, but it must say why it is empty.
        let empty = service
            .symbol_relations("no_such_symbol", "callers", 10)
            .unwrap();
        assert_eq!(empty["results"].as_array().map(Vec::len), Some(0));
        assert!(
            empty["note"]
                .as_str()
                .is_some_and(|note| note.contains("index.refresh")),
            "an empty result must distinguish itself from a stale index"
        );
    }
    #[test]
    fn non_ascii_queries_find_their_documents() {
        // The tokenizer regex was `[A-Za-z_][A-Za-z0-9_]*`, so CJK, Cyrillic,
        // and Greek queries produced no terms, an empty FTS MATCH, and an empty
        // result with no error at all.
        assert!(!terms("日本語").is_empty(), "CJK must tokenize");
        assert!(!terms("документация").is_empty(), "Cyrillic must tokenize");
        assert!(
            !terms("café résumé").is_empty(),
            "accented Latin must tokenize"
        );
        // ASCII behaviour is unchanged.
        assert_eq!(
            terms("refresh_if_stale"),
            vec!["refresh_if_stale".to_owned()]
        );
        assert_eq!(
            terms("Service::open"),
            vec!["Service".to_owned(), "open".to_owned()]
        );
        // Embeddings were blind to the same input.
        assert!(!embedding_tokens("日本語ドキュメント").is_empty());
        assert!(dot_product(&embed_text("документация"), &embed_text("документация")) > 0.99);
    }

    #[test]
    fn an_unreadable_source_file_is_reported_rather_than_indexed_as_empty() {
        let directory = fixture();
        // Invalid UTF-8 in a .rs file. `unwrap_or_default` turned this into an
        // empty file: zero symbols, no error, and `index_inputs` still hashed it
        // as healthy and current.
        fs::write(
            directory.path().join("src/broken.rs"),
            [0xFF, 0xFE, 0x00, 0x41],
        )
        .unwrap();
        let mut service = Service::open(directory.path()).unwrap();
        service.refresh(None).unwrap();

        let status = service.status().unwrap();
        let unreadable = status["unreadable_inputs"].as_array().unwrap();
        assert!(
            unreadable.iter().any(|path| path == "src/broken.rs"),
            "the skipped file must be named: {unreadable:?}"
        );
        assert!(
            status["degraded_areas"]
                .as_array()
                .unwrap()
                .iter()
                .any(|area| area.as_str().is_some_and(|area| area.contains("UTF-8"))),
            "the degradation must be visible in status"
        );
    }

    #[test]
    fn repeated_relationships_collapse_to_one_edge() {
        let directory = tempdir().unwrap();
        fs::write(
            directory.path().join("Cargo.toml"),
            "[package]\nname='edges'\nversion='0.1.0'\nedition='2024'\n",
        )
        .unwrap();
        fs::create_dir(directory.path().join("src")).unwrap();
        // `helper` is called three times from one function.
        fs::write(
            directory.path().join("src/lib.rs"),
            "pub fn helper() -> u8 { 1 }\n\
             pub fn caller() -> u8 { helper() + helper() + helper() }\n",
        )
        .unwrap();
        let mut service = Service::open(directory.path()).unwrap();
        service.refresh(None).unwrap();

        // `INSERT OR IGNORE INTO edges` could never ignore anything: the table
        // had no uniqueness constraint, so every repeat inserted another row and
        // the graph grew on each refresh.
        let duplicates: i64 = service
            .db
            .query_row(
                "SELECT COUNT(*) FROM (SELECT src,dst,kind,provenance FROM edges GROUP BY src,dst,kind,provenance HAVING COUNT(*)>1)",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(duplicates, 0, "an edge identity must appear once");

        let before: i64 = service
            .db
            .query_row("SELECT COUNT(*) FROM edges", [], |row| row.get(0))
            .unwrap();
        service.refresh(None).unwrap();
        let after: i64 = service
            .db
            .query_row("SELECT COUNT(*) FROM edges", [], |row| row.get(0))
            .unwrap();
        assert_eq!(before, after, "a second refresh must not grow the graph");
    }

    #[test]
    fn deeply_nested_input_degrades_instead_of_overflowing_the_stack() {
        // `syn::parse_file` is recursive descent with no depth limit, and a
        // stack overflow aborts the process rather than returning an error.
        let pathological = format!("fn f() {{ {} {} }}", "(".repeat(5_000), ")".repeat(5_000));
        assert!(
            parse_rust_file(&pathological).is_none(),
            "pathological nesting must be refused, not parsed"
        );
        // Ordinary nesting still parses.
        assert!(
            parse_rust_file(
                "fn f() -> u8 { (((1)))
}"
            )
            .is_some()
        );
    }

    #[test]
    fn check_results_are_bounded_and_carry_structured_diagnostics() {
        // `check` returned the entire uncapped stderr as prose, so a failing
        // `cargo test --all-targets` dumped its whole output into the response
        // and the compiler-error workflow got text instead of file:line.
        let stream = r#"{"reason":"compiler-message","message":{"level":"error","code":{"code":"E0308"},"message":"mismatched types","spans":[{"is_primary":true,"file_name":"src/lib.rs","line_start":42,"column_start":9}]}}
{"reason":"compiler-artifact","package_id":"x"}
{"reason":"compiler-message","message":{"level":"note","message":"ignored","spans":[]}}"#;
        let diagnostics = cargo_diagnostics(stream);
        assert_eq!(
            diagnostics.len(),
            1,
            "only errors and warnings are reported"
        );
        assert_eq!(diagnostics[0]["level"], "error");
        assert_eq!(diagnostics[0]["code"], "E0308");
        assert_eq!(diagnostics[0]["location"], "src/lib.rs:42");
        assert_eq!(diagnostics[0]["line"], 42);
    }
}

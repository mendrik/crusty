//! Repository indexing and change-impact services used by the MCP binary.
//! Semantic facts returned by a static scan are explicitly marked as such.

pub mod analysis;
mod auto_refresh;
pub mod cleanup;
pub mod coordination;
pub mod dashboard;
pub mod delivery;
pub mod domain;
mod execution;
pub mod github;
pub mod guidance;
mod index_build;
pub mod live_semantics;
mod memory_write;
pub mod observatory;
pub mod performance;
mod quality;
mod relevance;
mod response_budget;
mod rust_analyzer;
mod steering;
mod validation_diff;

use response_budget::Section;
pub mod verification;

pub use auto_refresh::AutoRefresh;
pub(crate) use rust_analyzer::SemanticBackend;
pub use rust_analyzer::SemanticWarmer;
use rust_analyzer::{AUTOSTART_ENV, autostart_enabled, rust_analyzer_version};
pub use steering::{RecordSteering, RetireSteering, STEERING_LIST_STATUSES, STEERING_STATUSES};
pub use validation_diff::{DiffSource, DiffTarget};

pub use quality::{
    ProblemInput, QualityConstraintInput, QualityScope, ValidationOutcomeInput, ValidationRecipe,
};

use anyhow::{Context, Result, bail, ensure};
use chrono::Utc;
use fs2::FileExt;
use regex::Regex;
use rusqlite::{Connection, OptionalExtension, params};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
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
/// Superseded index generations retained for publishing history.
const MAX_RETAINED_GENERATIONS: i64 = 20;
/// Upper bound on full-text decision hits examined before status filtering;
/// matches the largest list limit a caller can request.
const MAX_DECISION_HITS: usize = 200;
const SCHEMA_VERSION: &str = "8";
/// Version 9 records every file contributing an IMPLEMENTS edge; bumping it
/// rebuilds older stores once so incremental refreshes can rely on that.
const INDEXER_VERSION: &str = "9";
const WATCHER_ENV: &str = "RUST_REPO_INTELLIGENCE_ENABLE_WATCHER";
const FEATURES_ENV: &str = "RUST_REPO_INTELLIGENCE_FEATURES";
const CHECKPOINT_REF_PREFIX: &str = "refs/codex/checkpoints";
const EMBEDDING_MODEL: &str = "subword-hash-v1";
const EMBEDDING_DIMENSIONS: usize = 192;
const SYMBOL_CARD_VERSION: &str = "symbol-card-v1";
const RRF_K: f64 = 60.0;
/// Upper bound on ids bound into one `IN (...)` list.
const SQL_CHUNK: usize = 500;
/// Full-text entity types owned by human and project memory rather than by
/// the source index.
const MEMORY_SEARCH_ENTITIES: [&str; 5] = [
    "decision",
    "steering",
    "work",
    "problem",
    "quality_constraint",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Revision {
    pub head: Option<String>,
    pub dirty: bool,
    pub workspace_digest: String,
}

/// What one refresh did, before it is summarised for the refresh task result.
#[derive(Debug, Clone, Copy)]
pub struct RefreshOutcome {
    /// `full`, `incremental`, `unchanged`, or `git_history`.
    pub mode: &'static str,
    /// Whether a new generation was published.
    pub published: bool,
    /// Inputs re-indexed: every input for a full rebuild, the changed ones otherwise.
    pub changed_inputs: usize,
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
    /// Vectors computed from scratch.
    embedded: usize,
    /// Vectors kept or copied from an identical card.
    reused: usize,
    /// Symbol cards built; incremental refreshes build them only for nodes an
    /// edit can affect.
    cards: usize,
}

/// A parsed symbol ready to be stored as a node.
struct IndexedSymbol {
    kind: String,
    canonical_name: String,
    crate_name: Option<String>,
    file: String,
    start_line: usize,
    end_line: usize,
    visibility: String,
    content_hash: String,
    parser: &'static str,
}

/// Source files read at most once during one indexing pass. Bulk passes used
/// to re-read a file for every symbol it defines.
#[derive(Default)]
struct SourceCache {
    files: HashMap<String, Option<Vec<String>>>,
}

impl SourceCache {
    fn lines(&mut self, root: &Path, file: &str) -> Option<&[String]> {
        self.files
            .entry(file.to_owned())
            .or_insert_with(|| {
                read_source_text(&root.join(file))
                    .map(|text| text.lines().map(str::to_owned).collect())
            })
            .as_deref()
    }

    /// The node's indexed line range as `source_slice` reads it.
    fn slice(&mut self, root: &Path, node: &Node) -> Option<String> {
        let lines = self.lines(root, &node.file)?;
        let end = node.end_line.min(lines.len());
        let start = node.start_line.saturating_sub(1).min(end);
        let source = lines[start..end].join("\n");
        Some(if source.len() > MAX_SLICE_BYTES {
            trim_text(&source, MAX_SLICE_BYTES)
        } else {
            source
        })
    }
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

/// Lifecycle states of a human architectural decision.
///
/// Only `accepted` decisions govern consultation, prepared changes, and
/// validation. `superseded` and `retired` are terminal: the record stays in the
/// ledger with its review trail, and only a new decision can take its place.
pub const DECISION_STATUSES: &[&str] = &["accepted", "superseded", "retired"];

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RecordDecision {
    pub title: String,
    /// `accepted`, `superseded`, or `retired`.
    #[serde(default = "accepted")]
    pub status: String,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub applies_to: Vec<String>,
    #[serde(default)]
    pub consequences: Vec<String>,
    /// Accepted decision IDs this decision replaces; each becomes `superseded`.
    /// A single ID is accepted as well as a list.
    #[serde(default, deserialize_with = "one_or_many")]
    pub supersedes: Vec<String>,
    /// Who records the decision. Required when `supersedes` is non-empty,
    /// because replacing a decision changes another human's record.
    #[serde(default)]
    pub recorded_by: String,
    #[serde(default)]
    pub materialize: bool,
}

/// Retires one accepted decision without replacing it.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RetireDecision {
    pub id: String,
    pub retired_by: String,
    #[serde(default)]
    pub note: String,
}

/// Accepts `null`, one string, or a list of strings, so callers written against
/// the earlier single-ID `supersedes` field keep working.
fn one_or_many<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }
    Ok(match Option::<OneOrMany>::deserialize(deserializer)? {
        None => Vec::new(),
        Some(OneOrMany::One(one)) => vec![one],
        Some(OneOrMany::Many(many)) => many,
    })
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

/// The background refresher's environment switches: whether it runs
/// (`CRUSTY_AUTO_REFRESH`) and whether it may watch the filesystem.
pub(crate) fn auto_refresh_switches() -> (bool, bool) {
    (
        auto_refresh::auto_refresh_enabled(
            std::env::var(auto_refresh::AUTO_REFRESH_ENV)
                .ok()
                .as_deref(),
        ),
        watcher_enabled(std::env::var(WATCHER_ENV).ok().as_deref()),
    )
}

/// The rust-analyzer environment switches: whether the companion is enabled
/// at server start and whether an enabled companion starts loading right away.
pub(crate) fn semantic_switches() -> (bool, bool) {
    (
        autostart_enabled(std::env::var(AUTOSTART_ENV).ok().as_deref()),
        rust_analyzer::warm_start_enabled(
            std::env::var(rust_analyzer::WARM_START_ENV).ok().as_deref(),
        ),
    )
}

/// Whether the Observatory's background refresher may watch the filesystem.
pub(crate) fn watcher_enabled(value: Option<&str>) -> bool {
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

pub struct Service {
    root: PathBuf,
    db: Connection,
    ra: Arc<SemanticBackend>,
    execution: execution::ExecutionControl,
    ra_enabled: bool,
    ra_program: PathBuf,
    embedding_cache: RefCell<Option<EmbeddingCache>>,
    last_refresh_timings: Value,
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
        // Builds use a private snapshot; only publication and memory mutations
        // acquire this writer. Bound contention between those short transactions.
        db.busy_timeout(std::time::Duration::from_secs(15))?;
        initialize_schema(&db)?;
        let ra_program = rust_analyzer::configured_program(&root);
        // MCP clients impose a short initialization deadline.  Indexing a large
        // workspace here makes the server appear unavailable. Cache creation is
        // lazy, and the optional rust-analyzer companion is shared per process
        // and warmed in the background (see `rust_analyzer::SemanticWarmer`).
        Ok(Self {
            root,
            db,
            ra: Arc::default(),
            execution: execution::ExecutionControl::default(),
            // Off until `semantic.enable`; `with_backend` adopts the shared state.
            ra_enabled: false,
            ra_program,
            embedding_cache: RefCell::new(None),
            last_refresh_timings: Value::Null,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn with_backend(mut self, backend: Arc<SemanticBackend>) -> Self {
        self.ra_enabled = backend.is_enabled();
        self.ra = backend;
        self
    }

    pub(crate) fn with_execution(mut self, execution: execution::ExecutionControl) -> Self {
        self.execution = execution;
        self
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
        self.reindex_unlocked().map(|_| ())
    }

    fn reindex_unlocked(&mut self) -> Result<RefreshOutcome> {
        let inputs = self.with_index_build(|service| service.reindex_inner())?;
        Ok(RefreshOutcome {
            mode: "full",
            published: true,
            changed_inputs: inputs,
        })
    }

    /// Rebuilds every index table and returns the number of indexed inputs.
    fn reindex_inner(&mut self) -> Result<usize> {
        // Cargo may materialize Cargo.lock on first metadata resolution. Establish
        // the indexed snapshot only after that deterministic side effect.
        let cargo = cargo_metadata(&self.root)?;
        let (inputs, worktree) = index_inputs_with_worktree(&self.root);
        let revision = self.revision_from_inputs(&inputs);
        let semantic = self.semantic_snapshot(&inputs);
        let digest = revision.workspace_digest.as_str();
        // Node ids are reassigned below. Vectors are content-addressed by their
        // symbol card, so every unchanged card keeps its vector.
        let reusable = self.reusable_embeddings(None)?;
        self.db
            .execute("DELETE FROM edges WHERE provenance = 'StaticIndex'", [])?;
        self.db
            .execute("DELETE FROM edges WHERE provenance = 'RustAnalyzer'", [])?;
        self.db.execute("DELETE FROM nodes", [])?;
        self.db.execute("DELETE FROM commits", [])?;
        self.db.execute("DELETE FROM commit_files", [])?;
        self.db.execute("DELETE FROM co_changes", [])?;
        self.db.execute("DELETE FROM documents", [])?;
        self.db.execute("DELETE FROM symbol_embeddings", [])?;
        self.db.execute("DELETE FROM lifecycle_edges", [])?;
        self.db.execute("DELETE FROM unresolved_references", [])?;
        self.db.execute("DELETE FROM use_case_nodes", [])?;
        self.db.execute("DELETE FROM use_cases", [])?;
        self.replace_cargo_tables(&cargo, digest)?;
        let packages = cargo_packages_from_metadata(&cargo);
        let nodes = self.index_source_files(&rust_files(&self.root), &packages)?;
        self.index_static_references(&nodes, None, digest)?;
        self.index_structural_edges(&nodes, None, digest)?;
        self.index_lifecycle_evidence(&nodes, &nodes, digest)?;
        self.index_documents(&all_text_files(&self.root), digest)?;
        self.index_use_cases(&nodes, digest)?;
        self.index_proposed_work(None, digest)?;
        self.index_git(digest, revision.head.as_deref())?;
        let stats = self.rebuild_symbol_embeddings(&nodes, &semantic.id, &reusable)?;
        self.record_embedding_stats(&stats)?;
        self.rebuild_search_index()?;
        // Decision targets resolve through the full-text index, so they are
        // relinked only once it describes the new nodes.
        self.relink_decision_targets()?;
        ensure!(
            index_inputs(&self.root) == inputs,
            "workspace changed during full indexing; retry against a stable snapshot"
        );
        self.record_index_state(&revision, &inputs)?;
        self.record_worktree_inputs(&worktree)?;
        Ok(inputs.len())
    }

    /// Replaces the Cargo package, dependency, target, and feature tables.
    fn replace_cargo_tables(&self, cargo: &Value, revision: &str) -> Result<()> {
        self.db.execute("DELETE FROM packages", [])?;
        self.db.execute("DELETE FROM package_dependencies", [])?;
        self.db.execute("DELETE FROM package_targets", [])?;
        self.db.execute("DELETE FROM package_features", [])?;
        for package in cargo_packages_from_metadata(cargo) {
            self.db.execute("INSERT OR REPLACE INTO packages(name, manifest_path, metadata, revision) VALUES (?1, ?2, ?3, ?4)", params![package.0, package.1, package.2, revision])?;
        }
        for (source, target, context) in cargo_dependency_edges_from_metadata(cargo) {
            self.db.execute(
                "INSERT OR REPLACE INTO package_dependencies(source, target, context_json, revision) VALUES (?1, ?2, ?3, ?4)",
                params![source, target, context, revision],
            )?;
        }
        self.index_cargo_matrix(cargo, revision)
    }

    /// Workspace packages as last indexed, in Cargo metadata order. Incremental
    /// refreshes read them here instead of re-running `cargo metadata`.
    fn stored_packages(&self) -> Result<Vec<(String, String, String)>> {
        Ok(self
            .db
            .prepare("SELECT name,COALESCE(manifest_path,''),COALESCE(metadata,'') FROM packages ORDER BY rowid")?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Re-attributes indexed files to crates after a Cargo change and returns
    /// the nodes whose crate changed.
    fn reassign_crates(&self) -> Result<BTreeSet<i64>> {
        let packages = self.stored_packages()?;
        let mut changed = BTreeSet::new();
        for node in self.all_nodes()? {
            let crate_name = crate_for_file(&packages, &self.root.join(&node.file));
            if crate_name != node.crate_name {
                self.db.execute(
                    "UPDATE nodes SET crate_name=?1 WHERE id=?2",
                    params![crate_name, node.id],
                )?;
                changed.insert(node.id);
            }
        }
        Ok(changed)
    }

    /// Refresh only the portions invalidated by changed inputs.
    ///
    /// Only an empty input state or an indexer-version change rebuilds
    /// everything. Source edits re-index the changed files and re-resolve the
    /// files whose references they can affect; Cargo edits replace the package
    /// tables and re-attribute crates; a changed semantic profile (target,
    /// features, build environment, rust-analyzer version) drops only the
    /// rust-analyzer evidence recorded under the old profile, since the
    /// syntactic index does not depend on it. Documentation alone never
    /// invalidates source symbols or graph edges.
    pub fn refresh_if_stale(&mut self) -> Result<RefreshOutcome> {
        let _publisher_lease = self.acquire_publisher_lease()?;
        let previous = self.stored_inputs()?;
        if previous.is_empty() {
            return self.reindex_unlocked();
        }
        if self.metadata_value("indexer_version")?.as_deref() != Some(INDEXER_VERSION) {
            return self.reindex_unlocked();
        }
        // Every refresh reconciles against the whole workspace; the Git index
        // makes that cheap, since only dirty and untracked inputs are read.
        let (mut inputs, mut worktree) = index_inputs_with_worktree(&self.root);
        let published_head = self
            .metadata_value("git_indexed_head")?
            .filter(|head| !head.is_empty());
        let current_head = command_text(&self.root, &["rev-parse", "HEAD"]);
        let mut changed = changed_inputs(&previous, &inputs);
        // Cargo may rewrite Cargo.lock while resolving metadata; take the
        // snapshot afterwards, exactly as a full reindex does.
        let cargo = if changed.iter().any(|(_, kind)| kind == "cargo") {
            let cargo = cargo_metadata(&self.root)?;
            (inputs, worktree) = index_inputs_with_worktree(&self.root);
            changed = changed_inputs(&previous, &inputs);
            Some(cargo)
        } else {
            None
        };
        let revision = self.revision_from_inputs_and_head(&inputs, current_head);
        let semantic = self.semantic_snapshot(&inputs);
        let profile_changed = !self.active_semantic_profile_matches(&semantic)?;
        let history_head = self
            .metadata_value("git_history_head")?
            .filter(|head| !head.is_empty())
            .or(published_head);
        let history_stale = history_head != revision.head;
        let republish = self.indexed_revision().as_ref() != Some(&revision);
        if !changed.is_empty() || profile_changed || history_stale || republish {
            self.with_index_build(|service| {
                service.refresh_changed(
                    &changed,
                    &revision,
                    &inputs,
                    cargo.as_ref(),
                    history_stale,
                    profile_changed,
                )?;
                service.record_worktree_inputs(&worktree)
            })?;
            return Ok(RefreshOutcome {
                mode: "incremental",
                published: true,
                changed_inputs: changed.len(),
            });
        }
        // Nothing to publish, but a store written before the worktree set was
        // recorded still needs it for cheap freshness checks.
        if self.stored_worktree_inputs()?.as_ref() != Some(&worktree) {
            self.record_worktree_inputs(&worktree)?;
        }
        Ok(RefreshOutcome {
            mode: "unchanged",
            published: false,
            changed_inputs: 0,
        })
    }

    /// The per-input content identities recorded with the published generation.
    fn stored_inputs(&self) -> Result<BTreeMap<String, (String, String)>> {
        Ok(self
            .db
            .prepare("SELECT path, kind, content_hash FROM input_state")?
            .query_map([], |row| Ok((row.get(0)?, (row.get(1)?, row.get(2)?))))?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Inputs that differed from `HEAD` when the published generation was
    /// indexed, or `None` for a store that never recorded them.
    fn stored_worktree_inputs(&self) -> Result<Option<BTreeSet<String>>> {
        stored_worktree_inputs(&self.db)
    }

    fn record_worktree_inputs(&self, worktree: &BTreeSet<String>) -> Result<()> {
        self.db.execute(
            "INSERT OR REPLACE INTO metadata(key, value) VALUES ('indexed_worktree_inputs', ?1)",
            [serde_json::to_string(worktree)?],
        )?;
        Ok(())
    }

    fn metadata_value(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .db
            .query_row("SELECT value FROM metadata WHERE key=?1", [key], |row| {
                row.get(0)
            })
            .optional()?)
    }

    /// Applies `changed` inputs to the published index and publishes a new
    /// generation.
    ///
    /// Work is bounded by the change: changed files are re-parsed with node ids
    /// kept for symbols that still exist, syntax edges are re-derived only for
    /// the affected files, and symbol cards, lifecycle evidence, use cases,
    /// documents, TODO markers, and full-text rows are rewritten only for what
    /// those files touch. Two steps still visit the workspace without parsing
    /// it: a word scan of Rust files when the set of defined symbol names
    /// changes (a new or removed name can change how any file's references
    /// resolve), and database-only bulk statements such as re-stamping vectors
    /// with the new semantic snapshot.
    fn refresh_changed(
        &mut self,
        changed: &[(String, String)],
        revision: &Revision,
        inputs: &BTreeMap<String, (String, String)>,
        cargo: Option<&Value>,
        history_stale: bool,
        semantic_profile_changed: bool,
    ) -> Result<()> {
        let digest = revision.workspace_digest.as_str();
        let semantic = self.semantic_snapshot(inputs);
        let mut touched = BTreeSet::new();
        let mut semantic_invalidated = semantic_profile_changed;
        if let Some(cargo) = cargo {
            self.replace_cargo_tables(cargo, digest)?;
            touched.extend(self.reassign_crates()?);
            semantic_invalidated = true;
        }
        // build.rs is a Cargo input and Rust source at once.
        let source_files: BTreeSet<String> = changed
            .iter()
            .filter(|(path, kind)| kind == "source" || path.ends_with(".rs"))
            .map(|(path, _)| path.clone())
            .collect();
        let previous_ids = self.node_ids_in_files(&source_files)?;
        let reusable = self.reusable_embeddings(Some(&previous_ids))?;
        if !semantic_invalidated && !source_files.is_empty() {
            semantic_invalidated = self.exposes_shared_api(&source_files)?;
        }
        if semantic_invalidated {
            touched.extend(self.edge_endpoints("provenance='RustAnalyzer'", &[] as &[i64])?);
            self.db
                .execute("DELETE FROM edges WHERE provenance='RustAnalyzer'", [])?;
        } else {
            // Private, non-trait edits keep compiler evidence for the rest of
            // the workspace; only evidence about the edited symbols is dropped.
            for chunk in previous_ids.chunks(SQL_CHUNK) {
                let filter = format!(
                    "provenance='RustAnalyzer' AND (src IN ({0}) OR dst IN ({0}))",
                    placeholders(chunk.len())
                );
                touched.extend(self.edge_endpoints(&filter, chunk)?);
                self.db.execute(
                    &format!("DELETE FROM edges WHERE {filter}"),
                    rusqlite::params_from_iter(chunk),
                )?;
            }
            self.db.execute(
                "UPDATE edges SET revision=?1 WHERE provenance='RustAnalyzer'",
                [&semantic.id],
            )?;
        }
        if !source_files.is_empty() {
            let affected_files = self.apply_source_changes(&source_files, inputs, &mut touched)?;
            touched.extend(self.reindex_file_edges(&affected_files, digest)?);
            let affected_nodes = self.nodes_in_files(&affected_files)?;
            touched.extend(affected_nodes.iter().map(|node| node.id));
            let affected_ids = affected_nodes
                .iter()
                .map(|node| node.id)
                .collect::<Vec<_>>();
            for chunk in affected_ids.chunks(SQL_CHUNK) {
                self.db.execute(
                    &format!(
                        "DELETE FROM lifecycle_edges WHERE legacy_node IN ({})",
                        placeholders(chunk.len())
                    ),
                    rusqlite::params_from_iter(chunk),
                )?;
            }
            let all_nodes = self.all_nodes()?;
            self.index_lifecycle_evidence(&affected_nodes, &all_nodes, digest)?;
            let changed_nodes = affected_nodes
                .into_iter()
                .filter(|node| source_files.contains(&node.file))
                .collect::<Vec<_>>();
            self.index_use_cases(&changed_nodes, digest)?;
            self.db.execute(
                "DELETE FROM use_cases WHERE provenance='AgentInference' AND id NOT IN (SELECT use_case_id FROM use_case_nodes)",
                [],
            )?;
            self.delete_search_rows("node", &previous_ids.iter().map(i64::to_string).collect())?;
            self.insert_node_search_rows(&changed_nodes)?;
        }
        let document_paths = changed
            .iter()
            .filter(|(_, kind)| kind != "source")
            .map(|(path, _)| path.clone())
            .collect::<BTreeSet<_>>();
        if !document_paths.is_empty() {
            self.refresh_documents(&document_paths, digest)?;
        }
        self.db
            .execute("UPDATE documents SET revision=?1", [digest])?;
        let changed_paths = changed
            .iter()
            .map(|(path, _)| path.clone())
            .collect::<BTreeSet<_>>();
        self.index_proposed_work(Some(&changed_paths), digest)?;
        if history_stale {
            self.refresh_git(digest, revision.head.as_deref())?;
        }
        let missing: Vec<i64> = self
            .db
            .prepare(
                "SELECT id FROM nodes WHERE id NOT IN (SELECT node_id FROM symbol_embeddings)",
            )?
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        touched.extend(missing);
        let touched_nodes = self.nodes_by_ids(&touched)?;
        let mut stats = self.rebuild_symbol_embeddings(&touched_nodes, &semantic.id, &reusable)?;
        self.db.execute(
            "UPDATE symbol_embeddings SET semantic_snapshot=?1",
            [&semantic.id],
        )?;
        let total: usize =
            self.db
                .query_row("SELECT COUNT(*) FROM symbol_embeddings", [], |row| {
                    row.get(0)
                })?;
        // Vectors outside the touched set are carried forward unchanged.
        stats.reused += total.saturating_sub(touched_nodes.len());
        self.record_embedding_stats(&stats)?;
        self.refresh_memory_search_rows()?;
        self.relink_decision_targets()?;
        ensure!(
            changed_inputs_still_match(&self.root, inputs, changed),
            "workspace changed during incremental indexing; retry against a stable snapshot"
        );
        self.record_index_state(revision, inputs)
    }

    /// Publishes a generation for `inputs`. The caller records, in the same
    /// savepoint, which inputs differed from `HEAD` when they were read
    /// (`record_worktree_inputs`); freshness checks re-hash only those and the
    /// currently dirty inputs instead of the whole workspace.
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

    fn nodes_in_files(&self, files: &BTreeSet<String>) -> Result<Vec<Node>> {
        let files = files.iter().collect::<Vec<_>>();
        let mut nodes = Vec::new();
        for chunk in files.chunks(SQL_CHUNK) {
            let mut statement = self.db.prepare(&format!(
                "SELECT id,kind,canonical_name,crate_name,file,start_line,end_line,visibility,content_hash FROM nodes WHERE file IN ({}) ORDER BY file,start_line,id",
                placeholders(chunk.len())
            ))?;
            nodes.extend(
                statement
                    .query_map(rusqlite::params_from_iter(chunk), node_from_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?,
            );
        }
        Ok(nodes)
    }

    fn node_ids_in_files(&self, files: &BTreeSet<String>) -> Result<Vec<i64>> {
        Ok(self
            .nodes_in_files(files)?
            .into_iter()
            .map(|node| node.id)
            .collect())
    }

    fn nodes_by_ids(&self, ids: &BTreeSet<i64>) -> Result<Vec<Node>> {
        let ids = ids.iter().collect::<Vec<_>>();
        let mut nodes = Vec::new();
        for chunk in ids.chunks(SQL_CHUNK) {
            let mut statement = self.db.prepare(&format!(
                "SELECT id,kind,canonical_name,crate_name,file,start_line,end_line,visibility,content_hash FROM nodes WHERE id IN ({}) ORDER BY id",
                placeholders(chunk.len())
            ))?;
            nodes.extend(
                statement
                    .query_map(rusqlite::params_from_iter(chunk), node_from_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?,
            );
        }
        Ok(nodes)
    }

    /// Both endpoints of every edge matching `filter`, whose numbered
    /// parameters are bound to `values`.
    fn edge_endpoints<T: rusqlite::ToSql>(
        &self,
        filter: &str,
        values: &[T],
    ) -> Result<BTreeSet<i64>> {
        let mut endpoints = BTreeSet::new();
        let mut statement = self
            .db
            .prepare(&format!("SELECT src,dst FROM edges WHERE {filter}"))?;
        let rows = statement.query_map(rusqlite::params_from_iter(values), |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
        })?;
        for row in rows {
            let (source, target) = row?;
            endpoints.insert(source);
            endpoints.insert(target);
        }
        Ok(endpoints)
    }

    /// Whether a changed file defines public items, traits, impls, or macros,
    /// whose edits can change compiler-resolved relationships anywhere.
    fn exposes_shared_api(&self, files: &BTreeSet<String>) -> Result<bool> {
        for file in files {
            let exposed = self
                .db
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM nodes WHERE file=?1 AND (visibility='public' OR kind IN ('trait','impl','macro_rules!')))",
                    [file],
                    |row| row.get::<_, bool>(0),
                )
                .unwrap_or(true);
            if exposed {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Re-parses changed source files into the node table and returns every
    /// file whose syntax evidence must be re-derived.
    ///
    /// A symbol that still exists keeps its node id (matched by kind and
    /// canonical name, in source order), so edges from unchanged files, use
    /// cases, decision links, and vectors attached to it survive. Removed
    /// symbols are deleted with their dependent rows. Because static references
    /// resolve by short name, a name that appears or disappears can change how
    /// any file's references resolve; files mentioning such a name are added to
    /// the result, as are files whose edges pointed at a removed symbol.
    fn apply_source_changes(
        &self,
        files: &BTreeSet<String>,
        inputs: &BTreeMap<String, (String, String)>,
        touched: &mut BTreeSet<i64>,
    ) -> Result<BTreeSet<String>> {
        let packages = self.stored_packages()?;
        let mut affected = files.clone();
        let mut changed_names = BTreeSet::new();
        let mut removed = Vec::new();
        let mut unreadable: BTreeSet<String> = self
            .metadata_value("unreadable_inputs")?
            .and_then(|value| serde_json::from_str(&value).ok())
            .unwrap_or_default();
        for file in files {
            unreadable.remove(file);
            let mut previous: HashMap<(String, String), VecDeque<Node>> = HashMap::new();
            for node in self.nodes_in_files(&BTreeSet::from([file.clone()]))? {
                previous
                    .entry((node.kind.clone(), node.canonical_name.clone()))
                    .or_default()
                    .push_back(node);
            }
            let path = self.root.join(file);
            let parsed = if path.is_file() {
                self.parse_source_file(&path, &packages).unwrap_or_else(|| {
                    unreadable.insert(file.clone());
                    Vec::new()
                })
            } else {
                Vec::new()
            };
            for symbol in parsed {
                let key = (symbol.kind.clone(), symbol.canonical_name.clone());
                match previous.get_mut(&key).and_then(VecDeque::pop_front) {
                    Some(existing) => self.update_node(existing.id, &symbol)?,
                    None => {
                        changed_names.insert(short_name(&symbol.canonical_name).to_owned());
                        self.insert_node(symbol)?;
                    }
                }
            }
            for node in previous.into_values().flatten() {
                changed_names.insert(short_name(&node.canonical_name).to_owned());
                removed.push(node.id);
            }
        }
        for chunk in removed.chunks(SQL_CHUNK) {
            let marks = placeholders(chunk.len());
            let filter = format!("src IN ({marks}) OR dst IN ({marks})");
            touched.extend(self.edge_endpoints(&filter, chunk)?);
            let mut statement = self.db.prepare(&format!(
                "SELECT metadata FROM edges WHERE provenance='Syntax' AND ({filter})"
            ))?;
            let metadata = statement
                .query_map(rusqlite::params_from_iter(chunk), |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            affected.extend(metadata.iter().flat_map(|value| edge_contributors(value)));
            self.db.execute(
                &format!("DELETE FROM nodes WHERE id IN ({marks})"),
                rusqlite::params_from_iter(chunk),
            )?;
        }
        affected.extend(self.rust_files_mentioning(&changed_names, inputs)?);
        self.db.execute(
            "INSERT OR REPLACE INTO metadata(key, value) VALUES ('unreadable_inputs', ?1)",
            [serde_json::to_string(&unreadable)?],
        )?;
        Ok(affected)
    }

    /// Indexed Rust files containing any of `names` as a whole word. This is a
    /// text scan without parsing, and runs only when the set of defined symbol
    /// names changes.
    fn rust_files_mentioning(
        &self,
        names: &BTreeSet<String>,
        inputs: &BTreeMap<String, (String, String)>,
    ) -> Result<BTreeSet<String>> {
        let identifiers = names
            .iter()
            .filter(|name| {
                !name.is_empty()
                    && name
                        .chars()
                        .all(|character| character.is_alphanumeric() || character == '_')
            })
            .map(|name| regex::escape(name))
            .collect::<Vec<_>>();
        if identifiers.is_empty() {
            return Ok(BTreeSet::new());
        }
        let pattern = Regex::new(&format!(r"\b(?:{})\b", identifiers.join("|")))?;
        Ok(inputs
            .keys()
            .filter(|path| path.ends_with(".rs"))
            .filter(|path| {
                read_source_text(&self.root.join(path)).is_some_and(|text| pattern.is_match(&text))
            })
            .cloned()
            .collect())
    }

    /// Replaces the syntax edges and unresolved references derived from
    /// `files` and returns the endpoints of every edge removed or added.
    fn reindex_file_edges(
        &self,
        files: &BTreeSet<String>,
        revision: &str,
    ) -> Result<BTreeSet<i64>> {
        let mut touched = self.withdraw_implements_contributors(files)?;
        let files = files.iter().collect::<Vec<_>>();
        for chunk in files.chunks(SQL_CHUNK) {
            let marks = placeholders(chunk.len());
            let filter = format!(
                "provenance IN ('Syntax','StaticIndex') AND kind!='IMPLEMENTS' AND json_extract(metadata,'$.file') IN ({marks})"
            );
            touched.extend(self.edge_endpoints(&filter, chunk)?);
            self.db.execute(
                &format!("DELETE FROM edges WHERE {filter}"),
                rusqlite::params_from_iter(chunk),
            )?;
            self.db.execute(
                &format!("DELETE FROM unresolved_references WHERE file IN ({marks})"),
                rusqlite::params_from_iter(chunk),
            )?;
        }
        let selected = files
            .iter()
            .map(|file| (*file).clone())
            .collect::<BTreeSet<_>>();
        let nodes = self.all_nodes()?;
        self.index_static_references(&nodes, Some(&selected), revision)?;
        self.index_structural_edges(&nodes, Some(&selected), revision)?;
        for chunk in files.chunks(SQL_CHUNK) {
            let marks = placeholders(chunk.len());
            touched.extend(self.edge_endpoints(
                &format!(
                    "provenance='Syntax' AND (json_extract(metadata,'$.file') IN ({marks}) \
                     OR EXISTS(SELECT 1 FROM json_each(edges.metadata,'$.files') WHERE value IN ({marks})))"
                ),
                chunk,
            )?);
        }
        Ok(touched)
    }

    /// Removes `files` from the contributors of IMPLEMENTS edges, deleting
    /// the edges no remaining contributor yields, and returns their endpoints.
    /// Remaining contributors are unaffected files whose impl headers and name
    /// resolution are unchanged, so their evidence still stands.
    fn withdraw_implements_contributors(&self, files: &BTreeSet<String>) -> Result<BTreeSet<i64>> {
        let edges: Vec<(i64, i64, i64, String)> = self
            .db
            .prepare("SELECT id,src,dst,metadata FROM edges WHERE provenance='Syntax' AND kind='IMPLEMENTS'")?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let mut touched = BTreeSet::new();
        for (id, source, target, metadata) in edges {
            let contributors = edge_contributors(&metadata);
            if contributors.is_disjoint(files) {
                continue;
            }
            touched.extend([source, target]);
            let remaining = contributors
                .difference(files)
                .cloned()
                .collect::<BTreeSet<_>>();
            if remaining.is_empty() {
                self.db.execute("DELETE FROM edges WHERE id=?1", [id])?;
            } else {
                self.db.execute(
                    "UPDATE edges SET metadata=?1 WHERE id=?2",
                    params![contributors_metadata(&remaining).to_string(), id],
                )?;
            }
        }
        Ok(touched)
    }

    /// Replaces the stored documents for changed non-source inputs.
    fn refresh_documents(&self, paths: &BTreeSet<String>, revision: &str) -> Result<()> {
        let mut stale = BTreeSet::new();
        for path in paths {
            let ids: Vec<i64> = self
                .db
                .prepare("SELECT id FROM documents WHERE path=?1")?
                .query_map([path], |row| row.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            stale.extend(ids.iter().map(i64::to_string));
            self.db
                .execute("DELETE FROM documents WHERE path=?1", [path])?;
        }
        self.delete_search_rows("document", &stale)?;
        let existing = paths
            .iter()
            .map(|path| self.root.join(path))
            .filter(|path| path.is_file())
            .collect::<Vec<_>>();
        let inserted = self.index_documents(&existing, revision)?;
        self.insert_document_search_rows(&inserted)
    }

    /// Rebuilds the full-text index atomically.
    ///
    /// The body runs `DELETE` followed by one insert per row. Outside a
    /// transaction, a failure part-way through — a busy writer, an I/O error,
    /// a killed process — committed the delete and left the published search
    /// index empty, so every search, locate, and decision lookup silently
    /// returned nothing until the next full reindex. Only a full reindex
    /// rebuilds everything; incremental refreshes and memory writes replace
    /// their own rows.
    fn rebuild_search_index(&self) -> Result<()> {
        self.with_search_savepoint("rebuild_search_index", |service| {
            service.db.execute("DELETE FROM search_index", [])?;
            service.insert_node_search_rows(&service.all_nodes()?)?;
            let documents: Vec<i64> = service
                .db
                .prepare("SELECT id FROM documents ORDER BY id")?
                .query_map([], |row| row.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            service.insert_document_search_rows(&documents)?;
            service.insert_commit_search_rows()?;
            service.insert_memory_search_rows()
        })
    }

    /// Replaces the full-text rows of human and project memory — decisions,
    /// steerings, work items, problem records, and quality constraints — after
    /// one of them is written. Database-only: unlike a full rebuild it never
    /// re-reads source files, so recording a decision costs the same in a large
    /// workspace as in a small one.
    pub(crate) fn refresh_memory_search_rows(&self) -> Result<()> {
        self.with_search_savepoint("refresh_memory_search", |service| {
            for entity_type in MEMORY_SEARCH_ENTITIES {
                service.db.execute(
                    "DELETE FROM search_index WHERE entity_type=?1",
                    [entity_type],
                )?;
            }
            service.insert_memory_search_rows()
        })
    }

    fn with_search_savepoint(
        &self,
        name: &str,
        operation: impl FnOnce(&Self) -> Result<()>,
    ) -> Result<()> {
        self.db.execute_batch(&format!("SAVEPOINT {name}"))?;
        match operation(self) {
            Ok(()) => {
                self.db.execute_batch(&format!("RELEASE {name}"))?;
                Ok(())
            }
            Err(error) => {
                let _ = self
                    .db
                    .execute_batch(&format!("ROLLBACK TO {name}; RELEASE {name}"));
                Err(error)
            }
        }
    }

    /// Deletes the full-text rows of the given entities. The entity columns are
    /// unindexed, so their row ids are collected in one pass and deleted by id.
    fn delete_search_rows(&self, entity_type: &str, ids: &BTreeSet<String>) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let rows: Vec<i64> = self
            .db
            .prepare("SELECT rowid,entity_id FROM search_index WHERE entity_type=?1")?
            .query_map([entity_type], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?
            .filter_map(Result::ok)
            .filter(|(_, id)| ids.contains(id))
            .map(|(rowid, _)| rowid)
            .collect();
        for rowid in rows {
            self.db
                .execute("DELETE FROM search_index WHERE rowid=?1", [rowid])?;
        }
        Ok(())
    }

    fn insert_node_search_rows(&self, nodes: &[Node]) -> Result<()> {
        let mut sources = SourceCache::default();
        for node in nodes {
            let body = sources
                .slice(&self.root, node)
                .map(|source| trim_text(&source, MAX_SEARCH_BODY_BYTES))
                .unwrap_or_default();
            self.db.execute(
                "INSERT INTO search_index(entity_type,entity_id,title,path,body) VALUES ('node',?1,?2,?3,?4)",
                params![node.id.to_string(), node.canonical_name, node.file, body],
            )?;
        }
        Ok(())
    }

    fn insert_document_search_rows(&self, ids: &[i64]) -> Result<()> {
        for id in ids {
            let (path, text): (String, String) =
                self.db
                    .query_row("SELECT path,text FROM documents WHERE id=?1", [id], |row| {
                        Ok((row.get(0)?, row.get(1)?))
                    })?;
            self.db.execute(
                "INSERT INTO search_index(entity_type,entity_id,title,path,body) VALUES ('document',?1,?2,?2,?3)",
                params![id.to_string(), path, trim_text(&text, MAX_SEARCH_BODY_BYTES)],
            )?;
        }
        Ok(())
    }

    fn insert_commit_search_rows(&self) -> Result<()> {
        self.db.execute(
            "INSERT INTO search_index(entity_type,entity_id,title,path,body) SELECT 'commit',hash,subject,'',author FROM commits",
            [],
        )?;
        Ok(())
    }

    fn insert_memory_search_rows(&self) -> Result<()> {
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
        quality::append_search_index(&self.db)?;
        Ok(())
    }

    fn search_hits(
        &self,
        query: &[String],
        entity_type: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Value>> {
        Ok(self
            .search_candidates(query, entity_type, limit)?
            .into_iter()
            .map(|(hit, _, _)| hit)
            .collect())
    }

    /// Full-text hits that are about the query rather than sharing one
    /// incidental word with it: each must match enough distinct meaningful
    /// terms and rank close to the best hit. Consultation uses this; broad
    /// discovery keeps `search_hits`.
    fn relevant_hits(
        &self,
        query: &[String],
        entity_type: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Value>> {
        let matcher = relevance::TermMatcher::new(query);
        if matcher.is_empty() {
            return Ok(Vec::new());
        }
        let candidates =
            self.search_candidates(query, entity_type, limit.saturating_mul(4).max(16))?;
        Ok(relevance::retain_relevant(candidates, &matcher, limit))
    }

    /// Ranked FTS rows as `(hit, bm25, matchable text)`.
    fn search_candidates(
        &self,
        query: &[String],
        entity_type: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(Value, f64, String)>> {
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
                let title: String = row.get(2)?;
                let path: String = row.get(3)?;
                let body: String = row.get(4)?;
                let score: f64 = row.get(5)?;
                let text = format!("{title} {path} {body}");
                Ok((
                    json!({
                        "entity_type": row.get::<_, String>(0)?,
                        "entity_id": row.get::<_, String>(1)?,
                        "title": title,
                        "path": path,
                        "evidence": trim_text(&body, 420),
                        "score": score,
                        "provenance": "SQLiteFTS5",
                    }),
                    score,
                    text,
                ))
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    fn search_contract_artifacts(&self, query: &str, limit: usize) -> Result<Vec<Value>> {
        Ok(self
            .contract_candidates(&terms(query), limit)?
            .into_iter()
            .map(|(hit, _, _)| hit)
            .collect())
    }

    /// Runtime contracts that are about `query`; see `relevant_hits`.
    fn relevant_contract_artifacts(&self, query: &str, limit: usize) -> Result<Vec<Value>> {
        let query = terms(query);
        let matcher = relevance::TermMatcher::new(&query);
        if matcher.is_empty() {
            return Ok(Vec::new());
        }
        let candidates = self.contract_candidates(&query, limit.saturating_mul(4).max(16))?;
        Ok(relevance::retain_relevant(candidates, &matcher, limit))
    }

    fn contract_candidates(
        &self,
        query: &[String],
        limit: usize,
    ) -> Result<Vec<(Value, f64, String)>> {
        if query.iter().all(String::is_empty) {
            return Ok(Vec::new());
        }
        let mut statement = self.db.prepare(
            "SELECT d.kind,d.path,d.text,bm25(search_index,0.0,0.0,10.0,4.0,1.0) AS score \
             FROM search_index JOIN documents d ON search_index.entity_type='document' AND search_index.entity_id=CAST(d.id AS TEXT) \
             WHERE search_index MATCH ?1 AND d.kind='runtime_contract' ORDER BY score LIMIT ?2",
        )?;
        Ok(statement
            .query_map(params![fts_query(query), limit as i64], |row| {
                let path: String = row.get(1)?;
                let text: String = row.get(2)?;
                let score: f64 = row.get(3)?;
                let matchable = format!("{path} {text}");
                Ok((
                    json!({
                        "kind": row.get::<_, String>(0)?,
                        "path": path,
                        "evidence": trim_text(&text, 420),
                        "score": score,
                        "provenance": "SQLiteFTS5",
                        "verification": "Search evidence only; validate the artifact and its runtime consumer separately.",
                    }),
                    score,
                    matchable,
                ))
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

    fn symbol_card(&self, node: &Node, sources: &mut SourceCache) -> Result<SymbolCard> {
        let source = sources
            .slice(&self.root, node)
            .map(|source| trim_text(&source, 8_000))
            .unwrap_or_default();
        let relationships = self
            .db
            .prepare_cached(
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

    /// Vectors of the current embedding model keyed by symbol-card hash,
    /// optionally limited to the given nodes. Vectors are content-addressed: a
    /// card with the same hash has the same vector regardless of node id.
    fn reusable_embeddings(&self, nodes: Option<&[i64]>) -> Result<HashMap<String, Vec<u8>>> {
        let base = format!(
            "SELECT content_hash,vector FROM symbol_embeddings WHERE model='{EMBEDDING_MODEL}' AND dimensions={EMBEDDING_DIMENSIONS}"
        );
        let mut reusable = HashMap::new();
        let mut collect = |sql: &str, ids: &[i64]| -> Result<()> {
            let mut statement = self.db.prepare(sql)?;
            let rows = statement.query_map(rusqlite::params_from_iter(ids), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
            })?;
            for row in rows {
                let (hash, vector) = row?;
                reusable.insert(hash, vector);
            }
            Ok(())
        };
        match nodes {
            None => collect(&base, &[])?,
            Some(ids) => {
                for chunk in ids.chunks(SQL_CHUNK) {
                    collect(
                        &format!("{base} AND node_id IN ({})", placeholders(chunk.len())),
                        chunk,
                    )?;
                }
            }
        }
        Ok(reusable)
    }

    /// Builds symbol cards for `nodes` and stores their vectors. A node whose
    /// stored card hash is unchanged keeps its row; otherwise a vector stored
    /// under the same card hash in `reusable` is reused, and only genuinely
    /// new cards are embedded.
    fn rebuild_symbol_embeddings(
        &self,
        nodes: &[Node],
        snapshot_id: &str,
        reusable: &HashMap<String, Vec<u8>>,
    ) -> Result<EmbeddingBuildStats> {
        *self.embedding_cache.borrow_mut() = None;
        let mut stats = EmbeddingBuildStats {
            cards: nodes.len(),
            ..EmbeddingBuildStats::default()
        };
        let mut sources = SourceCache::default();
        for node in nodes {
            let card = self.symbol_card(node, &mut sources)?;
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
            let vector = match reusable.get(&card.hash) {
                Some(vector) => {
                    stats.reused += 1;
                    vector.clone()
                }
                None => {
                    stats.embedded += 1;
                    encode_vector(&embed_text(&card.text))
                }
            };
            self.db.execute(
                "INSERT INTO symbol_embeddings(node_id,model,dimensions,vector,content_hash,semantic_snapshot) \
                 VALUES (?1,?2,?3,?4,?5,?6) \
                 ON CONFLICT(node_id) DO UPDATE SET model=excluded.model,dimensions=excluded.dimensions,vector=excluded.vector,content_hash=excluded.content_hash,semantic_snapshot=excluded.semantic_snapshot",
                params![node.id,EMBEDDING_MODEL,EMBEDDING_DIMENSIONS,vector,card.hash,snapshot_id],
            )?;
        }
        Ok(stats)
    }

    fn record_embedding_stats(&self, stats: &EmbeddingBuildStats) -> Result<()> {
        for (key, value) in [
            ("embedding_model", EMBEDDING_MODEL.to_owned()),
            ("embedding_dimensions", EMBEDDING_DIMENSIONS.to_string()),
            ("embedding_card_version", SYMBOL_CARD_VERSION.to_owned()),
            ("embedding_recomputed", stats.embedded.to_string()),
            ("embedding_reused", stats.reused.to_string()),
            ("embedding_cards_built", stats.cards.to_string()),
            ("embedding_updated_at", Utc::now().to_rfc3339()),
        ] {
            self.db.execute(
                "INSERT OR REPLACE INTO metadata(key,value) VALUES (?1,?2)",
                params![key, value],
            )?;
        }
        Ok(())
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
            self.matching_steerings(query, steering::SteeringStatus::Active, 12, false)?,
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
        Ok(response_budget::bound(
            json!({
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
            }),
            token_budget,
        ))
    }

    /// Re-reads Git history and records the head it describes. History has its
    /// own head marker: refreshing it alone never claims that the published
    /// symbol generation matches the new head.
    fn refresh_git(&self, revision: &str, head: Option<&str>) -> Result<()> {
        self.db.execute("DELETE FROM commits", [])?;
        self.db.execute("DELETE FROM commit_files", [])?;
        self.db.execute("DELETE FROM co_changes", [])?;
        self.db
            .execute("DELETE FROM search_index WHERE entity_type='commit'", [])?;
        self.index_git(revision, head)?;
        self.insert_commit_search_rows()
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
            // An unreadable or non-UTF-8 file is recorded and skipped. Reading
            // it as an empty string silently removed every one of its symbols
            // while `index_inputs` still hashed it as healthy and current.
            let Some(symbols) = self.parse_source_file(path, packages) else {
                unreadable.push(relative(&self.root, path));
                continue;
            };
            for symbol in symbols {
                nodes.push(self.insert_node(symbol)?);
            }
        }
        // Surfaced by `status()` as a degraded area rather than being silent.
        self.db.execute(
            "INSERT OR REPLACE INTO metadata(key, value) VALUES ('unreadable_inputs', ?1)",
            [serde_json::to_string(&unreadable)?],
        )?;
        Ok(nodes)
    }

    /// Parses one Rust file into node rows, or `None` when it cannot be read.
    fn parse_source_file(
        &self,
        path: &Path,
        packages: &[(String, String, String)],
    ) -> Option<Vec<IndexedSymbol>> {
        let relative = relative(&self.root, path);
        let text = read_source_text(path)?;
        let lines: Vec<_> = text.lines().collect();
        let crate_name = crate_for_file(packages, path);
        let parsed = parse_rust_symbols(&text).unwrap_or_else(|| regex_symbols(&lines));
        Some(
            parsed
                .into_iter()
                .map(|symbol| {
                    let start = symbol.start_line.max(1).min(lines.len().max(1));
                    let end = symbol.end_line.max(start).min(lines.len());
                    let mut canonical_parts =
                        vec![relative.trim_end_matches(".rs").replace('/', "::")];
                    canonical_parts.extend(symbol.scope);
                    canonical_parts.push(symbol.name);
                    let content_hash = format!(
                        "b3:{}",
                        blake3::hash(lines[start.saturating_sub(1)..end].join("\n").as_bytes())
                            .to_hex()
                    );
                    IndexedSymbol {
                        kind: symbol.kind,
                        canonical_name: canonical_parts.join("::"),
                        crate_name: crate_name.clone(),
                        file: relative.clone(),
                        start_line: start,
                        end_line: end,
                        visibility: symbol.visibility,
                        content_hash,
                        parser: symbol.parser,
                    }
                })
                .collect(),
        )
    }

    fn insert_node(&self, symbol: IndexedSymbol) -> Result<Node> {
        self.db.execute("INSERT INTO nodes(kind, canonical_name, crate_name, file, start_line, end_line, visibility, content_hash, metadata) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)", params![symbol.kind, symbol.canonical_name, symbol.crate_name, symbol.file, symbol.start_line, symbol.end_line, symbol.visibility, symbol.content_hash, json!({"parser":symbol.parser}).to_string()])?;
        Ok(Node {
            id: self.db.last_insert_rowid(),
            kind: symbol.kind,
            canonical_name: symbol.canonical_name,
            crate_name: symbol.crate_name,
            file: symbol.file,
            start_line: symbol.start_line,
            end_line: symbol.end_line,
            visibility: symbol.visibility,
            content_hash: symbol.content_hash,
        })
    }

    /// Moves an existing node to its re-parsed location and content, keeping
    /// its id and everything attached to it.
    fn update_node(&self, id: i64, symbol: &IndexedSymbol) -> Result<()> {
        self.db.execute(
            "UPDATE nodes SET crate_name=?1,start_line=?2,end_line=?3,visibility=?4,content_hash=?5,metadata=?6 WHERE id=?7",
            params![symbol.crate_name, symbol.start_line, symbol.end_line, symbol.visibility, symbol.content_hash, json!({"parser":symbol.parser}).to_string(), id],
        )?;
        Ok(())
    }

    /// Records syntax reference edges originating in `files` (every Rust file
    /// when `None`), resolving targets against all `nodes`.
    fn index_static_references(
        &self,
        nodes: &[Node],
        files: Option<&BTreeSet<String>>,
        revision: &str,
    ) -> Result<()> {
        let mut targets_by_name: HashMap<&str, Vec<&Node>> = HashMap::new();
        let mut nodes_by_file: HashMap<&str, Vec<&Node>> = HashMap::new();
        for node in nodes {
            targets_by_name
                .entry(short_name(&node.canonical_name))
                .or_default()
                .push(node);
            nodes_by_file.entry(&node.file).or_default().push(node);
        }
        if files.is_none() {
            self.db.execute("DELETE FROM unresolved_references", [])?;
        }
        for file in self.selected_rust_files(files) {
            let Some(text) = read_source_text(&file) else {
                continue;
            };
            let rel = relative(&self.root, &file);
            let file_nodes = nodes_by_file.get(rel.as_str()).cloned().unwrap_or_default();
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
                    .min_by_key(|node| innermost_key(node));
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

    /// Records containment, implementation, and import edges derived from
    /// `files` (every Rust file when `None`).
    fn index_structural_edges(
        &self,
        nodes: &[Node],
        files: Option<&BTreeSet<String>>,
        revision: &str,
    ) -> Result<()> {
        let mut nodes_by_file: HashMap<&str, Vec<&Node>> = HashMap::new();
        for node in nodes {
            nodes_by_file.entry(&node.file).or_default().push(node);
        }
        for child in nodes
            .iter()
            .filter(|node| files.is_none_or(|files| files.contains(&node.file)))
        {
            if let Some(parent) = nodes_by_file[child.file.as_str()]
                .iter()
                .copied()
                .filter(|parent| {
                    parent.id != child.id
                        && parent.start_line <= child.start_line
                        && parent.end_line >= child.end_line
                })
                .min_by_key(|parent| innermost_key(parent))
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
        for file in self.selected_rust_files(files) {
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
                let source = first_by_location(nodes.iter().filter(|node| {
                    short_name(&node.canonical_name) == type_name
                        && matches!(node.kind.as_str(), "struct" | "enum" | "type")
                }));
                let target = first_by_location(nodes.iter().filter(|node| {
                    short_name(&node.canonical_name) == trait_name && node.kind == "trait"
                }));
                if let (Some(source), Some(target)) = (source, target) {
                    self.add_implements_contributor(source.id, target.id, &rel, revision)?;
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
                let target = first_by_location(
                    nodes
                        .iter()
                        .filter(|node| short_name(&node.canonical_name) == imported),
                );
                let source = nodes_by_file
                    .get(rel.as_str())
                    .and_then(|file_nodes| first_by_location(file_nodes.iter().copied()));
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

    /// Absolute paths of the selected Rust files, or of every workspace Rust
    /// file when `files` is `None`.
    fn selected_rust_files(&self, files: Option<&BTreeSet<String>>) -> Vec<PathBuf> {
        match files {
            Some(files) => files
                .iter()
                .filter(|file| file.ends_with(".rs"))
                .map(|file| self.root.join(file))
                .filter(|path| path.is_file())
                .collect(),
            None => rust_files(&self.root),
        }
    }

    /// Records `file` as one producer of an IMPLEMENTS edge.
    ///
    /// Impl headers resolve their type and trait by short name, so several
    /// files can yield the same edge. The edge keeps every contributing file
    /// (sorted; `file` names the first) and an incremental refresh removes it
    /// only once no contributor yields it any more. Recording only the first
    /// file used to drop a still-justified edge when that file was re-indexed.
    fn add_implements_contributor(
        &self,
        source: i64,
        target: i64,
        file: &str,
        revision: &str,
    ) -> Result<()> {
        let existing: Option<(i64, String)> = self
            .db
            .query_row(
                "SELECT id,metadata FROM edges WHERE src=?1 AND dst=?2 AND kind='IMPLEMENTS' AND provenance='Syntax'",
                params![source, target],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        match existing {
            Some((id, metadata)) => {
                let mut files = edge_contributors(&metadata);
                files.insert(file.to_owned());
                self.db.execute(
                    "UPDATE edges SET revision=?1,metadata=?2 WHERE id=?3",
                    params![revision, contributors_metadata(&files).to_string(), id],
                )?;
                Ok(())
            }
            None => self.insert_edge(EdgeRecord {
                source,
                target,
                kind: "IMPLEMENTS",
                confidence: 0.85,
                provenance: "Syntax",
                revision,
                metadata: contributors_metadata(&BTreeSet::from([file.to_owned()])),
            }),
        }
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
    /// `candidates` are scanned; replacement names resolve against all `nodes`.
    /// Callers remove the candidates' previous evidence first.
    fn index_lifecycle_evidence(
        &self,
        candidates: &[Node],
        nodes: &[Node],
        revision: &str,
    ) -> Result<()> {
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
        let mut sources = SourceCache::default();
        for node in candidates {
            let Some(lines) = sources.lines(&self.root, &node.file) else {
                continue;
            };
            let start = node.start_line.saturating_sub(5).min(lines.len());
            let excerpt = lines[start..(start + 28).min(lines.len())].join("\n");
            let replacement = explicit
                .captures(&excerpt)
                .and_then(|capture| capture.get(1))
                .map(|capture| capture.as_str())
                .and_then(|name| {
                    first_by_location(
                        nodes
                            .iter()
                            .filter(|other| short_name(&other.canonical_name) == name),
                    )
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

    /// Scans TODO/FIXME markers in `paths` (every text input when `None`).
    /// Items of unscanned files keep their evidence and are re-validated
    /// against `revision`.
    fn index_proposed_work(&self, paths: Option<&BTreeSet<String>>, revision: &str) -> Result<()> {
        let marker = Regex::new(r"(?i)\b(TODO|FIXME|XXX)\b\s*[:\-]?\s*(.+)")?;
        let indexed_at = Utc::now().to_rfc3339();
        let absent = json!([
            "Source marker absent at the current snapshot; review or remove this proposed item."
        ])
        .to_string();
        // Preserve the item for architectural history, but make disappearing
        // automatic evidence visibly stale instead of silently trusting it.
        let files = match paths {
            None => {
                self.db.execute(
                    "UPDATE work_items SET evidence_json=?1, confidence=0.20, last_validated_snapshot=?2, updated_at=?3 \
                     WHERE provenance='SourceDoc' AND discovered_from='TODO/FIXME scanner' AND status='proposed'",
                    params![absent, revision, indexed_at],
                )?;
                all_text_files(&self.root)
            }
            Some(paths) => {
                self.db.execute(
                    "UPDATE work_items SET last_validated_snapshot=?1 \
                     WHERE provenance='SourceDoc' AND discovered_from='TODO/FIXME scanner' AND status='proposed'",
                    [revision],
                )?;
                for path in paths {
                    self.db.execute(
                        "UPDATE work_items SET evidence_json=?1, confidence=0.20, updated_at=?2 \
                         WHERE provenance='SourceDoc' AND discovered_from='TODO/FIXME scanner' AND status='proposed' AND scope_json=?3",
                        params![absent, indexed_at, json!([path]).to_string()],
                    )?;
                }
                paths
                    .iter()
                    .map(|path| self.root.join(path))
                    .filter(|path| path.is_file() && input_kind(path).is_some())
                    .collect()
            }
        };
        for path in files {
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

    fn index_git(&self, revision: &str, head: Option<&str>) -> Result<()> {
        self.db.execute(
            "INSERT OR REPLACE INTO metadata(key, value) VALUES ('git_history_head', ?1)",
            [head.unwrap_or_default()],
        )?;
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

    /// Stores non-source text inputs among `paths` and returns their row ids.
    fn index_documents(&self, paths: &[PathBuf], revision: &str) -> Result<Vec<i64>> {
        let mut inserted = Vec::new();
        for path in paths {
            let Some(input_kind) = input_kind(path) else {
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
            let Some(text) = read_source_text(path) else {
                continue;
            };
            // Documents are stored whole; cap them so one checked-in dump
            // cannot put an unbounded blob into the index.
            let text = trim_text(&text, MAX_DOCUMENT_BYTES);
            self.db.execute(
                "INSERT INTO documents(kind, path, text, revision) VALUES (?1, ?2, ?3, ?4)",
                params![kind, relative(&self.root, path), text, revision],
            )?;
            inserted.push(self.db.last_insert_rowid());
        }
        Ok(inserted)
    }

    /// Infers use cases from the test symbols among `nodes`.
    fn index_use_cases(&self, nodes: &[Node], revision: &str) -> Result<()> {
        for node in nodes.iter().filter(|node| {
            node.file.contains("test") || short_name(&node.canonical_name).starts_with("test_")
        }) {
            let name = short_name(&node.canonical_name).replace('_', " ");
            let id = format!("usecase:{}", slug(&format!("{}-{}", node.file, name)));
            // An upsert, not a replace: replacing the row cascaded away the
            // links of every other test that shares the use case.
            self.db.execute(
                "INSERT INTO use_cases(id, name, description, provenance, confidence, revision) VALUES (?1, ?2, ?3, 'AgentInference', 0.60, ?4) \
                 ON CONFLICT(id) DO UPDATE SET name=excluded.name,description=excluded.description,confidence=excluded.confidence,revision=excluded.revision",
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

    /// Build a bounded, live-worktree architecture map and advisory report.
    ///
    /// Unlike the published relationship index, these facts are collected
    /// directly from current manifests and Rust syntax. The returned profile
    /// and limitations make that evidence envelope explicit.
    pub fn architecture(&self, scope: Option<&str>, max_findings: usize) -> Result<Value> {
        Ok(serde_json::to_value(
            self.analyze_architecture(scope, max_findings)?,
        )?)
    }

    fn analyze_architecture(
        &self,
        scope: Option<&str>,
        max_findings: usize,
    ) -> Result<analysis::ArchitectureReport> {
        let revision = self.revision();
        let report = analysis::analyze(
            &self.root,
            serde_json::to_value(&revision)?,
            scope,
            max_findings,
        )?;
        ensure!(
            revision == self.revision(),
            "the live worktree changed during architecture analysis; retry so facts are not labelled with a mixed revision"
        );
        Ok(report)
    }

    /// Run and persist a snapshot-scoped architecture audit.
    pub fn architecture_audit(&self, scope: Option<&str>, max_findings: usize) -> Result<Value> {
        let report = self.analyze_architecture(scope, max_findings)?;
        let payload = serde_json::to_string(&report)?;
        self.db.execute(
            "INSERT OR REPLACE INTO architecture_reports(id,revision,profile,scope,analyzer_version,payload,created_at) VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![
                report.id,
                report.revision.to_string(),
                report.profile,
                report.scope,
                report.analyzer_version,
                payload,
                report.generated_at
            ],
        )?;
        self.db.execute(
            "DELETE FROM architecture_reports WHERE id NOT IN (SELECT id FROM architecture_reports ORDER BY created_at DESC LIMIT 20)",
            [],
        )?;
        Ok(serde_json::to_value(report)?)
    }

    pub fn architecture_report(&self, id: &str) -> Result<Value> {
        let payload: String = self
            .db
            .query_row(
                "SELECT payload FROM architecture_reports WHERE id=?1",
                [id],
                |row| row.get(0),
            )
            .optional()?
            .with_context(|| format!("unknown architecture audit `{id}`"))?;
        Ok(serde_json::from_str(&payload)?)
    }

    pub fn architecture_reports(&self, limit: usize) -> Result<Value> {
        let mut statement = self.db.prepare(
            "SELECT payload FROM architecture_reports ORDER BY created_at DESC LIMIT ?1",
        )?;
        let reports = statement
            .query_map([limit.clamp(1, 100) as i64], |row| row.get::<_, String>(0))?
            .filter_map(Result::ok)
            .filter_map(|payload| serde_json::from_str::<Value>(&payload).ok())
            .map(|report| {
                json!({
                    "id":report.get("id"),
                    "revision":report.get("revision"),
                    "profile":report.get("profile"),
                    "scope":report.get("scope"),
                    "generated_at":report.get("generated_at"),
                    "summary":report.get("summary")
                })
            })
            .collect::<Vec<_>>();
        Ok(json!({"reports":reports,"count":reports.len()}))
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
        let target_text = targets.join(" ");
        let decisions = self.governing_decisions(&target_text, 12)?;
        let steerings = self.governing_steerings(&target_text, 12)?;
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
        let architecture_baseline = self.analyze_architecture(None, 1_000)?;
        let architecture_guard = json!({
            "baseline_id": architecture_baseline.id,
            "analyzer_version": architecture_baseline.analyzer_version,
            "profile": architecture_baseline.profile,
            "summary": architecture_baseline.summary,
            "baseline_findings": analysis::baseline(&architecture_baseline.findings),
            "policy": "advisory_only",
            "note": "Validation reports only new, worsened, and resolved findings. Existing inferred debt is never promoted or made blocking automatically."
        });
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
        let payload = json!({"context_id":context_id,"intent":intent,"revision":self.revision(),"snapshot":self.snapshot(),"automatic_problem_capture":automatic_problem_capture,"primary_symbols":target_nodes,"references":references,"reference_provenance":{"semantic":"RustAnalyzer (confidence 1.0), cached by semantic snapshot","fallback":"Syntax-derived, ambiguity-suppressed relationships (confidence labelled)"},"semantic_references":semantic_references,"uncertain_static_references":uncertain_static_references,"runtime_contracts":runtime_contracts,"tests":tests,"use_cases":use_cases,"decisions":decisions,"steerings":steerings,"lifecycle":lifecycle,"obsolete_candidates":obsolete.get("candidates"),"work_items":work.get("items"),"likely_change_surface":likely_surface,"source_slices":slices,"context_budget":{"tokens":token_budget,"estimated_tokens":used.div_ceil(4)},"generation":self.active_generation(),"semantic_snapshot":self.semantic_snapshot_resource(),"risk":risk(&target_nodes, &references),"validation_queue":validation_queue,"architecture_guard":architecture_guard,"approved_models":domain::approved_for_root(&self.root,&self.execution)?,"live_instructions":self.live_instructions()?,"engineering_route":guidance::guidance_hint(intent),"unresolved_edges":["Ambiguous static references are reported separately and excluded from likely_change_surface.","Runtime registration, generated code, inactive feature/target profiles, external consumers, and deployment state require profile-specific or runtime verification."],"verification_plan":["Read current project.contract and engineering.guidance for this change.","Use verification.plan for the supported Cargo profiles, targets, MSRV and lint policy; default tests include doctests.","Run relevant changed-artifact and specialized correctness/performance checks.","Results are revision/profile bound; incomplete or stale evidence cannot authorize delivery."],"semantic_note":"rust-analyzer facts are resolved on demand and persisted for the active semantic snapshot. Syntax relationships are AST-derived; unresolved ambiguous names are retained as uncertainty, not impact edges."});
        self.db.execute("INSERT INTO change_contexts(id, intent, payload, revision, created_at) VALUES (?1, ?2, ?3, ?4, ?5)", params![context_id, intent, payload.to_string(), self.revision().workspace_digest, Utc::now().to_rfc3339()])?;
        Ok(response_budget::bound(payload, token_budget))
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
                        semantic.iter().map(semantic_location).collect::<Vec<_>>(),
                        "rust-analyzer",
                    )
                }
            }
            "callers" | "references" => {
                let semantic = self.semantic_locations(&nodes, "references");
                if relation == "references" && !semantic.is_empty() {
                    return Ok(
                        json!({"symbol":symbol,"relation":relation,"definitions":definitions,"results":semantic.iter().take(limit).map(semantic_location).collect::<Vec<_>>(),"channel":"rust-analyzer","freshness":self.snapshot()}),
                    );
                }
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
        let result = self.with_memory_write(|| self.record_decision_inner(&input))?;
        Ok(self.record_decision_materialization(&input, result))
    }

    fn record_decision_inner(&self, input: &RecordDecision) -> Result<Value> {
        quality::validate_choice("decision status", &input.status, DECISION_STATUSES)?;
        let mut supersedes: Vec<String> = Vec::new();
        for id in input.supersedes.iter().map(|id| id.trim()) {
            if !id.is_empty() && !supersedes.iter().any(|seen| seen == id) {
                supersedes.push(id.to_owned());
            }
        }
        if !supersedes.is_empty() {
            ensure!(
                input.status == "accepted",
                "only an accepted decision can supersede others; status was `{}`",
                input.status
            );
            ensure!(
                !input.recorded_by.trim().is_empty(),
                "recorded_by is required when superseding decisions"
            );
            for superseded in &supersedes {
                self.ensure_decision_accepted(superseded, "supersede")?;
            }
        }
        let next: i64 = self.db.query_row(
            "SELECT COALESCE(MAX(sequence), 0) + 1 FROM decisions",
            [],
            |r| r.get(0),
        )?;
        let id = format!("DEC-{:04}", next);
        let now = Utc::now().to_rfc3339();
        self.db.execute("INSERT INTO decisions(id, sequence, status, title, rationale, applies_to, consequences, supersedes, revision, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)", params![id, next, input.status, input.title, input.reason, serde_json::to_string(&input.applies_to)?, serde_json::to_string(&input.consequences)?, serde_json::to_string(&supersedes)?, self.revision().workspace_digest, now])?;
        for superseded in &supersedes {
            self.close_decision(
                superseded,
                "superseded",
                input.recorded_by.trim(),
                &format!("superseded by {id}"),
                &now,
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
        self.refresh_memory_search_rows()?;
        Ok(
            json!({"id":id,"status":input.status,"supersedes":supersedes,"resource_uri":format!("rustrepo://decision/{id}"),"materialized_path":null,"committed":true,"revision":self.revision()}),
        )
    }

    /// Retires an accepted decision without replacing it. The record stays in
    /// the ledger as history; only a new decision can take its place.
    pub fn retire_decision(&self, input: RetireDecision) -> Result<Value> {
        let result = self.with_memory_write(|| self.retire_decision_inner(&input))?;
        Ok(self.retire_decision_materialization(&input, result))
    }

    fn retire_decision_inner(&self, input: &RetireDecision) -> Result<Value> {
        let id = input.id.trim();
        ensure!(!id.is_empty(), "id is required");
        let retired_by = input.retired_by.trim();
        ensure!(!retired_by.is_empty(), "retired_by is required");
        self.ensure_decision_accepted(id, "retire")?;
        let note = input.note.trim();
        self.close_decision(id, "retired", retired_by, note, &Utc::now().to_rfc3339())?;
        let mut decision = self.decision_resource(id)?;
        decision["materialized_path"] = Value::Null;
        Ok(decision)
    }

    /// Fails unless `id` names an accepted decision. Superseded and retired
    /// decisions are terminal history and cannot be closed a second time.
    fn ensure_decision_accepted(&self, id: &str, action: &str) -> Result<()> {
        let status: Option<String> = self
            .db
            .query_row("SELECT status FROM decisions WHERE id=?1", [id], |row| {
                row.get(0)
            })
            .optional()?;
        match status.as_deref() {
            None => bail!("cannot {action} unknown decision `{id}`"),
            Some("accepted") => Ok(()),
            Some(status) => bail!(
                "cannot {action} decision `{id}` with status `{status}`; only accepted decisions can be {action}d"
            ),
        }
    }

    /// Moves an accepted decision to a terminal status, appends its review
    /// history row. Optional Markdown is exported after the database commits.
    fn close_decision(
        &self,
        id: &str,
        status: &str,
        actor: &str,
        note: &str,
        now: &str,
    ) -> Result<()> {
        self.db.execute(
            "UPDATE decisions SET status=?1 WHERE id=?2",
            params![status, id],
        )?;
        self.db.execute(
            "INSERT INTO decision_reviews(decision_id,action,actor,note,created_at) VALUES (?1,?2,?3,?4,?5)",
            params![id, status, actor, note, now],
        )?;
        Ok(())
    }

    /// Rewrites the `Status:` line of a decision materialized under
    /// `docs/decisions`. The file name starts with the decision sequence, so the
    /// path never needs to be stored; a decision that was never materialized
    /// yields `None`.
    fn update_materialized_decision(
        &self,
        sequence: i64,
        status_line: &str,
    ) -> Result<Option<String>> {
        let prefix = format!("{sequence:04}-");
        let Ok(entries) = fs::read_dir(self.root.join("docs/decisions")) else {
            return Ok(None);
        };
        let Some(path) = entries
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.path())
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(&prefix) && name.ends_with(".md"))
            })
        else {
            return Ok(None);
        };
        let text = fs::read_to_string(&path)?;
        let mut replaced = false;
        let lines = text
            .lines()
            .map(|line| {
                if !replaced && line.starts_with("Status: ") {
                    replaced = true;
                    format!("Status: {status_line}")
                } else {
                    line.to_owned()
                }
            })
            .collect::<Vec<_>>();
        if !replaced {
            return Ok(None);
        }
        fs::write(&path, format!("{}\n", lines.join("\n")))?;
        Ok(Some(relative(&self.root, &path)))
    }

    pub fn validate_change(
        &self,
        context_id: &str,
        diff: Option<&str>,
        run_checks: bool,
    ) -> Result<Value> {
        let source = diff
            .map(|text| DiffSource::Inline(text.to_owned()))
            .unwrap_or(DiffSource::Pending);
        self.validate_change_from_source(Some(context_id), &source, run_checks)
    }

    /// Validate a locally resolved patch or Git comparison. Checks and static
    /// analysis still run against the current worktree, never a checkout of HEAD.
    ///
    /// A prepared context may be validated any number of times, e.g. at each
    /// milestone of a long refactor. Without one, an unprepared context is
    /// recorded so diff-driven obligations can still be tracked and recorded;
    /// only the before/after comparisons that need prepared evidence (the
    /// architecture delta and the expected change surface) are unavailable.
    pub fn validate_change_from_source(
        &self,
        context_id: Option<&str>,
        source: &DiffSource,
        run_checks: bool,
    ) -> Result<Value> {
        let source_before = self.revision();
        let index_before = index_freshness(&self.root, &self.db)?;
        let resolved = source.resolve(&self.root)?;
        let diff = resolved.text;
        let changed_files = changed_files_in_diff(&diff);
        let (context_id, context) = match context_id {
            Some(context_id) => {
                let payload: String = self
                    .db
                    .query_row(
                        "SELECT payload FROM change_contexts WHERE id=?1",
                        [context_id],
                        |r| r.get(0),
                    )
                    .optional()?
                    .context("unknown change context")?;
                (context_id.to_owned(), serde_json::from_str(&payload)?)
            }
            None => self.record_unprepared_context(&changed_files)?,
        };
        let context_id = context_id.as_str();
        let prepared = context["prepared"] != false;
        let architecture_baseline_truncated = context
            .pointer("/architecture_guard/summary/findings_truncated")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let architecture_delta = if !prepared {
            json!({
                "policy":"advisory_only",
                "available":false,
                "status":"unavailable",
                "reason":"no prepared context",
                "note":"Run change.prepare before the change to capture an architecture baseline; one prepared context serves every later validation."
            })
        } else if architecture_baseline_truncated {
            json!({
                "policy":"advisory_only",
                "available":false,
                "reason":"The prepared architecture baseline was truncated, so Crusty will not guess whether a finding is new. Narrow the audit scope for review."
            })
        } else if let Some(baseline) = context
            .pointer("/architecture_guard/baseline_findings")
            .and_then(Value::as_array)
        {
            let baseline = baseline
                .iter()
                .cloned()
                .map(serde_json::from_value)
                .collect::<serde_json::Result<Vec<analysis::ArchitectureFindingBaseline>>>(
            )?;
            let current = self.analyze_architecture(None, 1_000)?;
            if current
                .summary
                .get("findings_truncated")
                .and_then(Value::as_bool)
                .unwrap_or(true)
            {
                json!({
                    "policy":"advisory_only",
                    "available":false,
                    "reason":"The current architecture scan was truncated, so Crusty will not guess whether a finding is new. Narrow the audit scope for review."
                })
            } else {
                analysis::delta(&baseline, &current.findings, &changed_files)
            }
        } else {
            json!({
                "policy":"advisory_only",
                "available":false,
                "reason":"This context predates architecture baselines; run change.prepare again to enable a before/after architecture delta."
            })
        };
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
        let unmodified = if prepared {
            json!(expected.difference(&changed_files).collect::<Vec<_>>())
        } else {
            json!({"status":"unavailable","reason":"no prepared context"})
        };
        self.execution.check()?;
        let verification = if run_checks {
            let coordinator = coordination::Coordinator::open(&self.root, self.execution.clone())?;
            let plan = coordinator.verification_plan(verification::VerificationPlanRequest {
                profile: verification::BuildProfile::default(),
                checks: vec![
                    verification::CheckKind::Format,
                    verification::CheckKind::Check,
                    verification::CheckKind::Test,
                    verification::CheckKind::Clippy,
                ],
                offline: false,
                test_filter: None,
                deny_warnings: None,
            })?;
            Some(
                coordinator.verification_run(
                    plan["id"].as_str().context("verification plan has no ID")?,
                )?,
            )
        } else {
            None
        };
        let mut checks = verification
            .as_ref()
            .and_then(|report| report["checks"].as_array())
            .into_iter()
            .flatten()
            .map(|check| {
                let mut check = check.clone();
                check["success"] = check["passed"].clone();
                check["command"] = json!(format!(
                    "cargo {}",
                    check["args"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(" ")
                ));
                check
            })
            .collect::<Vec<_>>();
        if let Some(verification) = &verification
            && verification["source_unchanged"] != true
        {
            checks.push(json!({"command":"revision/environment binding","success":false,"reason":"Source, toolchain or environment changed while checks ran; results do not validate current code."}));
        }
        let artifact_checks = if run_checks {
            changed_files
                .iter()
                .filter_map(|path| artifact_validator(Path::new(path)))
                .map(|(program, arguments)| {
                    run_optional_command_check(&self.root, program, &arguments, &self.execution)
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
        self.execution.check()?;
        let semantic_diagnostics = self.validation_diagnostics(&changed_files)?;
        let blocking = self.blocking_obligation_status(context_id, run_checks)?;
        let validation_status = validation_verdict(
            run_checks,
            &checks,
            &artifact_checks,
            &learned_checks,
            &blocking,
        );
        let source_after = self.revision();
        let index_after = index_freshness(&self.root, &self.db)?;
        let source_unchanged = source_before == source_after;
        let generation_unchanged = index_before.generation == index_after.generation;
        let skipped_inputs = resolved
            .scope
            .pointer("/untracked/skipped")
            .and_then(Value::as_array)
            .is_some_and(|items| !items.is_empty());
        let review_evidence = json!({
            "status": if !source_unchanged || !generation_unchanged { "changed_during_review" }
                else if index_before.stale() || skipped_inputs { "incomplete" } else { "current" },
            "source_unchanged": source_unchanged,
            "source_before": source_before, "source_after": source_after,
            "index": {"generation": index_before.generation, "generation_after": index_after.generation,
                "stable_during_review": generation_unchanged, "stale": index_before.stale(),
                "reason": index_before.reason(), "workspace_digest": index_before.workspace_digest},
            "diff_complete": !skipped_inputs,
            "architecture": {"authority": "advisory", "backend": "live_worktree"},
            "semantic_correctness": "not_established",
            "delivery_authorized": false,
            "next": "Use live source and revision-bound verification.plan/run for supported profiles; execute applicable runtime tests and benchmarks. A completed review is not proof of semantic correctness.",
        });
        Ok(
            json!({"context_id":context_id,"prepared":prepared,"diff_scope":resolved.scope,"analysis_target":"current_worktree","revision_before":context.get("revision"),"revision_after":self.revision(),"snapshot":self.snapshot(),"changed_files":changed_files,"expected_affected_files":if prepared { json!(expected) } else { json!({"status":"unavailable","reason":"no prepared context"}) },"unmodified_expected_callers":unmodified,"new_references":"Re-run change.prepare after signature changes to refresh static candidates.","architectural_violations":self.decision_conflicts(&diff)?,"architecture_delta":architecture_delta,"legacy_paths_touched":legacy_left,"unresolved_edges":context.get("unresolved_edges"),"recommended_tests":context.get("tests"),"recommended_commands":["Read project.contract and use verification.plan for supported profiles and current lint policy.","Run applicable artifact and human-approved specialized checks; report every unrun profile."],"validation_queue":validation_queue,"checks":checks,"artifact_checks":artifact_checks,"learned_checks":learned_checks,"validation_status":validation_status,"review_evidence":review_evidence,"verification":verification,"semantic_diagnostics":semantic_diagnostics,"blocking":blocking,"uncertainty":"Artifact validators and architecture detectors can identify current static evidence, but not runtime registration, generated-code drift, external-consumer compatibility, or deployment behavior."}),
        )
    }

    /// Persists a context for a validation that has no prepared one, so the
    /// obligations activated from its diff have an owner that
    /// `validation.record` and later validations can address. It carries no
    /// architecture baseline or expected change surface.
    fn record_unprepared_context(
        &self,
        changed_files: &BTreeSet<String>,
    ) -> Result<(String, Value)> {
        let context_id = format!(
            "ctx_{}",
            &blake3::hash(format!("unprepared:{changed_files:?}:{}", Utc::now()).as_bytes())
                .to_hex()[..12]
        );
        let nodes = self
            .all_nodes()?
            .into_iter()
            .filter(|node| changed_files.contains(&node.file))
            .collect::<Vec<_>>();
        let intent = "validation without a prepared context";
        let payload = json!({"context_id":context_id,"intent":intent,"prepared":false,"revision":self.revision(),"snapshot":self.snapshot(),"changed_files":changed_files,"lifecycle":self.lifecycle_for(&nodes)?});
        self.db.execute("INSERT INTO change_contexts(id, intent, payload, revision, created_at) VALUES (?1, ?2, ?3, ?4, ?5)", params![context_id, intent, payload.to_string(), self.revision().workspace_digest, Utc::now().to_rfc3339()])?;
        Ok((context_id, payload))
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
            json!({"change":change,"snapshot":self.snapshot(),"decisions":self.governing_decisions(change,20)?,"steerings":self.governing_steerings(change,20)?,"learned_quality_constraints":self.relevant_quality_constraints(change)?,"lifecycle_risks":self.obsolete_candidates(Some(change), 20)?,"runtime_contracts":self.search_contract_artifacts(change,12)?,"unresolved_edges":["Ambiguous syntax references are preserved as uncertainty rather than promoted to affected files.","Dynamic dispatch, macros, runtime registration, generated bindings, external API consumers, and deployment state need external verification."],"required_verification":["cargo check --all-targets","cargo test","Run the bounded feature-profile plan from repo.matrix.","Validate changed GTK markup with project GTK tooling.","Compare D-Bus XML changes with runtime introspection and external consumer expectations.","Run a deployment smoke test in the target environment when packaging or service configuration changes."]}),
        )
    }

    /// A bounded, read-only briefing that gives an attached agent repository
    /// guidance before it plans, answers, or acts on any repository topic.
    pub fn consult(&self, topic: &str, budget: usize) -> Result<Value> {
        self.consult_within(topic, budget, None, Vec::new())
    }

    /// `consult` budgeted as one response: with `freshness`, the briefing is
    /// returned as `{freshness, result}` and the envelope counts against the
    /// same budget as the sections.
    pub(crate) fn consult_within(
        &self,
        topic: &str,
        budget: usize,
        freshness: Option<Value>,
        approved_models: Vec<Value>,
    ) -> Result<Value> {
        ensure!(!topic.trim().is_empty(), "topic is required");
        let token_budget = budget.clamp(250, 20_000);
        let instructions = self.consult_instructions(topic)?;
        let live_paths = instructions
            .iter()
            .filter_map(|(full, _)| full["path"].as_str().map(str::to_owned))
            .collect::<BTreeSet<_>>();
        let decisions = self.governing_decisions(topic, 20)?;
        let steerings = self.governing_steerings(topic, 20)?;
        // Agents must not retire human guidance on their own initiative, so a
        // stale or apparently replaced record only produces a hint to ask.
        let cleanup = decisions
            .iter()
            .chain(&steerings)
            .filter(|record| steering::needs_cleanup(record))
            .filter_map(|record| record["id"].as_str().map(str::to_owned))
            .collect::<Vec<_>>();
        let sections = vec![
            Section::new(
                "decisions",
                decisions,
                |decision| {
                    let mut compact = json!({"id":decision["id"],"title":decision["title"],"status":decision["status"],
                        "applies_to":first_values(&decision["applies_to"], 4)});
                    if steering::needs_cleanup(decision) {
                        compact["stale_references"] =
                            first_values(&decision["stale_references"], 3);
                    }
                    compact
                },
                "decision.list with the topic, or repo.constraints, returns full accepted decisions.",
            ),
            Section::with_forms(
                "steerings",
                steerings
                    .into_iter()
                    .map(steering::consultation_forms)
                    .collect(),
                "steering.list with the topic or scope returns the full steering and its history.",
            ),
            Section::new(
                "approved_models",
                approved_models,
                |model| json!({"id":model["id"],"title":model["title"],"status":model["status"]}),
                "Approved domain models are listed by id; read the full contract before changing a concept owner.",
            ),
            Section::new(
                "learned_quality_constraints",
                self.relevant_quality_constraints(topic)?,
                |item| {
                    let constraint = &item["constraint"];
                    json!({"constraint":{"id":constraint["id"],"rule":trim_text(constraint["rule"].as_str().unwrap_or(""), 200),
                        "enforcement":constraint["enforcement"],"status":constraint["status"]},
                        "match":{"matched":item["match"]["matched"]}})
                },
                "quality.get with an id, or repo.constraints, returns the full constraint and match.",
            ),
            Section::with_forms(
                "live_instructions",
                instructions,
                "Read each listed instruction file directly before acting in its scope.",
            ),
            Section::new(
                "governing_documents",
                self.governing_documents(topic, 12, &live_paths)?,
                |document| json!({"path":document["path"],"relevance":document["relevance"]}),
                "Read the listed paths directly, or call repo.search for the topic.",
            ),
            Section::new(
                "known_work",
                self.work_matches(Some(topic), 12, true)?["items"]
                    .as_array()
                    .map(|items| items.iter().map(relevance::work_brief).collect())
                    .unwrap_or_default(),
                |work| json!({"id":work["id"],"title":work["title"],"status":work["status"]}),
                "work.get with an id returns evidence, acceptance criteria, and verification; work.list lists more.",
            ),
            Section::new(
                "lifecycle_risks",
                self.obsolete_candidates(Some(topic), 12)?["candidates"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default(),
                |risk| {
                    json!({"classification":risk["classification"],"symbol":risk["symbol"],
                        "file":risk["file"],"replacement":risk["replacement"]})
                },
                "repo.obsolete_candidates with the topic returns the full removal slices.",
            ),
            Section::new(
                "runtime_contracts",
                self.relevant_contract_artifacts(topic, 8)?,
                |contract| json!({"kind":contract["kind"],"path":contract["path"]}),
                "Read the listed contract paths directly, or call repo.search for the topic.",
            ),
        ];
        let relevant_sections = sections
            .iter()
            .filter(|section| !section.is_empty())
            .map(Section::name)
            .collect::<Vec<_>>();
        // The full snapshot repeats index internals that `index.status`
        // reports; consultation carries only what dates its guidance.
        let snapshot = self.snapshot();
        let snapshot = json!({
            "branch": snapshot["branch"],
            "generation": snapshot["generation"],
            "revision": snapshot["revision"],
            "indexing_timestamp": snapshot["indexing_timestamp"],
        });
        let mut next_steps = vec![
            "Apply relevant human guidance before continuing.".to_owned(),
            "If the request will modify repository files, call change.prepare before the first edit and change.validate after the edits.".to_owned(),
            "Use repo.context when the consultation identifies a concept that needs deeper repository evidence.".to_owned(),
        ];
        next_steps.extend(self.semantic_hint());
        if !cleanup.is_empty() {
            next_steps.push(format!(
                "{} name missing paths (stale_references) or appear replaced by a newer steering (possibly_superseded_by). Tell the user and ask a human to retire or supersede them (steering.retire, steering.record with supersedes, decision.retire); do not retire them yourself.",
                cleanup.join(", ")
            ));
        }
        let body = json!({
            "topic": topic,
            "consulted": true,
            "guidance_found": !relevant_sections.is_empty(),
            "relevant_sections": relevant_sections,
            "snapshot": snapshot,
            "engineering_route": guidance::guidance_hint(topic),
            "next_steps": next_steps,
            "authority": "Human decisions and steering govern. Indexed documents and relationships are freshness-labelled guidance; current source and runtime/compiler behavior remain authoritative.",
        });
        let value = match freshness {
            Some(freshness) => json!({"freshness": freshness, "result": body}),
            None => body,
        };
        let packed = response_budget::pack(
            value,
            sections,
            &[
                ("next_steps", None),
                ("authority", None),
                ("snapshot", None),
            ],
            token_budget,
        );
        // Only an envelope larger than the whole budget reaches this net.
        Ok(response_budget::bound(packed, token_budget))
    }

    /// Live instruction files as `(consultation form, stub)`. Root and
    /// topic-path-ancestor files are inlined when they fit; others are always
    /// stubs, so every instruction file stays discoverable within the budget.
    fn consult_instructions(&self, topic: &str) -> Result<Vec<(Value, Value)>> {
        let paths = relevance::topic_paths(topic);
        Ok(self
            .live_instructions()?
            .into_iter()
            .map(|mut document| {
                let Some(path) = document["path"].as_str().map(str::to_owned) else {
                    return (document.clone(), document);
                };
                let scope = document["scope"].as_str().unwrap_or("").to_owned();
                let stub = json!({
                    "path": path,
                    "scope": scope,
                    "bytes": document["bytes"],
                    "content_digest": document["content_digest"],
                    "inlined": false,
                    "note": "Read this file before acting in its scope.",
                });
                let governs = scope.is_empty()
                    || paths
                        .iter()
                        .any(|topic_path| relevance::path_contains(&scope, topic_path));
                if governs {
                    document["inlined"] = json!(true);
                    (document, stub)
                } else {
                    (stub.clone(), stub)
                }
            })
            .collect())
    }

    /// Accepted decisions that are about `topic`, then every global one.
    /// Unlike `decisions_for`, a decision sharing one incidental word with a
    /// long intent does not govern it.
    fn governing_decisions(&self, topic: &str, limit: usize) -> Result<Vec<Value>> {
        let mut decisions = Vec::new();
        for hit in self.relevant_hits(&terms(topic), Some("decision"), MAX_DECISION_HITS)? {
            if let Some(id) = hit["entity_id"].as_str() {
                let decision = self.decision_resource(id)?;
                if decision["status"] == "accepted" {
                    decisions.push(decision);
                }
            }
        }
        let global_ids = self
            .db
            .prepare("SELECT id FROM decisions WHERE status='accepted' AND json_array_length(applies_to)=0 ORDER BY sequence DESC LIMIT ?1")?
            .query_map([limit as i64], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for id in global_ids {
            decisions.push(self.decision_resource(&id)?);
        }
        let mut seen = BTreeSet::new();
        decisions.retain(|decision| {
            decision["id"]
                .as_str()
                .is_some_and(|id| seen.insert(id.to_owned()))
        });
        decisions.truncate(limit);
        Ok(decisions)
    }

    /// Documents that govern `topic`: topical full-text hits first, then prose
    /// policy documents. Instruction files are reported by `live_instructions`
    /// and never repeated here, and CI/workflow configuration appears only when
    /// the topic is about CI, workflows, or configuration.
    fn governing_documents(
        &self,
        topic: &str,
        limit: usize,
        live_paths: &BTreeSet<String>,
    ) -> Result<Vec<Value>> {
        let concerns_ci = relevance::topic_concerns_ci(topic);
        let admissible = |path: &str| {
            !live_paths.contains(path)
                && !is_instruction_file(path)
                && (concerns_ci || !relevance::is_ci_configuration(path))
        };
        let mut documents = Vec::new();
        for mut hit in
            self.relevant_hits(&terms(topic), Some("document"), limit.saturating_mul(2))?
        {
            let path = hit["path"].as_str().unwrap_or("").to_owned();
            if !admissible(&path) {
                continue;
            }
            hit["relevance"] = json!(if relevance::is_ci_configuration(&path) {
                "ci_configuration_for_topic"
            } else {
                "topic_match"
            });
            documents.push(hit);
        }
        let mut statement = self.db.prepare(
            "SELECT kind,path,text FROM documents \
             WHERE lower(path)='design.md' OR lower(path) LIKE '%/design.md' \
                OR lower(path)='style.md' OR lower(path) LIKE '%/style.md' \
                OR lower(path)='theme.md' OR lower(path) LIKE '%/theme.md' \
                OR lower(path) LIKE 'workflow%' \
                OR lower(path) LIKE '%/workflows/%' \
                OR lower(path) LIKE '%/workflow%' \
                OR lower(path) LIKE 'guideline%' \
                OR lower(path) LIKE '%/guideline%' \
                OR lower(path) LIKE 'policy%' \
                OR lower(path) LIKE '%/policy%' \
             ORDER BY path LIMIT 200",
        )?;
        let policies = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (kind, path, text) in policies {
            let ci = relevance::is_ci_configuration(&path);
            let prose = [".md", ".markdown", ".mdx", ".txt"]
                .iter()
                .any(|extension| path.to_lowercase().ends_with(extension));
            if !admissible(&path) || !(prose || ci && concerns_ci) {
                continue;
            }
            documents.push(json!({
                "kind": kind,
                "path": path,
                "evidence": trim_text(&text, 420),
                "provenance": "PublishedDocument",
                "relevance": if ci { "ci_configuration_for_topic" } else { "repository_policy" },
            }));
        }
        let mut seen = BTreeSet::new();
        documents.retain(|document| {
            document["path"]
                .as_str()
                .is_some_and(|path| seen.insert(path.to_owned()))
        });
        documents.truncate(limit);
        Ok(documents)
    }

    fn live_instructions(&self) -> Result<Vec<Value>> {
        let mut documents = Vec::new();
        let entries = walkdir::WalkDir::new(&self.root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|entry| {
                entry.depth() == 0
                    || !(matches!(
                        entry.file_name().to_str(),
                        Some(".git" | ".rust-repo-intelligence")
                    ) || entry.depth() == 1 && entry.file_name() == "target")
            });
        for (scanned, entry) in entries.enumerate() {
            self.execution.check()?;
            if scanned >= 100_000 {
                documents.push(json!({"complete":false,"provenance":"LiveInstructionScanLimit","next":"The scan reached 100000 entries. Read scoped instructions directly."}));
                break;
            }
            let entry = entry?;
            if !entry.file_type().is_file()
                || !entry.file_name().to_str().is_some_and(is_instruction_file)
            {
                continue;
            }
            let path = entry
                .path()
                .strip_prefix(&self.root)?
                .to_string_lossy()
                .into_owned();
            let size = entry.metadata()?.len();
            let mut bytes = Vec::new();
            File::open(entry.path())?
                .take(64_001)
                .read_to_end(&mut bytes)?;
            let complete = bytes.len() <= 64_000;
            bytes.truncate(64_000);
            let source = String::from_utf8_lossy(&bytes);
            documents.push(json!({"path":path,"scope":entry.path().parent().unwrap_or(&self.root).strip_prefix(&self.root)?.to_string_lossy(),
                "evidence":source,"content_digest":format!("b3:{}",blake3::hash(&bytes).to_hex()),"digest_scope":if complete {"file"} else {"captured_prefix"},"complete":complete,"bytes":size,"provenance":"LiveInstructionFile"}));
            if documents.len() >= 100 {
                documents.push(json!({"complete":false,"provenance":"LiveInstructionScanLimit","next":"Read additional scoped instructions directly; more than 100 documents were encountered."}));
                break;
            }
        }
        documents.sort_by_key(|doc| {
            (
                doc["scope"].as_str().unwrap_or("").split('/').count(),
                doc["path"].as_str().unwrap_or("").to_owned(),
            )
        });
        Ok(documents)
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
        self.work_matches(query, limit, false)
    }

    /// Work relevant to `query`, best match first; see `WorkQuery::score`.
    /// `open_only` drops closed work and stale source-discovered proposals,
    /// which is what consultation reports as known work.
    fn work_matches(&self, query: Option<&str>, limit: usize, open_only: bool) -> Result<Value> {
        if let Some(work) = observatory::relevant_work(&self.root, query, limit, open_only)? {
            return Ok(work);
        }
        let query = query.unwrap_or("").trim().to_lowercase();
        let scorer = relevance::WorkQuery::new(&query);
        let mut statement = self.db.prepare("SELECT id,title,status,priority,kind,scope_json,evidence_json,depends_json,blocked_json,acceptance_json,verification_json,discovered_from,provenance,confidence,last_validated_snapshot,updated_at FROM work_items ORDER BY CASE priority WHEN 'critical' THEN 0 WHEN 'high' THEN 1 WHEN 'normal' THEN 2 ELSE 3 END, updated_at DESC")?;
        let mut scored = statement
            .query_map([], work_row)?
            .filter_map(Result::ok)
            .filter(|item| {
                // Unqueried listings and consultation hide source-comment proposals whose
                // source evidence has gone stale; an explicit query finds them.
                let stale_proposal = item["provenance"] == "SourceDoc"
                    && item["status"] == "proposed"
                    && item["confidence"].as_f64().unwrap_or(0.0) < 0.5;
                !(stale_proposal && (query.is_empty() || open_only))
                    && (!open_only
                        || relevance::work_status_is_open(item["status"].as_str().unwrap_or("")))
            })
            .filter_map(|item| scorer.score(&item).map(|score| (score, item)))
            .collect::<Vec<_>>();
        // Stable: equal scores keep the priority and recency order.
        scored.sort_by(|left, right| right.0.total_cmp(&left.0));
        let items = scored
            .into_iter()
            .take(limit)
            .map(|(_, item)| item)
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
        self.with_memory_write(|| self.work_propose_inner(input))
    }

    fn work_propose_inner(&self, input: WorkItemInput) -> Result<Value> {
        let revision = self.revision();
        let identity = format!("{}:{:?}", input.title, input.scope);
        let id = format!("work_{}", &blake3::hash(identity.as_bytes()).to_hex()[..12]);
        self.db.execute("INSERT INTO work_items(id,title,status,priority,kind,scope_json,evidence_json,depends_json,blocked_json,acceptance_json,verification_json,discovered_from,provenance,confidence,last_validated_snapshot,created_at,updated_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,'explicit MCP proposal','HumanDecision',1.0,?12,?13,?13) ON CONFLICT(id) DO UPDATE SET title=excluded.title,status=excluded.status,priority=excluded.priority,kind=excluded.kind,scope_json=excluded.scope_json,evidence_json=excluded.evidence_json,depends_json=excluded.depends_json,blocked_json=excluded.blocked_json,acceptance_json=excluded.acceptance_json,verification_json=excluded.verification_json,last_validated_snapshot=excluded.last_validated_snapshot,updated_at=excluded.updated_at", params![id,input.title,input.status,input.priority,input.kind,serde_json::to_string(&input.scope)?,serde_json::to_string(&input.evidence)?,serde_json::to_string(&input.depends_on)?,serde_json::to_string(&input.blocked_by)?,serde_json::to_string(&input.acceptance_criteria)?,serde_json::to_string(&input.verification)?,revision.workspace_digest,Utc::now().to_rfc3339()])?;
        self.refresh_memory_search_rows()?;
        Ok(
            json!({"id":id,"snapshot":self.snapshot(),"status":input.status,"provenance":"HumanDecision"}),
        )
    }

    pub fn work_update(&self, id: &str, patch: &Value) -> Result<Value> {
        self.with_memory_write(|| self.work_update_inner(id, patch))
    }

    fn work_update_inner(&self, id: &str, patch: &Value) -> Result<Value> {
        let current = self.db.query_row("SELECT status,priority,kind,scope_json,evidence_json,depends_json,blocked_json,acceptance_json,verification_json FROM work_items WHERE id=?1",[id],|row|Ok(json!({"status":row.get::<_,String>(0)?,"priority":row.get::<_,String>(1)?,"kind":row.get::<_,String>(2)?,"scope":serde_json::from_str::<Value>(&row.get::<_,String>(3)?).unwrap_or(json!([])),"evidence":serde_json::from_str::<Value>(&row.get::<_,String>(4)?).unwrap_or(json!([])),"depends_on":serde_json::from_str::<Value>(&row.get::<_,String>(5)?).unwrap_or(json!([])),"blocked_by":serde_json::from_str::<Value>(&row.get::<_,String>(6)?).unwrap_or(json!([])),"acceptance_criteria":serde_json::from_str::<Value>(&row.get::<_,String>(7)?).unwrap_or(json!([])),"verification":serde_json::from_str::<Value>(&row.get::<_,String>(8)?).unwrap_or(json!([]))})))?;
        let field = |name: &str| {
            patch
                .get(name)
                .cloned()
                .unwrap_or_else(|| current[name].clone())
        };
        self.db.execute("UPDATE work_items SET status=?1,priority=?2,kind=?3,scope_json=?4,evidence_json=?5,depends_json=?6,blocked_json=?7,acceptance_json=?8,verification_json=?9,last_validated_snapshot=?10,updated_at=?11 WHERE id=?12",params![field("status").as_str(),field("priority").as_str(),field("kind").as_str(),field("scope").to_string(),field("evidence").to_string(),field("depends_on").to_string(),field("blocked_by").to_string(),field("acceptance_criteria").to_string(),field("verification").to_string(),self.revision().workspace_digest,Utc::now().to_rfc3339(),id])?;
        self.refresh_memory_search_rows()?;
        Ok(json!({"id":id,"snapshot":self.snapshot(),"updated":true}))
    }

    /// Store contents plus whether the published generation is stale, judged
    /// exactly as the freshness envelope judges it.
    pub fn status(&self) -> Result<Value> {
        let freshness = index_freshness(&self.root, &self.db)?;
        let mut status = self.index_summary()?;
        status["stale"] = Value::from(freshness.stale());
        status["freshness_reason"] = Value::from(freshness.reason());
        Ok(status)
    }

    /// Snapshot identity, counts, and degradation notes for `index.status`,
    /// without a freshness judgement or backend details.
    pub fn index_summary(&self) -> Result<Value> {
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
        let architecture_reports: i64 =
            self.db
                .query_row("SELECT COUNT(*) FROM architecture_reports", [], |row| {
                    row.get(0)
                })?;
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
            json!({"snapshot":self.snapshot(),"counts":{"nodes":nodes,"edges":edges,"semantic_edges":semantic_edges,"unresolved_static_references":unresolved_static_references,"runtime_contract_artifacts":runtime_contracts,"embeddings":embeddings,"lifecycle_evidence":lifecycle,"work_items":work,"actionable_work_items":actionable_work,"package_targets":package_targets,"package_features":package_features,"architecture_reports":architecture_reports},"quality_memory":quality_memory,"unreadable_inputs":unreadable,"degraded_areas":degraded,"cache":"rebuildable SQLite cache with atomic published generations; architecture audits retain the newest 20 snapshot-scoped reports."}),
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

    /// Refreshes the derived index under the publisher lease.
    ///
    /// `incremental` (the default) re-indexes only what changed since the
    /// published generation and falls back to a full rebuild only for a store
    /// that was never indexed or was written by another indexer version.
    /// `workspace` (alias `full`, and `cargo`) rebuilds everything. `git`
    /// re-reads commit history only; it publishes no generation, so freshness
    /// stays stale until an incremental or full refresh indexes the sources.
    ///
    /// Returns a compact summary (generation, counts, timing, reused versus
    /// recomputed vectors); `status` describes the whole store.
    pub fn refresh(&mut self, scope: Option<&str>) -> Result<Value> {
        let started = Instant::now();
        let scope = scope.unwrap_or("incremental");
        let previous_generation = self.active_generation();
        self.last_refresh_timings = Value::Null;
        let outcome = match scope {
            "incremental" => self.refresh_if_stale()?,
            "workspace" | "full" | "cargo" => {
                let _publisher_lease = self.acquire_publisher_lease()?;
                self.reindex_unlocked()?
            }
            "git" => {
                let _publisher_lease = self.acquire_publisher_lease()?;
                let revision = self.revision();
                self.with_index_build(|service| {
                    service.refresh_git(&revision.workspace_digest, revision.head.as_deref())
                })?;
                RefreshOutcome {
                    mode: "git_history",
                    published: false,
                    changed_inputs: 0,
                }
            }
            other => bail!(
                "unsupported refresh scope `{other}`; use incremental, workspace (full), cargo, or git"
            ),
        };
        self.refresh_summary(scope, outcome, previous_generation, started)
    }

    fn refresh_summary(
        &self,
        scope: &str,
        outcome: RefreshOutcome,
        previous_generation: Option<i64>,
        started: Instant,
    ) -> Result<Value> {
        let count = |table: &str| -> Result<i64> {
            Ok(self
                .db
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })?)
        };
        let metadata_count = |key: &str| -> Result<Option<i64>> {
            Ok(self
                .metadata_value(key)?
                .and_then(|value| value.parse().ok()))
        };
        // Vector statistics describe the last build, so they are reported only
        // when this refresh produced one.
        let embeddings = if outcome.published {
            json!({
                "recomputed": metadata_count("embedding_recomputed")?,
                "reused": metadata_count("embedding_reused")?,
            })
        } else {
            Value::Null
        };
        let mut timings = self
            .last_refresh_timings
            .as_object()
            .cloned()
            .unwrap_or_default();
        timings.insert(
            "total_ms".into(),
            json!(started.elapsed().as_secs_f64() * 1000.0),
        );
        Ok(json!({
            "scope": scope,
            "mode": outcome.mode,
            "published": outcome.published,
            "generation": self.active_generation(),
            "previous_generation": previous_generation,
            "changed_inputs": outcome.changed_inputs,
            "counts": {
                "nodes": count("nodes")?,
                "edges": count("edges")?,
                "documents": count("documents")?,
                "inputs": count("input_state")?,
            },
            "embeddings": embeddings,
            "timings": timings,
        }))
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
        let analyzer = self.ra.index_facts(self.ra_enabled, &self.ra_program);
        json!({"semantic_engine":"rust-analyzer LSP companion (off until semantic.enable, warmed in the background when enabled, persisted by semantic snapshot)","rust_analyzer_enabled":self.ra_enabled,"rust_analyzer_state":analyzer["state"],"rust_analyzer_running":analyzer["running"],"rust_analyzer_start_attempted":analyzer["start_attempted"],"rust_analyzer_program":self.ra_program.to_string_lossy(),"rust_analyzer_program_found":analyzer["program_found"],"rust_analyzer_error":analyzer["error"],"rust_analyzer_restarts":analyzer["restarts"],"rust_analyzer_hint":analyzer["hint"],"fallback":"AST Syntax index with ambiguity suppression; unresolved candidates are reported separately","embedding_model":EMBEDDING_MODEL,"embedding_dimensions":EMBEDDING_DIMENSIONS,"embedding_card_version":SYMBOL_CARD_VERSION,"embedding":self.embedding_index_status(),"retrieval":"BM25 + embedding + typed graph RRF","store":"SQLite FTS5 + graph + vectors"})
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
        json!({"repository":self.root.to_string_lossy(),"branch":command_text(&self.root,&["branch","--show-current"]),"revision":self.indexed_revision(),"generation":self.active_generation(),"semantic_snapshot":semantic,"indexing_timestamp":self.db.query_row("SELECT value FROM metadata WHERE key='indexed_at'",[],|row|row.get::<_,String>(0)).optional().ok().flatten(),"indexer_version":self.db.query_row("SELECT value FROM metadata WHERE key='indexer_version'",[],|row|row.get::<_,String>(0)).optional().ok().flatten(),"cargo_assumptions":{"features":feature_profile,"target_triple":target_triple,"package_targets":self.db.query_row("SELECT COUNT(*) FROM package_targets",[],|row|row.get::<_,i64>(0)).unwrap_or(0),"package_features":self.db.query_row("SELECT COUNT(*) FROM package_features",[],|row|row.get::<_,i64>(0)).unwrap_or(0)}})
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
        // A fresh generation means the live inputs equal the stored ones, so
        // the semantic identity is computed without re-hashing the workspace.
        let snapshot_current = index_freshness(&self.root, &self.db)
            .is_ok_and(|freshness| !freshness.stale())
            && self
                .stored_inputs()
                .is_ok_and(|inputs| self.semantic_snapshot(&inputs).id == snapshot);
        let mut slices = Vec::new();
        let mut analyzer = None;
        for target in targets.iter().take(8) {
            if snapshot_current
                && let Some(cached) = self.cached_semantic_slices(target, relation, &snapshot)
            {
                slices.extend(cached);
                continue;
            }
            if self
                .source_slice(target, "semantic target", relation)
                .is_ok_and(|slice| slice.stale)
            {
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
            // The analyzer is acquired once, and only when a target misses the cache.
            let Some(process) = analyzer
                .get_or_insert_with(|| self.analyzer_for_index())
                .as_mut()
            else {
                continue;
            };
            let Ok(client) = process.client() else {
                continue;
            };
            if client.sync_document(&path, &text).is_err() {
                continue;
            }
            let method = if relation == "implementations" {
                "textDocument/implementation"
            } else {
                "textDocument/references"
            };
            let Some(locations) = client
                .request(
                    method,
                    json!({"textDocument":{"uri":live_semantics::file_uri(&path)},
                        "position":{"line":target.start_line - 1,"character":character},
                        "context":{"includeDeclaration":true}}),
                    Duration::from_secs(10),
                    &self.execution,
                )
                .ok()
                .and_then(|result| result.as_array().cloned())
            else {
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
                let Some(path) = rust_analyzer::uri_to_path(uri) else {
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
                if snapshot_current
                    && let Some(source) = self.node_at_location(&path, start as usize)
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
            if snapshot_current {
                let _ = self.db.execute(
                "INSERT OR REPLACE INTO semantic_queries(node_id,relation,semantic_snapshot,result_count,queried_at) VALUES (?1,?2,?3,?4,?5)",
                params![target.id,relation,snapshot,persisted as i64,Utc::now().to_rfc3339()],
            );
            }
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
                        .map(|mut slice| {
                            if !slice.stale {
                                slice.provenance = "RustAnalyzerCached".into();
                                slice.confidence = 1.0;
                            }
                            slice
                        })
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
                    content_hash: String::new(),
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
        let mut source = lines[start..end].join("\n");
        let content_hash = format!("b3:{}", blake3::hash(source.as_bytes()).to_hex());
        let stale = indexed_start >= lines.len()
            || node.end_line > lines.len()
            || content_hash != node.content_hash;
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
            content_hash,
            reason: if stale {
                format!("{reason}; indexed range or content differs from the live file")
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
    /// Accepted decisions relevant to `text`. Superseded and retired decisions
    /// are history, so every consumer that cites governing guidance uses this.
    fn decisions_for(&self, text: &str) -> Result<Vec<Value>> {
        self.decision_list(text, Some("accepted"), 20)
    }

    /// Decisions relevant to `text`, or the newest decisions in the ledger when
    /// `text` is blank, optionally narrowed to one status.
    ///
    /// A blank query used to fall through to the full-text search, which
    /// short-circuits on empty terms, so the ledger could not be enumerated.
    pub fn decision_list(
        &self,
        text: &str,
        status: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Value>> {
        let ids: Vec<String> = if text.trim().is_empty() {
            self.db
                .prepare("SELECT id FROM decisions WHERE (?1 IS NULL OR status=?1) ORDER BY sequence DESC LIMIT ?2")?
                .query_map(params![status, limit as i64], |row| row.get(0))?
                .collect::<rusqlite::Result<_>>()?
        } else {
            // Status is not part of the search index, so over-fetch and filter
            // below; the ledger is small enough that this stays cheap.
            self.search_hits(&terms(text), Some("decision"), MAX_DECISION_HITS)?
                .into_iter()
                .filter_map(|hit| hit["entity_id"].as_str().map(str::to_owned))
                .collect()
        };
        let mut decisions = Vec::new();
        for id in ids {
            if decisions.len() >= limit {
                break;
            }
            let decision = self.decision_resource(&id)?;
            if status.is_none_or(|status| decision["status"] == status) {
                decisions.push(decision);
            }
        }
        Ok(decisions)
    }

    fn decision_resource(&self, id: &str) -> Result<Value> {
        let mut decision = self.db.query_row("SELECT id,status,title,rationale,applies_to,consequences,supersedes,revision,created_at FROM decisions WHERE id=?1",[id],|r|Ok(json!({"id":r.get::<_,String>(0)?,"status":r.get::<_,String>(1)?,"title":r.get::<_,String>(2)?,"rationale":r.get::<_,String>(3)?,"applies_to":serde_json::from_str::<Value>(&r.get::<_,String>(4)?).unwrap_or(Value::Null),"consequences":serde_json::from_str::<Value>(&r.get::<_,String>(5)?).unwrap_or(Value::Null),"supersedes":supersedes_list(r.get::<_,Option<String>>(6)?),"revision":r.get::<_,String>(7)?,"created_at":r.get::<_,String>(8)?})))?;
        let superseded_by = self
            .db
            .prepare("SELECT d.id FROM decisions d, json_each(d.supersedes) j WHERE d.supersedes IS NOT NULL AND j.value=?1 ORDER BY d.sequence")?
            .query_map([id], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let history = self
            .db
            .prepare("SELECT action,actor,note,created_at FROM decision_reviews WHERE decision_id=?1 ORDER BY id")?
            .query_map([id], |row| {
                Ok(json!({
                    "action": row.get::<_, String>(0)?,
                    "actor": row.get::<_, String>(1)?,
                    "note": row.get::<_, String>(2)?,
                    "created_at": row.get::<_, String>(3)?,
                }))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let stale = self.stale_references(
            decision["rationale"].as_str().into_iter().chain(
                decision["applies_to"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str),
            ),
        );
        decision["superseded_by"] = json!(superseded_by);
        decision["history"] = json!(history);
        decision["stale_references"] = json!(stale);
        Ok(decision)
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
CREATE TABLE IF NOT EXISTS decision_reviews(id INTEGER PRIMARY KEY,decision_id TEXT NOT NULL REFERENCES decisions(id) ON DELETE CASCADE,action TEXT NOT NULL,actor TEXT NOT NULL,note TEXT NOT NULL,created_at TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS decision_review_history ON decision_reviews(decision_id,created_at);
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
CREATE TABLE IF NOT EXISTS architecture_reports(id TEXT PRIMARY KEY,revision TEXT NOT NULL,profile TEXT NOT NULL,scope TEXT,analyzer_version TEXT NOT NULL,payload TEXT NOT NULL,created_at TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS architecture_reports_created ON architecture_reports(created_at);
CREATE TABLE IF NOT EXISTS input_state(path TEXT PRIMARY KEY,kind TEXT NOT NULL,content_hash TEXT NOT NULL,revision TEXT NOT NULL);
";

/// Bump when a migration step outside the schema texts changes (the edge
/// de-duplication below, `steering::migrate`, or `normalize_supersedes`), so a
/// store already stamped with the current fingerprint runs the migrations once
/// more. Changes to `SCHEMA` or `quality::SCHEMA` change the fingerprint by
/// themselves.
const MIGRATION_REVISION: &str = "1";

/// Identity of the schema and migrations this build expects.
fn schema_fingerprint() -> String {
    let mut hasher = blake3::Hasher::new();
    for part in [SCHEMA_VERSION, MIGRATION_REVISION, SCHEMA, quality::SCHEMA] {
        hasher.update(part.as_bytes());
        hasher.update(b"\0");
    }
    format!("b3:{}", hasher.finalize().to_hex())
}

/// Whether the store already carries this build's schema. Read-only: a
/// connection that answers `true` performs no write while opening.
fn schema_is_current(db: &Connection, fingerprint: &str) -> bool {
    db.query_row(
        "SELECT (SELECT value FROM metadata WHERE key='schema_version'),\
                (SELECT value FROM metadata WHERE key='schema_fingerprint')",
        [],
        |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?,
            ))
        },
    )
    .is_ok_and(|(version, stamped)| {
        version.as_deref() == Some(SCHEMA_VERSION) && stamped.as_deref() == Some(fingerprint)
    })
}

/// Opens the index schema for one connection.
///
/// Every tool call opens a fresh `Service`, and migration used to run on each
/// open: it wrote unconditionally (duplicate-edge collapse, schema stamp), so
/// an ordinary read waited up to the busy timeout behind a refresh holding the
/// write transaction. Migration now runs only while the store's fingerprint
/// differs from this build's — once per store and schema, in whichever
/// process opens it first — and every other open only sets connection
/// pragmas, so readers see the last committed generation without waiting.
fn initialize_schema(db: &Connection) -> Result<()> {
    db.execute_batch("PRAGMA foreign_keys=ON;")?;
    let fingerprint = schema_fingerprint();
    if schema_is_current(db, &fingerprint) {
        return Ok(());
    }
    db.execute_batch("PRAGMA journal_mode=WAL;")?;
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
        if stored != SCHEMA_VERSION && !matches!(stored, "7" | "6" | "5" | "4") {
            // Derived data is rebuildable, so an incompatible older generation is
            // discarded and reindexed. Human-authored tables are deliberately
            // absent from this batch: decisions and their review history,
            // steerings, work_items, the problem records, and the
            // quality/validation lifecycle survive.
            db.execute_batch("PRAGMA foreign_keys=OFF;
                DROP TABLE IF EXISTS search_index;
                DROP TABLE IF EXISTS symbol_embeddings; DROP TABLE IF EXISTS semantic_queries; DROP TABLE IF EXISTS index_generations; DROP TABLE IF EXISTS semantic_snapshots;
                DROP TABLE IF EXISTS input_state; DROP TABLE IF EXISTS change_contexts; DROP TABLE IF EXISTS architecture_reports;
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
    steering::migrate(db)?;
    normalize_supersedes(db)?;
    quality::initialize(db)?;
    db.execute(
        "INSERT OR REPLACE INTO metadata(key, value) VALUES ('schema_version', ?1)",
        [SCHEMA_VERSION],
    )?;
    db.execute(
        "INSERT OR REPLACE INTO metadata(key, value) VALUES ('schema_fingerprint', ?1)",
        [fingerprint],
    )?;
    Ok(())
}

/// Rewrites `decisions.supersedes` values written before multi-target
/// supersession, when the column held one bare decision ID, into the JSON array
/// form so `json_each` can compute `superseded_by` without tripping on them.
fn normalize_supersedes(db: &Connection) -> Result<()> {
    let rows: Vec<(String, String)> = db
        .prepare("SELECT id,supersedes FROM decisions WHERE supersedes IS NOT NULL")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (id, stored) in rows {
        if serde_json::from_str::<Vec<String>>(&stored).is_err() {
            db.execute(
                "UPDATE decisions SET supersedes=?1 WHERE id=?2",
                params![serde_json::to_string(&[stored.trim()])?, id],
            )?;
        }
    }
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
            // Binary patches, empty files, and mode-only changes may have no
            // ---/+++ headers. With unchanged names, the two Git header paths
            // have equal byte lengths, including quoting and embedded spaces.
            // Renames and copies use their explicit directives below.
            if let Some(paths) = line.strip_prefix("diff --git ") {
                let middle = paths.len() / 2;
                if paths.as_bytes().get(middle) == Some(&b' ')
                    && let (Some(old), Some(new)) = (paths.get(..middle), paths.get(middle + 1..))
                    && let (Some(old), Some(new)) = (diff_path(old), diff_path(new))
                    && old == new
                {
                    files.insert(old);
                }
            }
            continue;
        }
        if line.starts_with("@@") {
            in_hunk = true;
            continue;
        }
        if in_hunk {
            continue;
        }
        if let Some(rest) = line
            .strip_prefix("rename from ")
            .or_else(|| line.strip_prefix("copy from "))
        {
            files.extend(diff_path(rest));
        } else if let Some(rest) = line
            .strip_prefix("rename to ")
            .or_else(|| line.strip_prefix("copy to "))
        {
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
/// How the published generation relates to the live workspace.
#[derive(Debug, Clone, Default)]
pub(crate) struct IndexFreshness {
    pub(crate) generation: Option<i64>,
    pub(crate) published_at: Option<String>,
    pub(crate) workspace_digest: Option<String>,
    pub(crate) indexed_head: Option<String>,
    pub(crate) live_head: Option<String>,
    /// Inputs whose live content differs from the published input state.
    pub(crate) changed_inputs: Vec<String>,
    /// Live inputs that differ from `HEAD`, indexed or not.
    pub(crate) dirty_inputs: usize,
    /// The generation was written by another indexer version.
    pub(crate) indexer_outdated: bool,
}

impl IndexFreshness {
    /// `never_published`, `indexer_outdated`, `head_moved`,
    /// `worktree_changed`, or `ok`.
    pub(crate) fn reason(&self) -> &'static str {
        if self.generation.is_none() {
            "never_published"
        } else if self.indexer_outdated {
            "indexer_outdated"
        } else if self.live_head != self.indexed_head {
            "head_moved"
        } else if !self.changed_inputs.is_empty() {
            "worktree_changed"
        } else {
            "ok"
        }
    }

    pub(crate) fn stale(&self) -> bool {
        self.reason() != "ok"
    }
}

/// Compares the live workspace with the generation published in `db`.
///
/// Inside Git this costs `rev-parse`, one `git status`, and hashing only the
/// inputs that are dirty now or were dirty when indexed: every other input is
/// clean, so it equals its blob at `HEAD`, and an unchanged head means it
/// equals what was indexed. A dirty tree that was refreshed therefore reads
/// fresh, and files that are not inputs (Crusty's state, `target/`, editor
/// droppings) never make it stale. Outside Git, or for a store that never
/// recorded its dirty inputs, every input is re-hashed.
pub(crate) fn index_freshness(root: &Path, db: &Connection) -> Result<IndexFreshness> {
    let mut freshness = IndexFreshness::default();
    let Some((generation, digest, published_at)) = db
        .query_row(
            "SELECT id,workspace_digest,published_at FROM index_generations WHERE status='published' ORDER BY id DESC LIMIT 1",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, Option<String>>(2)?)),
        )
        .optional()?
    else {
        return Ok(freshness);
    };
    freshness.generation = Some(generation);
    freshness.workspace_digest = Some(digest);
    freshness.published_at = published_at;
    let metadata = |key: &str| -> Result<Option<String>> {
        Ok(db
            .query_row("SELECT value FROM metadata WHERE key=?1", [key], |row| {
                row.get::<_, String>(0)
            })
            .optional()?)
    };
    freshness.indexed_head = metadata("git_indexed_head")?.filter(|head| !head.is_empty());
    // An older indexer's generation lacks evidence this build records, so it
    // is stale even when every input is unchanged.
    freshness.indexer_outdated = metadata("indexer_version")?.as_deref() != Some(INDEXER_VERSION);
    freshness.live_head = command_text(root, &["rev-parse", "HEAD"]);
    let live_dirty = git_dirty_paths(root).map(|paths| {
        paths
            .into_iter()
            .filter(|path| is_input_path(root, path))
            .collect::<BTreeSet<_>>()
    });
    freshness.dirty_inputs = live_dirty.as_ref().map_or(0, BTreeSet::len);
    match (live_dirty, stored_worktree_inputs(db)?) {
        (Some(live_dirty), Some(indexed_dirty)) => {
            let candidates = live_dirty
                .union(&indexed_dirty)
                .cloned()
                .collect::<Vec<_>>();
            let existing = candidates
                .iter()
                .filter(|path| root.join(path).is_file())
                .cloned()
                .collect::<Vec<_>>();
            let live = content_hashes(root, &existing, true);
            let mut statement = db.prepare("SELECT content_hash FROM input_state WHERE path=?1")?;
            for path in candidates {
                let indexed = statement
                    .query_row([&path], |row| row.get::<_, String>(0))
                    .optional()?;
                if live.get(&path) != indexed.as_ref() {
                    freshness.changed_inputs.push(path);
                }
            }
        }
        _ => {
            let indexed = db
                .prepare("SELECT path, kind, content_hash FROM input_state")?
                .query_map([], |row| Ok((row.get(0)?, (row.get(1)?, row.get(2)?))))?
                .collect::<rusqlite::Result<BTreeMap<String, (String, String)>>>()?;
            freshness.changed_inputs = changed_inputs(&indexed, &index_inputs(root))
                .into_iter()
                .map(|(path, _)| path)
                .collect();
        }
    }
    Ok(freshness)
}

fn stored_worktree_inputs(db: &Connection) -> Result<Option<BTreeSet<String>>> {
    let value = db
        .query_row(
            "SELECT value FROM metadata WHERE key='indexed_worktree_inputs'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    Ok(match value {
        Some(value) => Some(serde_json::from_str(&value)?),
        None => None,
    })
}

/// Content identity of every index input, keyed by repository-relative path.
///
/// Inside a Git work tree every input is identified by its Git blob id: clean
/// tracked files take it from the index without reading them, while dirty and
/// untracked files are hashed with `git hash-object`. Identical content
/// therefore has one identity whether it is committed, modified, or untracked;
/// clean files used to be `git:<oid>` and dirty ones `b3:<hash>`, so merely
/// committing an indexed edit looked like a change to every committed file.
/// Outside Git the identity is a BLAKE3 content hash.
fn index_inputs(root: &Path) -> BTreeMap<String, (String, String)> {
    index_inputs_with_worktree(root).0
}

/// `index_inputs` together with the input paths Git reported as differing from
/// `HEAD` (modified, staged, untracked, or deleted) before they were hashed.
fn index_inputs_with_worktree(
    root: &Path,
) -> (BTreeMap<String, (String, String)>, BTreeSet<String>) {
    let blob_ids = git_tracked_blob_ids(root);
    let dirty_paths = git_dirty_paths(root).unwrap_or_default();
    let mut inputs = BTreeMap::new();
    let mut unhashed = Vec::new();
    for path in workspace_files(root) {
        let Some(kind) = input_kind(&path) else {
            continue;
        };
        let relative_path = relative(root, &path);
        match blob_ids
            .as_ref()
            .and_then(|ids| ids.get(&relative_path))
            .filter(|_| !dirty_paths.contains(&relative_path))
        {
            Some(oid) => {
                inputs.insert(relative_path, (kind.to_owned(), format!("git:{oid}")));
            }
            None => unhashed.push((relative_path, kind)),
        }
    }
    let paths = unhashed
        .iter()
        .map(|(path, _)| path.clone())
        .collect::<Vec<_>>();
    let mut hashes = content_hashes(root, &paths, blob_ids.is_some());
    for (path, kind) in unhashed {
        if let Some(hash) = hashes.remove(&path) {
            inputs.insert(path, (kind.to_owned(), hash));
        }
    }
    let worktree = dirty_paths
        .into_iter()
        .filter(|path| is_input_path(root, path))
        .collect();
    (inputs, worktree)
}

/// Whether a repository-relative path can be an index input: it has an input
/// kind and lies outside Crusty's state directory and Cargo target directories.
fn is_input_path(root: &Path, path: &str) -> bool {
    input_kind(Path::new(path)).is_some() && !excluded_from_inputs(root, path)
}

/// Crusty's own state directory (at any depth, since a server once started in
/// a subdirectory left one there) and Cargo target directories never hold
/// inputs, even in repositories that do not ignore them. A `target` directory
/// counts only beside a `Cargo.toml`, so a source module named `target` stays.
pub(crate) fn excluded_from_inputs(root: &Path, path: &str) -> bool {
    let mut parent = root.to_path_buf();
    let components = path.split('/').collect::<Vec<_>>();
    for component in components.iter().take(components.len().saturating_sub(1)) {
        if *component == INDEX_DIRECTORY
            || *component == ".git"
            || (*component == "target" && parent.join("Cargo.toml").is_file())
        {
            return true;
        }
        parent.push(component);
    }
    false
}

/// Hashes files on disk with the identity scheme of `index_inputs`. Files that
/// cannot be read are absent from the result.
fn content_hashes(root: &Path, paths: &[String], git: bool) -> HashMap<String, String> {
    let mut hashes = HashMap::new();
    if git {
        for chunk in paths.chunks(256) {
            let mut args = vec!["hash-object", "--"];
            args.extend(chunk.iter().map(String::as_str));
            // One unreadable path fails the whole invocation; that chunk then
            // falls back to BLAKE3, which costs at most one re-indexing of it.
            let Some(output) = git_output(root, &args) else {
                continue;
            };
            let output = String::from_utf8_lossy(&output);
            let ids = output.lines().collect::<Vec<_>>();
            if ids.len() == chunk.len() {
                for (path, id) in chunk.iter().zip(ids) {
                    hashes.insert(path.clone(), format!("git:{}", id.trim()));
                }
            }
        }
    }
    for path in paths {
        if !hashes.contains_key(path)
            && let Ok(bytes) = fs::read(root.join(path))
        {
            hashes.insert(
                path.clone(),
                format!("b3:{}", blake3::hash(&bytes).to_hex()),
            );
        }
    }
    hashes
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

fn changed_inputs_still_match(
    root: &Path,
    inputs: &BTreeMap<String, (String, String)>,
    changed: &[(String, String)],
) -> bool {
    // Each input is re-hashed with the scheme that produced its identity.
    let (git, other): (Vec<String>, Vec<String>) = changed
        .iter()
        .filter(|(path, _)| inputs.contains_key(path))
        .map(|(path, _)| path.clone())
        .partition(|path| inputs[path].1.starts_with("git:"));
    let mut current = content_hashes(root, &git, true);
    current.extend(content_hashes(root, &other, false));
    changed.iter().all(|(path, _)| match inputs.get(path) {
        None => !root.join(path).exists(),
        Some((_, expected)) => current.get(path) == Some(expected),
    })
}

/// Inputs added, modified, or removed between two input states, with their
/// kind (the previous kind for removed inputs).
fn changed_inputs(
    previous: &BTreeMap<String, (String, String)>,
    inputs: &BTreeMap<String, (String, String)>,
) -> Vec<(String, String)> {
    inputs
        .iter()
        .filter(|(path, input)| previous.get(*path) != Some(*input))
        .map(|(path, (kind, _))| (path.clone(), kind.clone()))
        .chain(
            previous
                .iter()
                .filter(|(path, _)| !inputs.contains_key(*path))
                .map(|(path, (kind, _))| (path.clone(), kind.clone())),
        )
        .collect()
}

/// Files recorded as producing a syntax edge: `files` when present (edges a
/// short-name collision lets several files yield), otherwise `file`.
fn edge_contributors(metadata: &str) -> BTreeSet<String> {
    let Ok(value) = serde_json::from_str::<Value>(metadata) else {
        return BTreeSet::new();
    };
    match value.get("files").and_then(Value::as_array) {
        Some(files) => files
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        None => value
            .get("file")
            .and_then(Value::as_str)
            .map(|file| BTreeSet::from([file.to_owned()]))
            .unwrap_or_default(),
    }
}

fn contributors_metadata(files: &BTreeSet<String>) -> Value {
    json!({"file": files.first(), "files": files})
}

/// Among same-named candidates, the first by location. Short-name lookups used
/// to take the lowest node id, which depends on which symbols an incremental
/// refresh re-inserted, so incremental and full results could differ.
fn first_by_location<'a>(candidates: impl Iterator<Item = &'a Node>) -> Option<&'a Node> {
    candidates.min_by(|left, right| {
        (
            left.file.as_str(),
            left.start_line,
            left.canonical_name.as_str(),
        )
            .cmp(&(
                right.file.as_str(),
                right.start_line,
                right.canonical_name.as_str(),
            ))
    })
}

/// Orders enclosing candidates innermost first, breaking ties by location
/// rather than by node id.
fn innermost_key(node: &Node) -> (usize, usize, &str) {
    (
        node.end_line.saturating_sub(node.start_line),
        node.start_line,
        node.canonical_name.as_str(),
    )
}

/// Numbered SQL parameters `?1,…,?count`; a numbered parameter may appear
/// several times in one statement while being bound once.
fn placeholders(count: usize) -> String {
    (1..=count)
        .map(|index| format!("?{index}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// Index blob ids of tracked files, or `None` outside a Git work tree.
fn git_tracked_blob_ids(root: &Path) -> Option<HashMap<String, String>> {
    let output = git_output(root, &["ls-files", "-s", "-z"])?;
    let ids = output
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
        .collect();
    Some(ids)
}

/// Paths `git status` reports as differing from `HEAD`, including untracked
/// files and both sides of a rename, or `None` outside a Git work tree.
///
/// Porcelain paths are relative to the repository top level while every other
/// input path is relative to `root`, so a root below the top level has its
/// prefix stripped and paths outside it dropped.
fn git_dirty_paths(root: &Path) -> Option<BTreeSet<String>> {
    let prefix = command_text(root, &["rev-parse", "--show-prefix"])?;
    let output = git_output(
        root,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--",
            ".",
        ],
    )?;
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
    if prefix.is_empty() {
        return Some(dirty);
    }
    Some(
        dirty
            .into_iter()
            .filter_map(|path| path.strip_prefix(&prefix).map(str::to_owned))
            .collect(),
    )
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
            .map(|path| String::from_utf8_lossy(path).into_owned())
            .filter(|path| !excluded_from_inputs(root, path))
            .map(|path| root.join(path))
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
        // Stopwords and single letters OR-ed into the FTS query matched almost
        // every row; they are dropped unless nothing else remains.
        relevance::meaningful_terms(values)
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
    let (chunks, _) = bytes.as_chunks::<4>();
    chunks
        .iter()
        .map(|chunk| f32::from_le_bytes(*chunk))
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

/// Reads the `supersedes` column, a JSON array of decision IDs. Databases
/// written before multi-target supersession stored one bare ID;
/// `initialize_schema` rewrites those, and this tolerates one that slipped
/// through.
fn supersedes_list(stored: Option<String>) -> Vec<String> {
    let Some(text) = stored else {
        return Vec::new();
    };
    serde_json::from_str::<Vec<String>>(&text)
        .unwrap_or_else(|_| vec![text])
        .into_iter()
        .filter(|id| !id.trim().is_empty())
        .collect()
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
/// Agent and contributor instruction files that `live_instructions` reads live.
const INSTRUCTION_FILES: &[&str] = &["AGENTS.md", "CLAUDE.md", "CONTEXT.md", "CONTRIBUTING.md"];

fn is_instruction_file(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    INSTRUCTION_FILES.contains(&name)
}

/// The first `count` entries of a JSON array, or the value itself.
fn first_values(value: &Value, count: usize) -> Value {
    match value.as_array() {
        Some(values) => json!(values.iter().take(count).collect::<Vec<_>>()),
        None => value.clone(),
    }
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
fn decision_markdown(id: &str, d: &RecordDecision, supersedes: &[String]) -> String {
    let supersedes = if supersedes.is_empty() {
        String::new()
    } else {
        format!("\nSupersedes: {}\n", supersedes.join(", "))
    };
    format!(
        "# {id}: {}\n\nStatus: {}\n{supersedes}\n## Rationale\n\n{}\n\n## Applies to\n\n{}\n\n## Consequences\n\n{}\n",
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
fn semantic_location(slice: &SourceSlice) -> Value {
    json!({"symbol":slice.symbol,"file":slice.file,"line":slice.range[0],"end_line":slice.range[1],"location":format!("{}:{}",slice.file,slice.range[0]),"provenance":slice.provenance,"confidence":slice.confidence,"stale":slice.stale})
}

fn validation_verdict(
    run_checks: bool,
    checks: &[Value],
    artifacts: &[Value],
    learned: &[Value],
    blocking: &Value,
) -> Value {
    let failed = checks
        .iter()
        .chain(artifacts)
        .filter(|check| check["success"] == false)
        .count()
        + learned
            .iter()
            .filter(|check| check["status"] == "failed")
            .count();
    let unavailable = artifacts
        .iter()
        .filter(|check| check["skipped"] == true)
        .count()
        + learned
            .iter()
            .filter(|check| {
                matches!(
                    check["status"].as_str(),
                    Some("unavailable" | "manual_required" | "queued" | "skipped")
                )
            })
            .count();
    json!({"verdict": if !run_checks { "not_run" } else if failed > 0 { "failed" } else if unavailable > 0 || blocking["blocked"] == true { "incomplete" } else { "passed" }, "failed_checks":failed,"unavailable_checks":unavailable,"checks_requested":run_checks,"note":"Task completion means the report was produced. This verdict describes executed checks; human-approved blocking obligations are reported separately."})
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

fn run_optional_command_check(
    root: &Path,
    program: &str,
    args: &[OsString],
    control: &execution::ExecutionControl,
) -> Value {
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
    match control.output(Command::new(program).args(args).current_dir(root)) {
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
    fn opening_service_does_not_start_rust_analyzer() {
        let d = fixture();
        let mut service = Service::open(d.path()).unwrap();
        service.ra_enabled = false;
        assert_eq!(service.index_status()["rust_analyzer_running"], false);
        assert_eq!(
            service.index_status()["rust_analyzer_start_attempted"],
            false
        );
        assert!(service.index_status()["rust_analyzer_program"].is_string());
        assert_eq!(service.index_status()["rust_analyzer_state"], "disabled");
        assert!(
            service.index_status()["rust_analyzer_hint"]
                .as_str()
                .unwrap()
                .contains("semantic.enable")
        );
    }

    #[test]
    fn consultation_says_how_to_enable_a_disabled_rust_analyzer() {
        let d = fixture();
        let mut service = Service::open(d.path()).unwrap();
        let hinted = |service: &Service| {
            service.consult("Rename the store", 3_000).unwrap()["next_steps"]
                .to_string()
                .contains("semantic.enable")
        };
        service.ra_enabled = false;
        assert!(hinted(&service));
        service.ra_enabled = true;
        assert!(!hinted(&service));
    }

    #[test]
    fn rust_analyzer_start_failure_is_actionable_status() {
        let d = fixture();
        let mut service = Service::open(d.path()).unwrap();
        service.ra_enabled = true;
        service.ra_program = PathBuf::from("/definitely/missing/rust-analyzer");
        assert!(service.analyzer_for_index().is_none());
        assert!(service.analyzer_for_index().is_none());
        let status = service.index_status();
        assert_eq!(status["rust_analyzer_start_attempted"], true);
        assert_eq!(status["rust_analyzer_running"], false);
        assert_eq!(status["rust_analyzer_program_found"], false);
        assert!(
            status["rust_analyzer_hint"]
                .as_str()
                .unwrap()
                .contains("was not found")
        );
        assert!(
            status["rust_analyzer_error"]
                .as_str()
                .unwrap()
                .contains("starting rust-analyzer")
        );
        // The first failure retries at once; repeated failures back off.
        assert_eq!(status["rust_analyzer_state"], "restarting");
        assert_eq!(service.semantic_status().unwrap()["starts"], 2);
        assert!(service.analyzer_for_index().is_none());
        assert_eq!(service.semantic_status().unwrap()["starts"], 2);
    }

    #[test]
    fn audit_same_length_source_edit_is_stale() {
        let d = fixture();
        let mut service = Service::open(d.path()).unwrap();
        service.refresh_if_stale().unwrap();
        let node = service.search_nodes(&terms("Store"), 1).unwrap().remove(0);
        let path = d.path().join(&node.file);
        fs::write(
            &path,
            fs::read_to_string(&path).unwrap().replace("Store", "Other"),
        )
        .unwrap();
        let slice = service.source_slice(&node, "audit", "DEFINES").unwrap();
        assert!(slice.stale);
        assert_ne!(slice.content_hash, node.content_hash);
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
    fn change_validation_reports_new_architecture_debt_without_blocking() {
        let d = fixture();
        let mut service = Service::open(d.path()).unwrap();
        service.refresh_if_stale().unwrap();
        let prepared = service
            .prepare_change("extend job state", &["src/lib.rs".into()], 1, None)
            .unwrap();
        assert_eq!(prepared["architecture_guard"]["policy"], "advisory_only");
        fs::write(
            d.path().join("src/lib.rs"),
            "pub trait Store { fn load(&self); }\n\
             fn uses(s: &dyn Store) { s.load(); }\n\
             pub struct Job { active: bool, running: bool, failed: bool }\n",
        )
        .unwrap();
        let diff = "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -2,0 +3 @@\n+pub struct Job { active: bool, running: bool, failed: bool }\n";
        let validated = service
            .validate_change(prepared["context_id"].as_str().unwrap(), Some(diff), false)
            .unwrap();
        assert_eq!(validated["architecture_delta"]["attention_required"], true);
        assert!(
            validated["architecture_delta"]["new"]
                .as_array()
                .unwrap()
                .iter()
                .any(|finding| finding["rule_id"] == "boolean-state-cluster")
        );
        assert_eq!(validated["blocking"]["blocked"], false);
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
                supersedes: vec![],
                recorded_by: String::new(),
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
                supersedes: vec![],
                recorded_by: String::new(),
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
            "architecture_reports",
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
                    supersedes: vec![],
                    recorded_by: String::new(),
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
                    supersedes: vec![],
                    recorded_by: String::new(),
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
    fn validation_git_comparison_covers_committed_and_pending_changes() {
        let d = git_fixture();
        fs::write(d.path().join("deleted.txt"), "old\n").unwrap();
        fs::write(d.path().join("old-name.txt"), "rename me\n").unwrap();
        git(d.path(), &["add", "."]);
        git(d.path(), &["commit", "-qm", "base"]);
        let base = command_text(d.path(), &["rev-parse", "HEAD"]).unwrap();
        let mut service = Service::open(d.path()).unwrap();
        service.refresh_if_stale().unwrap();
        let context = service
            .prepare_change("Update files", &[], 1, Some(1_000))
            .unwrap();
        let context_id = context["context_id"].as_str().unwrap();

        fs::write(d.path().join("committed.txt"), "committed\n").unwrap();
        fs::remove_file(d.path().join("deleted.txt")).unwrap();
        fs::rename(d.path().join("old-name.txt"), d.path().join("new-name.txt")).unwrap();
        git(
            d.path(),
            &[
                "add",
                "committed.txt",
                "deleted.txt",
                "old-name.txt",
                "new-name.txt",
            ],
        );
        git(d.path(), &["commit", "-qm", "committed changes"]);
        let head = command_text(d.path(), &["rev-parse", "HEAD"]).unwrap();
        fs::write(d.path().join("staged.txt"), "staged\n").unwrap();
        git(d.path(), &["add", "staged.txt"]);
        fs::write(d.path().join("src/lib.rs"), "pub struct Pending;\n").unwrap();
        fs::write(d.path().join("untracked.txt"), "only pending\n").unwrap();
        // Local validation must not invoke configured external diff programs.
        git(
            d.path(),
            &["config", "diff.external", "nonexistent-diff-program"],
        );
        let committed = service
            .validate_change_from_source(
                Some(context_id),
                &DiffSource::GitComparison {
                    base_ref: base.clone(),
                    target: DiffTarget::Head,
                },
                false,
            )
            .unwrap();
        assert_eq!(
            committed["changed_files"],
            json!([
                "committed.txt",
                "deleted.txt",
                "new-name.txt",
                "old-name.txt"
            ])
        );
        assert_eq!(committed["diff_scope"]["base_commit"], base);
        assert_eq!(committed["diff_scope"]["target_commit"], head);
        assert_eq!(committed["analysis_target"], "current_worktree");
        let combined = service
            .validate_change_from_source(
                Some(context_id),
                &DiffSource::GitComparison {
                    base_ref: base,
                    target: DiffTarget::Worktree,
                },
                false,
            )
            .unwrap();
        assert_eq!(
            combined["changed_files"],
            json!([
                "committed.txt",
                "deleted.txt",
                "new-name.txt",
                "old-name.txt",
                "src/lib.rs",
                "staged.txt"
            ])
        );
        assert_eq!(combined["diff_scope"]["head_commit"], head);
        assert!(combined["diff_scope"]["target_commit"].is_null());
        // Only the default pending diff includes untracked, non-ignored files
        // (the fixture never commits its Cargo.lock); the explicit comparisons
        // above keep excluding them.
        let pending = service.validate_change(context_id, None, false).unwrap();
        assert_eq!(
            pending["changed_files"],
            json!(["Cargo.lock", "src/lib.rs", "staged.txt", "untracked.txt"])
        );
        assert_eq!(pending["diff_scope"]["source"], "pending");
        assert_eq!(pending["diff_scope"]["tracked_only"], false);
        assert_eq!(
            pending["diff_scope"]["untracked"]["paths"],
            json!(["Cargo.lock", "untracked.txt"])
        );
        assert!(
            pending["diff_scope"]["untracked"]["paths"]
                .as_array()
                .unwrap()
                .iter()
                .all(|path| !path
                    .as_str()
                    .unwrap()
                    .starts_with(".rust-repo-intelligence/"))
        );
    }

    #[test]
    fn validation_without_a_prepared_context_reports_only_what_needs_one_as_unavailable() {
        let d = git_fixture();
        let mut service = Service::open(d.path()).unwrap();
        service.refresh_if_stale().unwrap();
        fs::write(d.path().join("src/lib.rs"), "pub struct Unprepared;\n").unwrap();
        fs::write(d.path().join("notes.md"), "new file\n").unwrap();
        let report = service
            .validate_change_from_source(None, &DiffSource::Pending, false)
            .unwrap();
        assert_eq!(report["prepared"], false);
        assert!(
            report["changed_files"]
                .as_array()
                .unwrap()
                .iter()
                .any(|file| file == "src/lib.rs")
        );
        assert!(
            report["changed_files"]
                .as_array()
                .unwrap()
                .iter()
                .any(|file| file == "notes.md")
        );
        for field in ["architecture_delta", "unmodified_expected_callers"] {
            assert_eq!(report[field]["status"], "unavailable", "{field}");
            assert_eq!(report[field]["reason"], "no prepared context", "{field}");
        }
        // Diff-driven checks still run.
        assert!(report["architectural_violations"].is_array());
        assert!(report["legacy_paths_touched"].is_array());
        assert_eq!(report["validation_status"]["verdict"], "not_run");
        assert_eq!(report["blocking"]["blocked"], false);
        // The recorded context owns the activated obligations and can be
        // validated again by id.
        let context_id = report["context_id"].as_str().unwrap();
        assert!(context_id.starts_with("ctx_"));
        assert!(service.quality_validation_queue(context_id).is_ok());
        let again = service.validate_change(context_id, None, false).unwrap();
        assert_eq!(again["prepared"], false);
        assert_eq!(again["architecture_delta"]["reason"], "no prepared context");
    }

    #[test]
    fn one_prepared_context_serves_every_validation_of_a_long_change() {
        let d = git_fixture();
        let mut service = Service::open(d.path()).unwrap();
        service.refresh_if_stale().unwrap();
        let context = service
            .prepare_change("Iterative refactor", &[], 1, Some(1_000))
            .unwrap();
        let context_id = context["context_id"].as_str().unwrap();
        let mut seen = Vec::new();
        for (step, file) in ["first.rs", "second.rs", "third.rs"].iter().enumerate() {
            fs::write(
                d.path().join("src").join(file),
                format!("pub fn step{step}() {{}}\n"),
            )
            .unwrap();
            let report = service.validate_change(context_id, None, false).unwrap();
            assert_eq!(report["context_id"], context_id);
            assert_eq!(report["prepared"], true);
            assert_ne!(report["architecture_delta"]["status"], "unavailable");
            assert!(report["unmodified_expected_callers"].is_array());
            seen.push(format!("src/{file}"));
            let changed = report["changed_files"].as_array().unwrap();
            assert!(
                seen.iter()
                    .all(|file| changed.iter().any(|changed| changed == file)),
                "milestone {step} must cover every file changed so far: {changed:?}"
            );
        }
    }

    #[test]
    fn validation_large_local_patch_matches_git_and_inline_inputs() {
        let d = git_fixture();
        fs::write(
            d.path().join("large.txt"),
            "a long repeated line of obsolete repository content to remove completely\n"
                .repeat(16_000),
        )
        .unwrap();
        git(d.path(), &["add", "large.txt"]);
        git(d.path(), &["commit", "-qm", "large base"]);
        let base = command_text(d.path(), &["rev-parse", "HEAD"]).unwrap();
        let mut service = Service::open(d.path()).unwrap();
        service.refresh_if_stale().unwrap();
        let context = service
            .prepare_change("Remove obsolete content", &[], 1, Some(1_000))
            .unwrap();
        let context_id = context["context_id"].as_str().unwrap();
        fs::remove_file(d.path().join("large.txt")).unwrap();
        let source = DiffSource::GitComparison {
            base_ref: base,
            target: DiffTarget::Worktree,
        };
        let resolved = source.resolve(d.path()).unwrap();
        assert!(resolved.text.len() > 890_000);
        fs::write(d.path().join("validation.patch"), &resolved.text).unwrap();
        let git_report = service
            .validate_change_from_source(Some(context_id), &source, false)
            .unwrap();
        let file_report = service
            .validate_change_from_source(
                Some(context_id),
                &DiffSource::PatchFile("validation.patch".into()),
                false,
            )
            .unwrap();
        let inline_report = service
            .validate_change(context_id, Some(&resolved.text), false)
            .unwrap();
        for report in [&git_report, &file_report, &inline_report] {
            assert_eq!(report["changed_files"], json!(["large.txt"]));
            assert_eq!(report["diff_scope"]["bytes"], resolved.text.len());
            assert_eq!(report["diff_scope"]["hash"], resolved.scope["hash"]);
        }
        assert_eq!(file_report["diff_scope"]["source"], "patch_file");
        assert_eq!(inline_report["diff_scope"]["source"], "inline");
        let absolute = DiffSource::PatchFile(d.path().join("validation.patch"))
            .resolve(d.path())
            .unwrap();
        assert_eq!(absolute.scope["hash"], resolved.scope["hash"]);
    }

    #[test]
    fn validation_git_scope_includes_binary_empty_and_mode_only_changes() {
        let d = git_fixture();
        fs::write(d.path().join("image with spaces.bin"), [0, 1, 2, 3]).unwrap();
        fs::write(d.path().join("mode.txt"), "mode\n").unwrap();
        git(d.path(), &["add", "image with spaces.bin", "mode.txt"]);
        git(d.path(), &["commit", "-qm", "artifact base"]);
        let mut service = Service::open(d.path()).unwrap();
        service.refresh_if_stale().unwrap();
        let context = service
            .prepare_change("Update artifacts", &[], 1, Some(1_000))
            .unwrap();
        fs::write(d.path().join("image with spaces.bin"), [0, 4, 5, 6]).unwrap();
        fs::write(d.path().join("empty.txt"), "").unwrap();
        git(d.path(), &["add", "image with spaces.bin", "empty.txt"]);
        git(d.path(), &["update-index", "--chmod=+x", "mode.txt"]);
        git(d.path(), &["commit", "-qm", "artifact changes"]);
        let source = DiffSource::GitComparison {
            base_ref: "HEAD~1".into(),
            target: DiffTarget::Head,
        };
        let resolved = source.resolve(d.path()).unwrap();
        assert!(resolved.text.contains("GIT binary patch"));
        let report = service
            .validate_change_from_source(
                Some(context["context_id"].as_str().unwrap()),
                &source,
                false,
            )
            .unwrap();
        assert_eq!(
            report["changed_files"],
            json!(["empty.txt", "image with spaces.bin", "mode.txt"])
        );
    }

    #[test]
    fn validation_diff_errors_never_become_empty_successes() {
        let d = git_fixture();
        for reference in ["missing-ref", "--output=/tmp/unwanted", "HEAD..HEAD", ""] {
            assert!(
                DiffSource::GitComparison {
                    base_ref: reference.into(),
                    target: DiffTarget::Head
                }
                .resolve(d.path())
                .is_err()
            );
        }
        assert!(
            DiffSource::PatchFile("missing.patch".into())
                .resolve(d.path())
                .is_err()
        );
        assert!(
            DiffSource::PatchFile("src".into())
                .resolve(d.path())
                .is_err()
        );
        fs::write(d.path().join("invalid.patch"), [0xff]).unwrap();
        assert!(
            DiffSource::PatchFile("invalid.patch".into())
                .resolve(d.path())
                .is_err()
        );
        let no_git = fixture();
        assert!(DiffSource::Pending.resolve(no_git.path()).is_err());
        git(no_git.path(), &["init", "-q"]);
        let unborn = DiffSource::Pending.resolve(no_git.path()).unwrap();
        assert_eq!(unborn.scope["comparison"], "index_to_worktree");
        assert!(unborn.scope["base_commit"].is_null());
        let explicit_empty = DiffSource::Inline(String::new()).resolve(d.path()).unwrap();
        assert_eq!(explicit_empty.scope["bytes"], 0);
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
        let result: Result<()> = service.with_index_build(|service| {
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
                supersedes: vec![],
                recorded_by: String::new(),
            })
            .unwrap();
        let listed = service
            .steering_list(Some("injectable Store"), None, 10)
            .unwrap();
        assert_eq!(listed["steerings"][0]["priority"], "high");
        let context = service
            .prepare_change("change Store", &["Store".into()], 1, Some(500))
            .unwrap();
        assert_eq!(context["steerings"][0]["title"], "Keep storage injectable");
    }

    #[test]
    fn current_scoped_instructions_are_bounded_and_available_before_refresh() {
        let d = fixture();
        fs::create_dir_all(d.path().join("src/nested")).unwrap();
        fs::write(d.path().join("AGENTS.md"), "current root policy\n").unwrap();
        fs::write(d.path().join("src/nested/AGENTS.md"), "x".repeat(90_000)).unwrap();
        fs::create_dir_all(d.path().join("target/generated")).unwrap();
        fs::write(
            d.path().join("target/generated/AGENTS.md"),
            "generated instruction is ignored",
        )
        .unwrap();
        let service = Service::open(d.path()).unwrap();
        let instructions = service.live_instructions().unwrap();
        let root = instructions
            .iter()
            .find(|item| item["path"] == "AGENTS.md")
            .unwrap();
        assert_eq!(root["complete"], true);
        assert_eq!(root["digest_scope"], "file");
        let nested = instructions
            .iter()
            .find(|item| item["path"] == "src/nested/AGENTS.md")
            .unwrap();
        assert_eq!(nested["complete"], false);
        assert_eq!(nested["digest_scope"], "captured_prefix");
        assert_eq!(nested["evidence"].as_str().unwrap().len(), 64_000);
        assert!(
            instructions
                .iter()
                .all(|item| !item["path"].as_str().unwrap_or("").starts_with("target/"))
        );
        fs::write(d.path().join("AGENTS.md"), "edited live policy\n").unwrap();
        assert!(
            service
                .live_instructions()
                .unwrap()
                .iter()
                .any(|item| item["evidence"] == "edited live policy\n")
        );
        assert!(service.active_generation().is_none());
    }

    #[test]
    fn consultation_includes_global_governance_and_policy_documents() {
        let d = fixture();
        fs::write(
            d.path().join("AGENTS.md"),
            "# Interface policy\nUse the ocean theme and run the accessibility workflow for UI designs.\n",
        )
        .unwrap();
        fs::create_dir_all(d.path().join("docs")).unwrap();
        fs::write(
            d.path().join("docs/design.md"),
            "# Design policy\nEvery settings screen uses the shared form layout.\n",
        )
        .unwrap();
        let mut service = Service::open(d.path()).unwrap();
        service.refresh_if_stale().unwrap();
        service
            .record_decision(RecordDecision {
                title: "Respect product accessibility".into(),
                status: "accepted".into(),
                reason: "All features must remain operable without a pointer".into(),
                applies_to: vec![],
                consequences: vec!["Include keyboard interaction in every design".into()],
                supersedes: vec![],
                recorded_by: String::new(),
                materialize: false,
            })
            .unwrap();
        service
            .record_steering(RecordSteering {
                title: "Use established visual language".into(),
                instruction: "Reuse the repository theme and design tokens".into(),
                scope: vec![],
                priority: "high".into(),
                status: "active".into(),
                expires_at: None,
                supersedes: vec![],
                recorded_by: String::new(),
            })
            .unwrap();

        let consultation = service.consult("Design a settings feature", 4_000).unwrap();
        assert_eq!(consultation["consulted"], true);
        assert_eq!(consultation["guidance_found"], true);
        assert!(
            consultation["decisions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|decision| decision["title"] == "Respect product accessibility")
        );
        assert!(
            consultation["steerings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|steering| steering["title"] == "Use established visual language")
        );
        // Instruction files are read live and inlined once, not repeated as
        // indexed governing documents.
        let instructions = consultation["live_instructions"].as_array().unwrap();
        let agents = instructions
            .iter()
            .find(|document| document["path"] == "AGENTS.md")
            .unwrap();
        assert_eq!(agents["inlined"], true);
        assert!(agents["evidence"].as_str().unwrap().contains("ocean theme"));
        let documents = consultation["governing_documents"].as_array().unwrap();
        assert!(
            documents
                .iter()
                .all(|document| document["path"] != "AGENTS.md")
        );
        let design = documents
            .iter()
            .find(|document| document["path"] == "docs/design.md")
            .unwrap();
        assert!(matches!(
            design["relevance"].as_str(),
            Some("topic_match" | "repository_policy")
        ));
        assert!(
            consultation["context_budget"]["estimated_tokens"]
                .as_u64()
                .unwrap()
                <= 4_000
        );
    }

    fn propose_work(service: &Service, title: &str, status: &str, scope: &[&str]) -> String {
        let input: WorkItemInput = serde_json::from_value(json!({
            "title": title,
            "status": status,
            "scope": scope,
            "evidence": ["x".repeat(400)],
            "acceptance_criteria": [format!("{title} is verified")],
        }))
        .unwrap();
        service.work_propose(input).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    fn ids(values: &Value) -> Vec<String> {
        values
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|value| value["id"].as_str().map(str::to_owned))
            .collect()
    }

    #[test]
    fn consultation_reports_only_relevant_open_work_compactly() {
        let d = fixture();
        let mut service = Service::open(d.path()).unwrap();
        service.refresh_if_stale().unwrap();
        let relevant = propose_work(&service, "Consult budget packing", "accepted", &[]);
        let done = propose_work(&service, "Consult budget packing cleanup", "done", &[]);
        let unrelated = propose_work(
            &service,
            "Refresh the logo on the marketing page",
            "proposed",
            &["web/site"],
        );
        let same_path = propose_work(
            &service,
            "Tune consult budget packing",
            "in_progress",
            &["crates/core"],
        );
        let other_path = propose_work(
            &service,
            "Tune consult budget packing elsewhere",
            "accepted",
            &["crates/other/src/budget.rs"],
        );

        let topic = "Please improve the consult budget packing for the settings page in crates/core/src/budget.rs";
        let consultation = service.consult(topic, 4_000).unwrap();
        let work = ids(&consultation["known_work"]);
        assert!(work.contains(&relevant), "{work:?}");
        assert!(work.contains(&same_path), "{work:?}");
        for excluded in [&done, &unrelated, &other_path] {
            assert!(!work.contains(excluded), "{excluded} in {work:?}");
        }
        // A path-related item outranks one that only shares vocabulary.
        assert_eq!(work[0], same_path);
        let brief = &consultation["known_work"][0];
        assert_eq!(brief["detail"], "work.get");
        assert_eq!(brief["status"], "in_progress");
        assert!(brief.get("evidence").is_none());
        assert!(brief["summary"].as_str().unwrap().ends_with("is verified"));

        // work.list keeps full rows and every status, ranked by the same rules.
        let listed = service.work_list(Some(topic), 10).unwrap();
        let listed_ids = ids(&listed["items"]);
        assert!(listed_ids.contains(&done));
        assert!(!listed_ids.contains(&unrelated));
        assert!(!listed_ids.contains(&other_path));
        assert!(listed["items"][0].get("evidence").is_some());
    }

    #[test]
    fn consultation_keeps_ci_configuration_to_ci_topics() {
        let d = fixture();
        fs::create_dir_all(d.path().join(".github/workflows")).unwrap();
        fs::write(
            d.path().join(".github/workflows/ci.yml"),
            "name: CI\njobs:\n  settings:\n    steps:\n      - run: cargo test settings feature\n",
        )
        .unwrap();
        let mut service = Service::open(d.path()).unwrap();
        service.refresh_if_stale().unwrap();
        let ci_paths = |topic: &str| {
            service.consult(topic, 4_000).unwrap()["governing_documents"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|document| document["path"] == ".github/workflows/ci.yml")
                .map(|document| document["relevance"].clone())
                .collect::<Vec<_>>()
        };
        assert!(ci_paths("Design a settings feature").is_empty());
        assert_eq!(
            ci_paths("Fix the CI workflow for the settings tests"),
            [json!("ci_configuration_for_topic")]
        );
    }

    #[test]
    fn consultation_compacts_before_dropping_and_says_how_to_read_the_rest() {
        let d = fixture();
        let mut service = Service::open(d.path()).unwrap();
        service.refresh_if_stale().unwrap();
        for index in 0..4 {
            let mut global = decision(&format!("Global rule {index}"), &[]);
            global.reason = "r".repeat(900);
            service.record_decision(global).unwrap();
        }
        for index in 0..10 {
            service
                .record_steering(RecordSteering {
                    title: format!("Steer {index}"),
                    instruction: "s".repeat(900),
                    scope: vec![],
                    priority: "normal".into(),
                    status: "active".into(),
                    expires_at: None,
                    supersedes: vec![],
                    recorded_by: String::new(),
                })
                .unwrap();
        }
        let work = propose_work(&service, "Settings storage migration", "accepted", &[]);

        let consultation = service
            .consult("Plan the settings storage migration", 3_000)
            .unwrap();
        let budget = &consultation["context_budget"];
        assert!(budget["serialized_bytes"].as_u64().unwrap() <= 12_000);
        assert_eq!(consultation["decisions"].as_array().unwrap().len(), 4);
        assert_eq!(consultation["steerings"].as_array().unwrap().len(), 10);
        assert_eq!(ids(&consultation["known_work"]), [work]);
        assert_eq!(budget["omitted"]["steerings"], 0);
        assert!(budget["compacted"]["steerings"].as_u64().unwrap() > 0);
        assert!(
            budget["fetch_more"]["steerings"]
                .as_str()
                .unwrap()
                .contains("steering.list")
        );
        assert_eq!(budget["truncated"], true);
        // Decisions outrank steerings when upgrading to full detail.
        assert!(
            consultation["decisions"]
                .as_array()
                .unwrap()
                .iter()
                .all(|decision| decision.get("rationale").is_some())
        );
        assert!(consultation.get("snapshot").is_some());
        assert!(consultation.get("next_steps").is_some());
    }

    #[test]
    fn consultation_stubs_instruction_files_outside_the_topic_and_budget() {
        let d = fixture();
        fs::write(d.path().join("AGENTS.md"), "root policy\n").unwrap();
        for directory in ["crates/core", "crates/other"] {
            fs::create_dir_all(d.path().join(directory)).unwrap();
            fs::write(
                d.path().join(directory).join("AGENTS.md"),
                format!("{directory} policy\n"),
            )
            .unwrap();
        }
        fs::write(d.path().join("CONTEXT.md"), "c".repeat(30_000)).unwrap();
        let service = Service::open(d.path()).unwrap();
        let consultation = service
            .consult("Change crates/core/src/lib.rs parsing", 2_000)
            .unwrap();
        let instructions = consultation["live_instructions"].as_array().unwrap();
        let find = |path: &str| {
            instructions
                .iter()
                .find(|document| document["path"] == path)
                .unwrap_or_else(|| panic!("{path} missing from {instructions:?}"))
        };
        assert_eq!(find("AGENTS.md")["inlined"], true);
        assert_eq!(find("crates/core/AGENTS.md")["inlined"], true);
        let other = find("crates/other/AGENTS.md");
        assert_eq!(other["inlined"], false);
        assert!(other.get("evidence").is_none());
        assert!(other["content_digest"].as_str().unwrap().starts_with("b3:"));
        // A root file too large for the budget is still listed, as a stub.
        let context = find("CONTEXT.md");
        assert_eq!(context["inlined"], false);
        assert_eq!(context["bytes"], 30_000);
        assert_eq!(
            consultation["context_budget"]["omitted"]["live_instructions"],
            0
        );
        assert_eq!(
            consultation["context_budget"]["compacted"]["live_instructions"],
            1
        );
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
        assert_eq!(cached[0].provenance, "RustAnalyzerCached");
    }

    fn metadata_count(service: &Service, key: &str) -> usize {
        service
            .metadata_value(key)
            .unwrap()
            .and_then(|value| value.parse().ok())
            .unwrap()
    }

    fn node_ids(service: &Service) -> BTreeMap<String, i64> {
        service
            .all_nodes()
            .unwrap()
            .into_iter()
            .map(|node| (node.canonical_name, node.id))
            .collect()
    }

    /// Syntax edges by endpoint names, comparable across node-id assignments.
    fn named_edges(service: &Service) -> BTreeSet<(String, String, String)> {
        service
            .db
            .prepare(
                "SELECT s.canonical_name,d.canonical_name,e.kind FROM edges e \
                 JOIN nodes s ON s.id=e.src JOIN nodes d ON d.id=e.dst WHERE e.provenance='Syntax'",
            )
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    /// Syntax edges with their evidence metadata, by endpoint names.
    fn edge_records(service: &Service) -> BTreeSet<(String, String, String, String)> {
        service
            .db
            .prepare(
                "SELECT s.canonical_name,d.canonical_name,e.kind,e.metadata FROM edges e \
                 JOIN nodes s ON s.id=e.src JOIN nodes d ON d.id=e.dst WHERE e.provenance='Syntax'",
            )
            .unwrap()
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    fn unresolved_names(service: &Service) -> BTreeSet<(String, String)> {
        service
            .db
            .prepare("SELECT file,name FROM unresolved_references")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    #[test]
    fn a_full_rebuild_reuses_every_unchanged_symbol_vector() {
        let d = fixture();
        let mut service = Service::open(d.path()).unwrap();
        service.refresh(Some("workspace")).unwrap();
        let nodes = service.all_nodes().unwrap().len();
        assert!(nodes > 0);
        assert_eq!(metadata_count(&service, "embedding_recomputed"), nodes);
        service.refresh(Some("full")).unwrap();
        // Node ids are reassigned by a full rebuild, yet every card is unchanged.
        let status = service.embedding_index_status();
        assert_eq!(status["last_build"]["reused"], nodes);
        assert_eq!(status["last_build"]["recomputed"], 0);
        assert_eq!(status["vectors"], nodes as i64);
    }

    #[test]
    fn an_edit_reindexes_only_affected_symbols_and_matches_a_full_rebuild() {
        let d = fixture();
        fs::write(
            d.path().join("src/lib.rs"),
            "pub mod helpers;\npub mod callers;\npub mod unrelated;\n",
        )
        .unwrap();
        fs::write(
            d.path().join("src/helpers.rs"),
            "pub fn helper() -> u8 { 1 }\npub fn untouched_helper() -> u8 { 2 }\n",
        )
        .unwrap();
        fs::write(
            d.path().join("src/callers.rs"),
            "pub fn caller() -> u8 { helper() }\n",
        )
        .unwrap();
        let mut unrelated = String::new();
        for index in 0..40 {
            unrelated.push_str(&format!(
                "pub fn standalone_{index}() -> u8 {{ {index} }}\n"
            ));
        }
        fs::write(d.path().join("src/unrelated.rs"), unrelated).unwrap();
        let mut service = Service::open(d.path()).unwrap();
        service.refresh(None).unwrap();
        let before = node_ids(&service);
        let total = before.len();
        assert!(named_edges(&service).contains(&(
            "src::callers::caller".into(),
            "src::helpers::helper".into(),
            "CALLS_DIRECT".into()
        )));

        // A body edit plus a new symbol: only the edited file is re-parsed.
        fs::write(
            d.path().join("src/helpers.rs"),
            "pub fn helper() -> u8 { 3 }\npub fn untouched_helper() -> u8 { 2 }\npub fn freshly_added_helper() -> u8 { 4 }\n",
        )
        .unwrap();
        service.refresh(None).unwrap();
        let after = node_ids(&service);
        for (name, id) in &before {
            assert_eq!(after.get(name), Some(id), "{name} kept its node id");
        }
        assert_eq!(after.len(), total + 1);
        let cards = metadata_count(&service, "embedding_cards_built");
        assert!(
            cards < total / 2,
            "built {cards} cards for a one-file edit of {total} symbols"
        );
        assert!(
            !service
                .search_nodes(&terms("freshly_added_helper"), 5)
                .unwrap()
                .is_empty()
        );
        assert!(named_edges(&service).contains(&(
            "src::callers::caller".into(),
            "src::helpers::helper".into(),
            "CALLS_DIRECT".into()
        )));

        // Removing the callee re-resolves the unchanged caller's references.
        fs::write(
            d.path().join("src/helpers.rs"),
            "pub fn untouched_helper() -> u8 { 2 }\npub fn freshly_added_helper() -> u8 { 4 }\n",
        )
        .unwrap();
        service.refresh(None).unwrap();
        assert!(
            !named_edges(&service)
                .iter()
                .any(|(_, target, _)| target == "src::helpers::helper")
        );
        assert!(
            service
                .search_nodes(&terms("helper"), 10)
                .unwrap()
                .iter()
                .all(|node| node.canonical_name != "src::helpers::helper")
        );

        // A second definition makes the name ambiguous for the unchanged caller.
        fs::write(
            d.path().join("src/helpers.rs"),
            "pub fn helper() -> u8 { 1 }\npub fn untouched_helper() -> u8 { 2 }\n",
        )
        .unwrap();
        fs::write(
            d.path().join("src/unrelated.rs"),
            "pub fn helper() -> u8 { 9 }\n",
        )
        .unwrap();
        service.refresh(None).unwrap();
        let incremental = (edge_records(&service), unresolved_names(&service));
        assert!(
            incremental
                .1
                .contains(&("src/callers.rs".into(), "helper".into()))
        );
        let incremental_count = service.all_nodes().unwrap().len();
        service.refresh(Some("workspace")).unwrap();
        assert_eq!(
            incremental,
            (edge_records(&service), unresolved_names(&service))
        );
        assert_eq!(service.all_nodes().unwrap().len(), incremental_count);
    }

    #[test]
    fn an_implementation_edge_from_two_colliding_files_survives_editing_either() {
        let d = fixture();
        fs::write(
            d.path().join("src/lib.rs"),
            "pub trait Backend { fn load(&self); }\npub trait Store { fn load(&self); }\npub mod one;\npub mod two;\n",
        )
        .unwrap();
        // Both files define a `Disk`; both impl headers resolve, by short
        // name, to the same (Disk, Backend) edge.
        let colliding = "pub struct Disk;\nimpl Backend for Disk { fn load(&self) {} }\n";
        fs::write(d.path().join("src/one.rs"), colliding).unwrap();
        fs::write(d.path().join("src/two.rs"), colliding).unwrap();
        let mut service = Service::open(d.path()).unwrap();
        service.refresh(None).unwrap();
        let implements = |service: &Service| {
            edge_records(service)
                .into_iter()
                .filter(|(_, _, kind, _)| kind == "IMPLEMENTS")
                .map(|(source, target, _, metadata)| (source, target, metadata))
                .collect::<Vec<_>>()
        };
        let initial = implements(&service);
        assert_eq!(initial.len(), 1, "{initial:?}");
        assert_eq!(
            edge_contributors(&initial[0].2),
            BTreeSet::from(["src/one.rs".to_owned(), "src/two.rs".to_owned()])
        );
        // Retarget the impl in the file recorded first without changing any
        // symbol name (so no other file is re-scanned), edit the other file,
        // restore, and finally drop one impl. After each incremental refresh
        // the graph equals a fresh full rebuild.
        for (file, text, edges) in [
            (
                "src/one.rs",
                "pub struct Disk;\nimpl Store for Disk { fn load(&self) {} }\n",
                2,
            ),
            (
                "src/two.rs",
                "pub struct Disk;\nimpl Backend for Disk { fn load(&self) { let _ = 1; } }\n",
                2,
            ),
            ("src/one.rs", colliding, 1),
            ("src/two.rs", "pub struct Disk;\n", 1),
        ] {
            fs::write(d.path().join(file), text).unwrap();
            service.refresh(None).unwrap();
            assert_eq!(implements(&service).len(), edges, "after editing {file}");
            let incremental = (edge_records(&service), unresolved_names(&service));
            service.refresh(Some("workspace")).unwrap();
            assert_eq!(
                incremental,
                (edge_records(&service), unresolved_names(&service)),
                "after editing {file}"
            );
        }
    }

    #[test]
    fn committing_an_indexed_edit_does_not_reprocess_it() {
        let d = git_fixture();
        let mut service = Service::open(d.path()).unwrap();
        service.refresh(None).unwrap();
        fs::write(
            d.path().join("src/lib.rs"),
            "pub trait Store { fn load(&self); }\nfn uses(s: &dyn Store) { s.load(); }\npub fn committed_later() {}\n",
        )
        .unwrap();
        fs::write(d.path().join("NOTES.md"), "untracked notes\n").unwrap();
        service.refresh(None).unwrap();
        let input_hash = |service: &Service, path: &str| -> String {
            service
                .db
                .query_row(
                    "SELECT content_hash FROM input_state WHERE path=?1",
                    [path],
                    |row| row.get(0),
                )
                .unwrap()
        };
        let dirty_hash = input_hash(&service, "src/lib.rs");
        let untracked_hash = input_hash(&service, "NOTES.md");
        let ids = node_ids(&service);
        git(d.path(), &["add", "src/lib.rs", "NOTES.md"]);
        git(d.path(), &["commit", "-q", "-m", "commit the indexed edit"]);
        let generation = service.active_generation();
        service.refresh(None).unwrap();
        assert_eq!(input_hash(&service, "src/lib.rs"), dirty_hash);
        assert_eq!(input_hash(&service, "NOTES.md"), untracked_hash);
        assert_eq!(node_ids(&service), ids);
        assert_eq!(metadata_count(&service, "embedding_cards_built"), 0);
        // The new head is published, so the snapshot is current again.
        assert_ne!(service.active_generation(), generation);
        assert_eq!(service.status().unwrap()["stale"], false);
        assert_eq!(
            service.metadata_value("git_indexed_head").unwrap(),
            command_text(d.path(), &["rev-parse", "HEAD"])
        );
    }

    #[test]
    fn an_unchanged_workspace_publishes_nothing_by_default() {
        let d = fixture();
        let mut service = Service::open(d.path()).unwrap();
        service.refresh(None).unwrap();
        let generation = service.active_generation();
        let ids = node_ids(&service);
        service.refresh(None).unwrap();
        assert_eq!(service.active_generation(), generation);
        assert_eq!(node_ids(&service), ids);
    }

    #[test]
    fn a_git_only_refresh_leaves_the_published_generation_stale() {
        let d = git_fixture();
        let mut service = Service::open(d.path()).unwrap();
        service.refresh(None).unwrap();
        let published_head = service.metadata_value("git_indexed_head").unwrap();
        fs::write(d.path().join("src/lib.rs"), "pub fn after_commit() {}\n").unwrap();
        git(d.path(), &["commit", "-q", "-am", "change sources"]);
        let generation = service.active_generation();
        service.refresh(Some("git")).unwrap();
        assert_eq!(service.active_generation(), generation);
        assert_eq!(
            service.metadata_value("git_indexed_head").unwrap(),
            published_head
        );
        assert_eq!(
            service.metadata_value("git_history_head").unwrap(),
            command_text(d.path(), &["rev-parse", "HEAD"])
        );
        assert_eq!(service.status().unwrap()["stale"], true);
        service.refresh(None).unwrap();
        assert_eq!(service.status().unwrap()["stale"], false);
        assert!(
            !service
                .search_nodes(&terms("after_commit"), 1)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn semantic_profile_and_cargo_changes_refresh_without_a_rebuild() {
        let d = fixture();
        let mut service = Service::open(d.path()).unwrap();
        service.refresh(None).unwrap();
        let ids = node_ids(&service);
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
        // Compiler evidence belongs to the profile it was recorded under.
        service
            .db
            .execute(
                "UPDATE semantic_snapshots SET target_triple='another-target' WHERE id=?1",
                [&snapshot],
            )
            .unwrap();
        service.refresh(None).unwrap();
        assert_eq!(node_ids(&service), ids);
        assert_eq!(service.status().unwrap()["counts"]["semantic_edges"], 0);

        fs::write(
            d.path().join("Cargo.toml"),
            "[package]\nname='demo'\nversion='0.1.0'\nedition='2024'\n[features]\nextra=[]\n",
        )
        .unwrap();
        service.refresh(None).unwrap();
        assert_eq!(node_ids(&service), ids);
        assert!(
            service.matrix().unwrap()["features"]
                .as_array()
                .unwrap()
                .iter()
                .any(|feature| feature["feature"] == "extra")
        );
    }

    #[test]
    fn index_build_allows_memory_writes_and_preserves_them_on_publish() {
        let d = fixture();
        let mut publisher = Service::open(d.path()).unwrap();
        publisher.refresh(None).unwrap();
        let generation = publisher.active_generation();
        publisher
            .with_index_build(|building| {
                building.reindex_inner()?;
                let memory = Service::open(d.path())?;
                memory.db.busy_timeout(Duration::from_millis(50))?;
                assert_eq!(memory.active_generation(), generation);
                memory.record_decision(serde_json::from_value(json!({
                    "title": "Recorded during indexing", "applies_to": ["Store"]
                }))?)?;
                Ok(())
            })
            .unwrap();
        assert!(publisher.active_generation() > generation);
        assert_eq!(
            publisher.decision_resource("DEC-0001").unwrap()["title"],
            "Recorded during indexing"
        );
        assert_eq!(
            publisher
                .search_hits(&terms("Recorded during indexing"), Some("decision"), 10)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn failed_memory_search_update_rolls_back_steering_and_problem() {
        let d = fixture();
        let service = Service::open(d.path()).unwrap();
        service.db.execute_batch("DROP TABLE search_index").unwrap();
        let steering = serde_json::from_value(
            json!({"title":"Atomic writes", "instruction":"Keep writes atomic"}),
        )
        .unwrap();
        assert!(service.record_steering(steering).is_err());
        let problem =
            serde_json::from_value(json!({"report":"Database writes fail during indexing"}))
                .unwrap();
        assert!(service.problem_record(problem).is_err());
        for table in [
            "steerings",
            "problem_records",
            "problem_occurrences",
            "quality_constraints",
        ] {
            let count: i64 = service
                .db
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
                .unwrap();
            assert_eq!(count, 0, "failed command left records in {table}");
        }
    }

    #[test]
    fn failed_optional_export_reports_the_committed_decision() {
        let d = fixture();
        fs::write(d.path().join("docs"), "not a directory").unwrap();
        let service = Service::open(d.path()).unwrap();
        let decision = service
            .record_decision(
                serde_json::from_value(json!({
                    "title":"Export failure", "materialize":true
                }))
                .unwrap(),
            )
            .unwrap();
        assert_eq!(decision["id"], "DEC-0001");
        assert_eq!(decision["committed"], true);
        assert_eq!(decision["materialization"]["status"], "failed");
        assert_eq!(decision["materialization"]["retry_record_creation"], false);
        assert_eq!(
            service.decision_resource("DEC-0001").unwrap()["title"],
            "Export failure"
        );
    }

    #[test]
    fn publication_failure_retains_generation_and_concurrent_memory() {
        let d = fixture();
        let mut publisher = Service::open(d.path()).unwrap();
        publisher.refresh(None).unwrap();
        let generation = publisher.active_generation();
        let result = publisher.with_index_build(|building| {
            building.reindex_inner()?;
            let memory = Service::open(d.path())?;
            memory.record_decision(serde_json::from_value(json!({"title":"Survives failed publication"}))?)?;
            memory.db.execute_batch("CREATE TRIGGER fail_publish BEFORE INSERT ON metadata WHEN NEW.key='active_generation' BEGIN SELECT RAISE(ABORT, 'injected publication failure'); END;")?;
            Ok(())
        });
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("injected publication failure")
        );
        assert_eq!(publisher.active_generation(), generation);
        assert_eq!(
            publisher.decision_resource("DEC-0001").unwrap()["title"],
            "Survives failed publication"
        );
        assert!(publisher.db.is_autocommit());
        assert!(
            !fs::read_dir(d.path().join(INDEX_DIRECTORY))
                .unwrap()
                .any(|entry| entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("index-build-"))
        );
    }

    #[test]
    fn validation_explains_stale_and_changing_evidence() {
        let d = fixture();
        let mut service = Service::open(d.path()).unwrap();
        service.refresh(None).unwrap();
        let source = DiffSource::Inline("diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-old\n+new\n".into());
        fs::write(d.path().join("src/lib.rs"), "pub fn edited() {}\n").unwrap();
        let report = service
            .validate_change_from_source(None, &source, false)
            .unwrap();
        assert_eq!(report["review_evidence"]["status"], "incomplete");
        assert_eq!(
            report["review_evidence"]["index"]["reason"],
            "worktree_changed"
        );
        assert_eq!(
            report["review_evidence"]["semantic_correctness"],
            "not_established"
        );
        assert_eq!(report["validation_status"]["verdict"], "not_run");
        let path = d.path().join("src/lib.rs");
        service.execution = execution::ExecutionControl::new(Duration::from_secs(30), move || {
            fs::write(&path, "pub fn changed_during_review() {}\n")?;
            Ok(false)
        });
        let report = service
            .validate_change_from_source(None, &source, false)
            .unwrap();
        assert_eq!(report["review_evidence"]["status"], "changed_during_review");
        assert_eq!(report["review_evidence"]["source_unchanged"], false);
    }

    #[test]
    fn concurrent_decision_writers_allocate_distinct_records() {
        let d = fixture();
        drop(Service::open(d.path()).unwrap());
        let barrier = std::sync::Barrier::new(4);
        let ids = std::thread::scope(|scope| {
            let handles = (0..4)
                .map(|index| {
                    let barrier = &barrier;
                    let path = d.path();
                    scope.spawn(move || {
                        let service = Service::open(path).unwrap();
                        barrier.wait();
                        service
                            .record_decision(
                                serde_json::from_value(json!({"title":format!("Writer {index}")}))
                                    .unwrap(),
                            )
                            .unwrap()["id"]
                            .as_str()
                            .unwrap()
                            .to_owned()
                    })
                })
                .collect::<Vec<_>>();
            handles
                .into_iter()
                .map(|thread| thread.join().unwrap())
                .collect::<BTreeSet<_>>()
        });
        assert_eq!(ids.len(), 4);
        assert!(ids.contains("DEC-0004"));
    }

    #[test]
    fn failed_decision_write_leaves_no_record_for_retry() {
        let d = fixture();
        let service = Service::open(d.path()).unwrap();
        service.db.execute_batch("CREATE TRIGGER fail_decision_target BEFORE INSERT ON decision_targets BEGIN SELECT RAISE(ABORT, 'injected target failure'); END;").unwrap();
        let request = || {
            serde_json::from_value::<RecordDecision>(json!({
                "title": "Retry safely", "applies_to": ["src/lib.rs"]
            }))
            .unwrap()
        };
        let error = service.record_decision(request()).unwrap_err();
        assert!(error.to_string().contains("injected target failure"));
        let count: i64 = service
            .db
            .query_row("SELECT COUNT(*) FROM decisions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            count, 0,
            "a failed operation must not leave a committed decision"
        );
        service
            .db
            .execute_batch("DROP TRIGGER fail_decision_target")
            .unwrap();
        assert_eq!(
            service.record_decision(request()).unwrap()["id"],
            "DEC-0001"
        );
    }

    #[test]
    fn reads_during_a_refresh_see_the_previous_generation_without_waiting() {
        let d = fixture();
        let mut writer = Service::open(d.path()).unwrap();
        writer.refresh(None).unwrap();
        let generation = writer.active_generation();
        let nodes = writer.all_nodes().unwrap().len();
        // Hold the write transaction a refresh holds while it rebuilds.
        writer
            .db
            .execute_batch("SAVEPOINT long_refresh; DELETE FROM nodes; DELETE FROM metadata WHERE key='active_generation';")
            .unwrap();
        let started = Instant::now();
        let reader = Service::open(d.path()).unwrap();
        assert_eq!(reader.active_generation(), generation);
        assert_eq!(reader.all_nodes().unwrap().len(), nodes);
        assert_eq!(reader.status().unwrap()["counts"]["nodes"], nodes as i64);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "read waited {:?} behind the writer",
            started.elapsed()
        );
        writer
            .db
            .execute_batch("ROLLBACK TO long_refresh; RELEASE long_refresh;")
            .unwrap();
    }

    #[test]
    fn schema_migration_runs_only_for_stores_without_the_current_stamp() {
        let d = fixture();
        drop(Service::open(d.path()).unwrap());
        let path = d.path().join(INDEX_DIRECTORY).join("index.sqlite3");
        let writer = Connection::open(&path).unwrap();
        writer.execute_batch("BEGIN IMMEDIATE;").unwrap();
        // Another process's connection opening a current store never writes,
        // so it does not contend for the held write lock.
        let reader = Connection::open(&path).unwrap();
        reader.busy_timeout(Duration::from_millis(50)).unwrap();
        initialize_schema(&reader).unwrap();
        writer.execute_batch("ROLLBACK;").unwrap();
        // A store without the stamp (written before it existed) is migrated.
        writer
            .execute("DELETE FROM metadata WHERE key='schema_fingerprint'", [])
            .unwrap();
        initialize_schema(&reader).unwrap();
        let stamp: String = reader
            .query_row(
                "SELECT value FROM metadata WHERE key='schema_fingerprint'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stamp, schema_fingerprint());
    }

    #[test]
    fn state_and_target_directories_are_never_inputs() {
        let d = git_fixture();
        fs::create_dir_all(d.path().join(".rust-repo-intelligence")).unwrap();
        fs::write(d.path().join(".rust-repo-intelligence/state.json"), "{}").unwrap();
        fs::create_dir_all(d.path().join("target/debug")).unwrap();
        fs::write(d.path().join("target/debug/build.json"), "{}").unwrap();
        fs::create_dir_all(d.path().join("src/target")).unwrap();
        fs::write(d.path().join("src/target/mod.rs"), "pub fn kept() {}\n").unwrap();
        let (inputs, worktree) = index_inputs_with_worktree(d.path());
        assert!(inputs.contains_key("src/target/mod.rs"));
        assert!(
            !inputs
                .keys()
                .any(|path| path.starts_with(".rust-repo-intelligence"))
        );
        assert!(!inputs.keys().any(|path| path.starts_with("target/")));
        assert_eq!(
            worktree,
            BTreeSet::from(["src/target/mod.rs".to_owned()]),
            "only input paths are recorded as dirty"
        );
    }

    #[test]
    fn dirty_paths_are_relative_to_a_subdirectory_root() {
        let d = git_fixture();
        fs::write(d.path().join("src/lib.rs"), "pub fn edited() {}\n").unwrap();
        fs::write(d.path().join("Cargo.toml"), "[package]\nname='edited'\n").unwrap();
        assert_eq!(
            git_dirty_paths(&d.path().join("src")),
            Some(BTreeSet::from(["lib.rs".to_owned()]))
        );
        let outside = tempdir().unwrap();
        assert_eq!(git_dirty_paths(outside.path()), None);
    }

    #[test]
    fn status_staleness_agrees_with_the_freshness_comparison() {
        let d = git_fixture();
        let mut service = Service::open(d.path()).unwrap();
        assert_eq!(
            service.status().unwrap()["freshness_reason"],
            "never_published"
        );
        service.refresh(None).unwrap();
        assert_eq!(service.status().unwrap()["freshness_reason"], "ok");
        fs::write(d.path().join("src/lib.rs"), "pub fn edited() {}\n").unwrap();
        let status = service.status().unwrap();
        assert_eq!(status["stale"], true);
        assert_eq!(status["freshness_reason"], "worktree_changed");
        let summary = service.refresh(None).unwrap();
        assert_eq!(summary["mode"], "incremental");
        assert_eq!(summary["changed_inputs"], 1);
        assert_eq!(service.status().unwrap()["stale"], false);
        let unchanged = service.refresh(None).unwrap();
        assert_eq!(unchanged["mode"], "unchanged");
        assert_eq!(unchanged["published"], false);
        assert_eq!(unchanged["generation"], summary["generation"]);
        assert!(unchanged["embeddings"].is_null());
        service
            .db
            .execute(
                "UPDATE metadata SET value='8' WHERE key='indexer_version'",
                [],
            )
            .unwrap();
        assert_eq!(
            service.status().unwrap()["freshness_reason"],
            "indexer_outdated"
        );
        assert_eq!(service.refresh(None).unwrap()["mode"], "full");
        assert_eq!(service.status().unwrap()["freshness_reason"], "ok");
        // A store written before the dirty-input set was recorded falls back
        // to a full comparison and records the set on its next refresh.
        service
            .db
            .execute(
                "DELETE FROM metadata WHERE key='indexed_worktree_inputs'",
                [],
            )
            .unwrap();
        assert_eq!(service.status().unwrap()["stale"], false);
        service.refresh(None).unwrap();
        assert_eq!(
            service.stored_worktree_inputs().unwrap(),
            Some(BTreeSet::from([
                "Cargo.lock".to_owned(),
                "src/lib.rs".to_owned()
            ]))
        );
    }

    #[test]
    fn identical_content_has_one_identity_whether_tracked_or_not() {
        let d = git_fixture();
        let committed = index_inputs(d.path());
        assert!(committed["src/lib.rs"].1.starts_with("git:"));
        let original = fs::read_to_string(d.path().join("src/lib.rs")).unwrap();
        fs::write(d.path().join("src/copy.rs"), &original).unwrap();
        fs::write(d.path().join("src/lib.rs"), "pub fn dirty() {}\n").unwrap();
        fs::write(d.path().join("src/lib.rs"), &original).unwrap();
        let inputs = index_inputs(d.path());
        assert_eq!(inputs["src/copy.rs"].1, committed["src/lib.rs"].1);
        assert_eq!(inputs["src/lib.rs"], committed["src/lib.rs"]);
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
                supersedes: vec![],
                recorded_by: String::new(),
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

    fn decision(title: &str, applies_to: &[&str]) -> RecordDecision {
        RecordDecision {
            title: title.into(),
            status: "accepted".into(),
            reason: format!("{title} rationale"),
            applies_to: applies_to
                .iter()
                .map(|target| (*target).to_owned())
                .collect(),
            consequences: vec![],
            supersedes: vec![],
            recorded_by: String::new(),
            materialize: false,
        }
    }

    fn decision_ids(values: &Value) -> Vec<String> {
        values
            .as_array()
            .unwrap()
            .iter()
            .map(|decision| decision["id"].as_str().unwrap().to_owned())
            .collect()
    }

    #[test]
    fn supersedes_accepts_a_single_id_or_a_list() {
        let single: RecordDecision =
            serde_json::from_value(json!({"title": "t", "supersedes": "DEC-0001"})).unwrap();
        assert_eq!(single.supersedes, ["DEC-0001"]);
        let many: RecordDecision =
            serde_json::from_value(json!({"title": "t", "supersedes": ["DEC-0001", "DEC-0002"]}))
                .unwrap();
        assert_eq!(many.supersedes, ["DEC-0001", "DEC-0002"]);
        let none: RecordDecision =
            serde_json::from_value(json!({"title": "t", "supersedes": null})).unwrap();
        assert!(none.supersedes.is_empty());
        let absent: RecordDecision = serde_json::from_value(json!({"title": "t"})).unwrap();
        assert!(absent.supersedes.is_empty());
        assert_eq!(absent.status, "accepted");
    }

    #[test]
    fn unscoped_decision_list_enumerates_the_ledger_newest_first() {
        let d = fixture();
        let mut service = Service::open(d.path()).unwrap();
        service.refresh_if_stale().unwrap();
        for title in ["First", "Second", "Third"] {
            service
                .record_decision(decision(title, &["Store"]))
                .unwrap();
        }
        let all = service.decision_list("", None, 200).unwrap();
        assert_eq!(
            decision_ids(&json!(all)),
            ["DEC-0003", "DEC-0002", "DEC-0001"]
        );
        assert_eq!(service.decision_list("", None, 2).unwrap().len(), 2);
        assert!(
            service
                .decision_list("", Some("superseded"), 200)
                .unwrap()
                .is_empty()
        );
        assert_eq!(service.decision_list("Store", None, 1).unwrap().len(), 1);
        assert_eq!(service.decision_list("Store", None, 200).unwrap().len(), 3);
    }

    #[test]
    fn superseding_closes_targets_and_exposes_back_links_and_history() {
        let d = fixture();
        let mut service = Service::open(d.path()).unwrap();
        service.refresh_if_stale().unwrap();
        service
            .record_decision(decision("Inject the store", &["Store"]))
            .unwrap();
        service
            .record_decision(decision("Load lazily", &["Store"]))
            .unwrap();

        let mut missing_actor = decision("Inject a lazily loading store", &["Store"]);
        missing_actor.supersedes = vec!["DEC-0001".into()];
        let error = service
            .record_decision(missing_actor)
            .unwrap_err()
            .to_string();
        assert!(error.contains("recorded_by is required"), "{error}");

        let mut unknown = decision("Unknown target", &[]);
        unknown.supersedes = vec!["DEC-9999".into()];
        unknown.recorded_by = "andreas".into();
        let error = service.record_decision(unknown).unwrap_err().to_string();
        assert!(error.contains("unknown decision `DEC-9999`"), "{error}");

        let mut not_accepted = decision("Historical note", &[]);
        not_accepted.status = "retired".into();
        not_accepted.supersedes = vec!["DEC-0001".into()];
        not_accepted.recorded_by = "andreas".into();
        let error = service
            .record_decision(not_accepted)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("only an accepted decision can supersede"),
            "{error}"
        );

        let mut consolidated = decision("Inject a lazily loading store", &["Store"]);
        consolidated.supersedes = vec!["DEC-0001".into(), " DEC-0002 ".into(), "DEC-0001".into()];
        consolidated.recorded_by = "andreas".into();
        let recorded = service.record_decision(consolidated).unwrap();
        assert_eq!(recorded["id"], "DEC-0003");
        assert_eq!(recorded["supersedes"], json!(["DEC-0001", "DEC-0002"]));

        let first = service.decision_resource("DEC-0001").unwrap();
        assert_eq!(first["status"], "superseded");
        assert_eq!(first["superseded_by"], json!(["DEC-0003"]));
        assert_eq!(first["history"][0]["action"], "superseded");
        assert_eq!(first["history"][0]["actor"], "andreas");
        assert_eq!(first["history"][0]["note"], "superseded by DEC-0003");
        let third = service.decision_resource("DEC-0003").unwrap();
        assert_eq!(third["status"], "accepted");
        assert_eq!(third["supersedes"], json!(["DEC-0001", "DEC-0002"]));
        assert_eq!(third["superseded_by"], json!([]));
        assert_eq!(third["history"], json!([]));

        let mut again = decision("Try again", &[]);
        again.supersedes = vec!["DEC-0001".into()];
        again.recorded_by = "andreas".into();
        let error = service.record_decision(again).unwrap_err().to_string();
        assert!(error.contains("status `superseded`"), "{error}");
        assert_eq!(service.decision_list("", None, 200).unwrap().len(), 3);
        assert_eq!(
            decision_ids(&json!(
                service.decision_list("", Some("superseded"), 200).unwrap()
            )),
            ["DEC-0002", "DEC-0001"]
        );
    }

    #[test]
    fn retiring_requires_an_actor_and_is_terminal() {
        let d = fixture();
        let mut service = Service::open(d.path()).unwrap();
        service.refresh_if_stale().unwrap();
        service
            .record_decision(decision("Inject the store", &["Store"]))
            .unwrap();
        let retire = |id: &str, retired_by: &str, note: &str| RetireDecision {
            id: id.into(),
            retired_by: retired_by.into(),
            note: note.into(),
        };
        let error = service
            .retire_decision(retire("DEC-0001", "  ", ""))
            .unwrap_err()
            .to_string();
        assert!(error.contains("retired_by is required"), "{error}");
        let error = service
            .retire_decision(retire("DEC-0042", "andreas", ""))
            .unwrap_err()
            .to_string();
        assert!(error.contains("unknown decision `DEC-0042`"), "{error}");

        let retired = service
            .retire_decision(retire("DEC-0001", "andreas", "No longer applies"))
            .unwrap();
        assert_eq!(retired["status"], "retired");
        assert_eq!(retired["history"][0]["action"], "retired");
        assert_eq!(retired["history"][0]["actor"], "andreas");
        assert_eq!(retired["history"][0]["note"], "No longer applies");
        assert_eq!(retired["materialized_path"], Value::Null);

        let error = service
            .retire_decision(retire("DEC-0001", "andreas", ""))
            .unwrap_err()
            .to_string();
        assert!(error.contains("status `retired`"), "{error}");
        let mut replacement = decision("Replacement", &[]);
        replacement.supersedes = vec!["DEC-0001".into()];
        replacement.recorded_by = "andreas".into();
        let error = service
            .record_decision(replacement)
            .unwrap_err()
            .to_string();
        assert!(error.contains("status `retired`"), "{error}");
        assert!(service.decisions_for("Store").unwrap().is_empty());
    }

    #[test]
    fn unknown_decision_and_steering_statuses_are_rejected() {
        let d = fixture();
        let service = Service::open(d.path()).unwrap();
        let mut invalid = decision("Typo", &[]);
        invalid.status = "acepted".into();
        let error = service.record_decision(invalid).unwrap_err().to_string();
        assert!(
            error.contains("unsupported decision status `acepted`"),
            "{error}"
        );
        let steering = |status: &str, expires_at: Option<&str>| RecordSteering {
            title: "Theme".into(),
            instruction: "Reuse the theme".into(),
            scope: vec![],
            priority: "normal".into(),
            status: status.into(),
            expires_at: expires_at.map(str::to_owned),
            supersedes: vec![],
            recorded_by: String::new(),
        };
        let error = service
            .record_steering(steering("enabled", None))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("unsupported steering status `enabled`"),
            "{error}"
        );
        let error = service
            .record_steering(steering("active", Some("tomorrow")))
            .unwrap_err()
            .to_string();
        assert!(error.contains("not an RFC 3339 timestamp"), "{error}");
        service
            .record_steering(steering("active", Some("2030-01-01T00:00:00Z")))
            .unwrap();
        assert!(service.decision_list("", None, 200).unwrap().is_empty());
        assert_eq!(
            service.steering_list(None, None, 200).unwrap()["steerings"]
                .as_array()
                .map(Vec::len),
            Some(1)
        );
    }

    #[test]
    fn governing_consumers_only_cite_accepted_decisions() {
        let d = fixture();
        let mut service = Service::open(d.path()).unwrap();
        service.refresh_if_stale().unwrap();
        service
            .record_decision(decision("Inject the store", &["Store"]))
            .unwrap();
        let mut replacement = decision("Inject the store through a builder", &["Store"]);
        replacement.supersedes = vec!["DEC-0001".into()];
        replacement.recorded_by = "andreas".into();
        service.record_decision(replacement).unwrap();

        assert_eq!(
            decision_ids(&service.explain("Store", 10).unwrap()["decisions"]),
            ["DEC-0002"]
        );
        assert_eq!(
            decision_ids(&service.why("Store").unwrap()["decisions"]),
            ["DEC-0002"]
        );
        assert_eq!(
            decision_ids(&service.context_pack("Store", 4_000, 10).unwrap()["decisions"]),
            ["DEC-0002"]
        );
        assert_eq!(
            decision_ids(&service.consult("change Store", 4_000).unwrap()["decisions"]),
            ["DEC-0002"]
        );
        assert_eq!(
            decision_ids(&service.constraints("Store").unwrap()["decisions"]),
            ["DEC-0002"]
        );
        assert_eq!(
            decision_ids(&json!(
                service
                    .decision_list("Store", Some("superseded"), 20)
                    .unwrap()
            )),
            ["DEC-0001"]
        );
    }

    #[test]
    fn consultation_reports_what_the_budget_left_out() {
        let d = fixture();
        let mut service = Service::open(d.path()).unwrap();
        service.refresh_if_stale().unwrap();
        for index in 0..6 {
            let mut global = decision(&format!("Global rule {index}"), &[]);
            global.reason = "x".repeat(600);
            service.record_decision(global).unwrap();
        }
        let tight = service.consult("anything at all", 250).unwrap();
        let budget = &tight["context_budget"];
        assert_eq!(budget["truncated"], true);
        assert!(budget["serialized_bytes"].as_u64().unwrap() <= 1_000);
        // Decisions that do not fit in full are compacted before any is dropped.
        let reduced = budget["omitted"]["decisions"].as_u64().unwrap()
            + budget["compacted"]["decisions"].as_u64().unwrap_or(0);
        assert!(reduced >= 1, "{budget}");
        assert!(
            budget["fetch_more"]["decisions"]
                .as_str()
                .unwrap()
                .contains("decision.list")
        );
        let generous = service.consult("anything at all", 20_000).unwrap();
        assert_eq!(generous["context_budget"]["truncated"], false);
        assert_eq!(generous["context_budget"]["omitted"]["decisions"], 0);
        assert_eq!(generous["decisions"].as_array().unwrap().len(), 6);
    }

    #[test]
    fn bare_legacy_supersedes_values_are_normalized_on_open() {
        let d = fixture();
        {
            let service = Service::open(d.path()).unwrap();
            service.record_decision(decision("Old", &[])).unwrap();
            service.record_decision(decision("New", &[])).unwrap();
            // Databases written before multi-target supersession stored one
            // bare ID and only flipped the old status.
            service
                .db
                .execute(
                    "UPDATE decisions SET supersedes='DEC-0001' WHERE id='DEC-0002'",
                    [],
                )
                .unwrap();
            service
                .db
                .execute(
                    "UPDATE decisions SET status='superseded' WHERE id='DEC-0001'",
                    [],
                )
                .unwrap();
            // Stores written before the migration stamp carry none.
            service
                .db
                .execute("DELETE FROM metadata WHERE key='schema_fingerprint'", [])
                .unwrap();
        }
        let service = Service::open(d.path()).unwrap();
        assert_eq!(
            service.decision_resource("DEC-0002").unwrap()["supersedes"],
            json!(["DEC-0001"])
        );
        assert_eq!(
            service.decision_resource("DEC-0001").unwrap()["superseded_by"],
            json!(["DEC-0002"])
        );
        let stored: String = service
            .db
            .query_row(
                "SELECT supersedes FROM decisions WHERE id='DEC-0002'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored, "[\"DEC-0001\"]");
    }

    #[test]
    fn materialized_markdown_tracks_supersession_and_retirement() {
        let d = fixture();
        let mut service = Service::open(d.path()).unwrap();
        service.refresh_if_stale().unwrap();
        let mut first = decision("Inject the store", &["Store"]);
        first.materialize = true;
        let first_path = service.record_decision(first).unwrap()["materialized_path"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(first_path, "docs/decisions/0001-inject-the-store.md");
        let mut second = decision("Cache the store", &["Store"]);
        second.materialize = true;
        service.record_decision(second).unwrap();

        let mut replacement = decision("Inject a cached store", &["Store"]);
        replacement.supersedes = vec!["DEC-0001".into()];
        replacement.recorded_by = "andreas".into();
        replacement.materialize = true;
        let recorded = service.record_decision(replacement).unwrap();
        let replacement_text = fs::read_to_string(
            d.path()
                .join(recorded["materialized_path"].as_str().unwrap()),
        )
        .unwrap();
        assert!(
            replacement_text.contains("Status: accepted\n\nSupersedes: DEC-0001\n\n## Rationale"),
            "{replacement_text}"
        );
        let first_text = fs::read_to_string(d.path().join(&first_path)).unwrap();
        assert!(
            first_text.contains("Status: superseded by DEC-0003\n\n## Rationale"),
            "{first_text}"
        );

        let retired = service
            .retire_decision(RetireDecision {
                id: "DEC-0002".into(),
                retired_by: "andreas".into(),
                note: "Caching moved to the client".into(),
            })
            .unwrap();
        assert_eq!(
            retired["materialized_path"],
            "docs/decisions/0002-cache-the-store.md"
        );
        let second_text =
            fs::read_to_string(d.path().join("docs/decisions/0002-cache-the-store.md")).unwrap();
        assert!(
            second_text.contains("Status: retired: Caching moved to the client\n\n## Rationale"),
            "{second_text}"
        );
    }
}

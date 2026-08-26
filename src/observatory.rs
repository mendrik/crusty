//! The public observatory boundary.
//!
//! Live source navigation never refreshes the derived index. Slow or mutating
//! operations are explicit durable tasks, while findings and human-owned work
//! live in a database that is independent from the rebuildable index.

use crate::Service;
use anyhow::{Context, Result, bail, ensure};
use chrono::{TimeZone, Utc};
use rand::random;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    env, fs,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
};
use tokio::task;
use walkdir::WalkDir;

const STATE_DIRECTORY: &str = ".rust-repo-intelligence";
const MEMORY_SCHEMA_VERSION: &str = "2";

const MEMORY_SCHEMA: &str = r#"
PRAGMA journal_mode=WAL;
PRAGMA foreign_keys=ON;
PRAGMA busy_timeout=2500;
CREATE TABLE IF NOT EXISTS metadata(key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS evidence_refs(
    id TEXT PRIMARY KEY, owner_type TEXT NOT NULL, owner_id TEXT NOT NULL,
    source_kind TEXT NOT NULL, uri TEXT NOT NULL, title TEXT NOT NULL,
    excerpt TEXT NOT NULL, publisher TEXT, accessed_at TEXT NOT NULL,
    local_revision TEXT, confidence REAL NOT NULL, is_primary INTEGER NOT NULL,
    qualification TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS evidence_owner ON evidence_refs(owner_type, owner_id);
CREATE TABLE IF NOT EXISTS findings(
    id TEXT PRIMARY KEY, title TEXT NOT NULL, summary TEXT NOT NULL,
    category TEXT NOT NULL, severity TEXT NOT NULL, confidence REAL NOT NULL,
    status TEXT NOT NULL, origin TEXT NOT NULL, research_run_id TEXT,
    scope_json TEXT NOT NULL, product_lens_json TEXT NOT NULL,
    created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
    reviewed_by TEXT, review_note TEXT
);
CREATE INDEX IF NOT EXISTS finding_inbox ON findings(status, severity, updated_at);
CREATE TABLE IF NOT EXISTS research_runs(
    id TEXT PRIMARY KEY, topic TEXT NOT NULL, goals_json TEXT NOT NULL,
    status TEXT NOT NULL, budget_json TEXT NOT NULL, consumed_json TEXT NOT NULL,
    questions_json TEXT NOT NULL, packet_json TEXT NOT NULL,
    schedule TEXT, next_due_at TEXT, task_id TEXT, error TEXT,
    created_at TEXT NOT NULL, updated_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS tasks(
    id TEXT PRIMARY KEY, kind TEXT NOT NULL, status TEXT NOT NULL,
    progress INTEGER NOT NULL, message TEXT NOT NULL, result_json TEXT,
    error TEXT, cancel_requested INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL, updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS tasks_state ON tasks(status, updated_at);
CREATE TABLE IF NOT EXISTS work_items(
    id TEXT PRIMARY KEY, title TEXT NOT NULL, status TEXT NOT NULL,
    priority TEXT NOT NULL, kind TEXT NOT NULL, scope_json TEXT NOT NULL,
    evidence_json TEXT NOT NULL, depends_json TEXT NOT NULL,
    blocked_json TEXT NOT NULL, acceptance_json TEXT NOT NULL,
    verification_json TEXT NOT NULL, discovered_from TEXT NOT NULL,
    provenance TEXT NOT NULL, confidence REAL NOT NULL,
    last_validated_snapshot TEXT NOT NULL, source_finding_id TEXT,
    human_owned INTEGER NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS work_state ON work_items(status, priority, updated_at);
CREATE TABLE IF NOT EXISTS legacy_records(
    kind TEXT NOT NULL, id TEXT NOT NULL, payload_json TEXT NOT NULL,
    migrated_at TEXT NOT NULL, PRIMARY KEY(kind, id)
);
"#;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct EvidenceInput {
    /// `local_repository`, `web_primary`, or `web_secondary`.
    pub source_kind: String,
    pub uri: String,
    pub title: String,
    #[serde(default)]
    pub excerpt: String,
    pub publisher: Option<String>,
    #[serde(default)]
    pub local_revision: Option<String>,
    #[serde(default = "default_confidence")]
    pub confidence: f64,
    #[serde(default)]
    pub is_primary: bool,
    #[serde(default)]
    pub qualification: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FindingInput {
    pub title: String,
    pub summary: String,
    /// `technical`, `product`, or `design`.
    pub category: String,
    #[serde(default = "normal")]
    pub severity: String,
    #[serde(default = "default_confidence")]
    pub confidence: f64,
    #[serde(default)]
    pub scope: Vec<String>,
    #[serde(default)]
    pub product_lenses: Vec<String>,
    #[serde(default)]
    pub evidence: Vec<EvidenceInput>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ResearchBudget {
    #[serde(default = "default_web_queries")]
    pub max_web_queries: usize,
    #[serde(default = "default_local_files")]
    pub max_local_files: usize,
    #[serde(default = "default_minutes")]
    pub max_minutes: usize,
    #[serde(default = "default_findings")]
    pub max_findings: usize,
}

impl Default for ResearchBudget {
    fn default() -> Self {
        Self {
            max_web_queries: default_web_queries(),
            max_local_files: default_local_files(),
            max_minutes: default_minutes(),
            max_findings: default_findings(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct SearchRequest {
    pub query: String,
    /// `exact` reads the live worktree; `broad` reads the published snapshot.
    #[serde(default = "exact")]
    pub mode: String,
    #[serde(default = "default_limit")]
    pub limit: usize,
    #[serde(default)]
    pub include_source: bool,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ContextRequest {
    pub query: String,
    #[serde(default = "default_budget")]
    pub budget: usize,
    #[serde(default = "default_context_limit")]
    pub limit: usize,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct PrepareRequest {
    pub intent: String,
    #[serde(default)]
    pub targets: Vec<String>,
    #[serde(default = "default_depth")]
    pub depth: usize,
    pub budget: Option<usize>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ValidateRequest {
    pub context_id: String,
    pub git_diff: Option<String>,
    #[serde(default)]
    pub run_checks: bool,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ResearchStartRequest {
    pub topic: String,
    #[serde(default)]
    pub goals: Vec<String>,
    #[serde(default)]
    pub budget: ResearchBudget,
    /// Optional human-defined recurrence description. Crusty records it but
    /// never enables an unattended external connector.
    pub schedule: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ResearchSubmitRequest {
    pub run_id: String,
    pub findings: Vec<FindingInput>,
    #[serde(default)]
    pub web_queries_used: usize,
    #[serde(default)]
    pub notes: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ReviewFindingRequest {
    pub finding_id: String,
    /// `accepted`, `dismissed`, or `needs_evidence`.
    pub decision: String,
    pub reviewed_by: String,
    #[serde(default)]
    pub note: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct PromoteFindingRequest {
    pub finding_id: String,
    pub reviewed_by: String,
    pub confirm_human: bool,
    #[serde(default = "normal")]
    pub priority: String,
    #[serde(default = "improvement")]
    pub kind: String,
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,
    #[serde(default)]
    pub verification: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct WorkCreateRequest {
    pub title: String,
    pub confirm_human: bool,
    #[serde(default = "proposed")]
    pub status: String,
    #[serde(default = "normal")]
    pub priority: String,
    #[serde(default = "improvement")]
    pub kind: String,
    #[serde(default)]
    pub scope: Vec<String>,
    #[serde(default)]
    pub evidence: Vec<String>,
    /// Exact work item IDs that must complete before this item is recommendable.
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// Exact work item IDs that currently prevent this item from proceeding.
    #[serde(default)]
    pub blocked_by: Vec<String>,
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,
    #[serde(default)]
    pub verification: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct WorkUpdateRequest {
    pub work_id: String,
    pub confirm_human: bool,
    pub status: Option<String>,
    pub priority: Option<String>,
    pub title: Option<String>,
    /// Replaces the dependency list when present; an empty list clears it.
    pub depends_on: Option<Vec<String>>,
    /// Replaces the blocker list when present; an empty list clears it.
    pub blocked_by: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct MemorySearchRequest {
    /// Natural-language or exact text to find in project prompts and preserved memory.
    pub query: String,
    #[serde(default = "default_memory_limit")]
    pub limit: usize,
    #[serde(default = "default_prompt_chars")]
    pub max_prompt_chars: usize,
}

fn default_confidence() -> f64 {
    0.7
}
fn default_web_queries() -> usize {
    6
}
fn default_local_files() -> usize {
    40
}
fn default_minutes() -> usize {
    10
}
fn default_findings() -> usize {
    12
}
fn default_limit() -> usize {
    20
}
fn default_context_limit() -> usize {
    24
}
fn default_budget() -> usize {
    3_000
}
fn default_depth() -> usize {
    2
}
fn default_memory_limit() -> usize {
    50
}
fn default_prompt_chars() -> usize {
    12_000
}
fn exact() -> String {
    "exact".into()
}
fn normal() -> String {
    "normal".into()
}
fn improvement() -> String {
    "improvement".into()
}
fn proposed() -> String {
    "proposed".into()
}

#[derive(Clone)]
pub struct Observatory {
    root: Arc<PathBuf>,
    memory_path: Arc<PathBuf>,
    dashboard_url: Arc<Mutex<Option<String>>>,
}

impl std::fmt::Debug for Observatory {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Observatory")
            .field("root", &self.root)
            .finish()
    }
}

impl Observatory {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = fs::canonicalize(root.into()).context("resolving workspace path")?;
        let state = root.join(STATE_DIRECTORY);
        fs::create_dir_all(&state)?;
        let memory_path = state.join("memory.sqlite3");
        let observatory = Self {
            root: Arc::new(root),
            memory_path: Arc::new(memory_path),
            dashboard_url: Arc::new(Mutex::new(None)),
        };
        observatory.initialize_memory()?;
        Ok(observatory)
    }

    pub fn root(&self) -> &Path {
        self.root.as_path()
    }

    fn db(&self) -> Result<Connection> {
        let db = Connection::open(self.memory_path.as_path())?;
        db.execute_batch("PRAGMA foreign_keys=ON; PRAGMA busy_timeout=2500;")?;
        Ok(db)
    }

    fn initialize_memory(&self) -> Result<()> {
        let db = Connection::open(self.memory_path.as_path())?;
        db.execute_batch(MEMORY_SCHEMA)?;
        ensure_column(&db, "research_runs", "task_id", "TEXT")?;
        db.execute(
            "INSERT OR REPLACE INTO metadata(key,value) VALUES ('schema_version',?1)",
            [MEMORY_SCHEMA_VERSION],
        )?;
        let migrated: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM metadata WHERE key='legacy_v7_migrated')",
            [],
            |row| row.get(0),
        )?;
        let legacy = self.root.join(STATE_DIRECTORY).join("index.sqlite3");
        if legacy.exists() {
            db.execute(
                "ATTACH DATABASE ?1 AS legacy",
                [legacy.to_string_lossy().as_ref()],
            )?;
            if !migrated && attached_table_exists(&db, "work_items")? {
                db.execute_batch(
                    r#"
                    INSERT OR IGNORE INTO work_items(
                        id,title,status,priority,kind,scope_json,evidence_json,
                        depends_json,blocked_json,acceptance_json,verification_json,
                        discovered_from,provenance,confidence,last_validated_snapshot,
                        source_finding_id,human_owned,created_at,updated_at)
                    SELECT id,title,status,priority,kind,scope_json,evidence_json,
                        depends_json,blocked_json,acceptance_json,verification_json,
                        discovered_from,provenance,confidence,last_validated_snapshot,
                        NULL, CASE WHEN provenance='HumanDecision' THEN 1 ELSE 0 END,
                        created_at,updated_at FROM legacy.work_items;
                "#,
                )?;
            }
            migrate_legacy_summaries(&db)?;
            db.execute("DETACH DATABASE legacy", [])?;
            if !migrated {
                db.execute(
                    "INSERT OR REPLACE INTO metadata(key,value) VALUES ('legacy_v7_migrated',?1)",
                    [Utc::now().to_rfc3339()],
                )?;
            }
        }
        Ok(())
    }

    pub async fn search(&self, request: SearchRequest) -> Result<Value> {
        ensure!(!request.query.trim().is_empty(), "query cannot be empty");
        ensure!(
            request.limit > 0 && request.limit <= 200,
            "limit must be 1..=200"
        );
        match request.mode.as_str() {
            "exact" => {
                let this = self.clone();
                task::spawn_blocking(move || this.live_exact_search(&request.query, request.limit))
                    .await
                    .context("exact search worker stopped")?
            }
            "broad" => {
                let root = self.root.as_ref().clone();
                let query = request.query;
                let limit = request.limit;
                let include_source = request.include_source;
                let freshness = self.freshness("published_snapshot")?;
                let results = task::spawn_blocking(move || {
                    Service::open(root)?.locate(&query, limit, include_source)
                })
                .await
                .context("broad search worker stopped")??;
                Ok(json!({"mode":"broad","freshness":freshness,"result":results}))
            }
            other => bail!("unknown search mode {other}; expected exact or broad"),
        }
    }

    fn live_exact_search(&self, query: &str, limit: usize) -> Result<Value> {
        let output = Command::new("rg")
            .current_dir(self.root.as_path())
            .args([
                "--json",
                "--fixed-strings",
                "--line-number",
                "--column",
                "--glob",
                "*.rs",
                "--glob",
                "!.git/**",
                "--glob",
                "!target/**",
                "--glob",
                "!.rust-repo-intelligence/**",
                "--",
                query,
                ".",
            ])
            .output();
        let mut matches = Vec::new();
        if let Ok(output) = output {
            if output.status.success() || output.status.code() == Some(1) {
                for line in String::from_utf8_lossy(&output.stdout).lines() {
                    let Ok(event) = serde_json::from_str::<Value>(line) else {
                        continue;
                    };
                    if event["type"] != "match" {
                        continue;
                    }
                    let data = &event["data"];
                    matches.push(json!({
                        "file": data["path"]["text"],
                        "line": data["line_number"],
                        "column": data["submatches"].get(0).and_then(|v| v["start"].as_u64()).map(|v| v + 1),
                        "source": data["lines"]["text"].as_str().unwrap_or("").trim_end()
                    }));
                    if matches.len() >= limit {
                        break;
                    }
                }
            } else {
                bail!(
                    "rg exact search failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        } else {
            matches = fallback_exact_search(self.root.as_path(), query, limit)?;
        }
        Ok(json!({
            "query": query,
            "mode": "exact",
            "authority": "live_worktree",
            "freshness": self.freshness("live_worktree")?,
            "matches": matches
        }))
    }

    pub async fn context(&self, request: ContextRequest) -> Result<Value> {
        let freshness = self.freshness("published_snapshot")?;
        let root = self.root.as_ref().clone();
        let result = task::spawn_blocking(move || {
            Service::open(root)?.context_pack(&request.query, request.budget, request.limit)
        })
        .await
        .context("context worker stopped")??;
        Ok(json!({"freshness":freshness,"result":result}))
    }

    pub async fn prepare_change(&self, request: PrepareRequest) -> Result<Value> {
        let freshness = self.freshness("published_snapshot")?;
        let root = self.root.as_ref().clone();
        let result = task::spawn_blocking(move || {
            Service::open(root)?.prepare_change(
                &request.intent,
                &request.targets,
                request.depth,
                request.budget,
            )
        })
        .await
        .context("prepare worker stopped")??;
        Ok(json!({"freshness":freshness,"result":result}))
    }

    pub fn start_prepare_change(&self, request: PrepareRequest) -> Result<Value> {
        self.spawn_blocking_task(
            "change.prepare",
            "queued for snapshot-aware change preparation",
            move |observatory, task_id| {
                observatory.update_task(&task_id, "running", 10, "building change evidence")?;
                let freshness = observatory.freshness("published_snapshot")?;
                let service = Service::open(observatory.root.as_ref().clone())?;
                let result = service.prepare_change(
                    &request.intent,
                    &request.targets,
                    request.depth,
                    request.budget,
                )?;
                Ok(json!({"freshness":freshness,"result":result}))
            },
        )
    }

    pub async fn validate_change(&self, request: ValidateRequest) -> Result<Value> {
        let freshness = self.freshness("published_snapshot")?;
        let root = self.root.as_ref().clone();
        let result = task::spawn_blocking(move || {
            Service::open(root)?.validate_change(
                &request.context_id,
                request.git_diff.as_deref(),
                request.run_checks,
            )
        })
        .await
        .context("validation worker stopped")??;
        Ok(json!({
            "freshness": freshness,
            "result": result,
            "authority_note": "This is guidance and provenance. Source, compiler/runtime behavior, and human review remain authoritative."
        }))
    }

    pub fn start_validate_change(&self, request: ValidateRequest) -> Result<Value> {
        self.spawn_blocking_task(
            "change.validate",
            "queued for diff validation",
            move |observatory, task_id| {
                observatory.update_task(
                    &task_id,
                    "running",
                    10,
                    "validating prepared evidence",
                )?;
                let freshness = observatory.freshness("published_snapshot")?;
                let service = Service::open(observatory.root.as_ref().clone())?;
                let result = service.validate_change(
                    &request.context_id,
                    request.git_diff.as_deref(),
                    request.run_checks,
                )?;
                Ok(json!({
                    "freshness":freshness,
                    "result":result,
                    "authority_note":"This is guidance and provenance. Source, compiler/runtime behavior, and human review remain authoritative."
                }))
            },
        )
    }

    pub fn freshness(&self, backend: &str) -> Result<Value> {
        let head = git_text(self.root.as_path(), &["rev-parse", "HEAD"]);
        let dirty = Command::new("git")
            .current_dir(self.root.as_path())
            .args(["status", "--porcelain=v2", "--untracked-files=all"])
            .output()
            .ok()
            .is_some_and(|output| !output.stdout.is_empty());
        let index_path = self.root.join(STATE_DIRECTORY).join("index.sqlite3");
        let mut indexed_head = None;
        let mut indexed_digest = None;
        let mut generation = None;
        let mut published_at = None;
        if index_path.exists() {
            let db = Connection::open_with_flags(
                index_path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )?;
            indexed_head = metadata_value(&db, "git_indexed_head").ok().flatten();
            if let Some((id, digest, published)) = db
                .query_row(
                    "SELECT id,workspace_digest,published_at FROM index_generations WHERE status='published' ORDER BY id DESC LIMIT 1",
                    [],
                    |row| Ok((row.get::<_,i64>(0)?,row.get::<_,String>(1)?,row.get::<_,Option<String>>(2)?)),
                )
                .optional()
                .ok()
                .flatten()
            {
                generation = Some(id);
                indexed_digest = Some(digest);
                published_at = published;
            }
        }
        let stale = generation.is_none() || dirty || head != indexed_head;
        Ok(json!({
            "backend": backend,
            "live":{"head":head,"dirty":dirty},
            "indexed":{"head":indexed_head,"workspace_digest":indexed_digest,"generation":generation,"published_at":published_at},
            "stale":stale,
            "confidence": if backend == "live_worktree" { 1.0 } else if stale { 0.65 } else { 0.95 },
            "note": if backend == "live_worktree" { "Read directly from current Rust source; no refresh was attempted." } else { "Read from the last atomically published snapshot; no implicit refresh was attempted." }
        }))
    }

    pub fn index_status(&self) -> Result<Value> {
        let db = self.db()?;
        let task = latest_task(&db, "index.refresh")?;
        Ok(json!({
            "freshness": self.freshness("published_snapshot")?,
            "latest_refresh_task": task,
            "publisher_lock": self.root.join(STATE_DIRECTORY).join("index.lock"),
            "policy": {"implicit_refresh":false,"single_writer":true,"reads_during_refresh":"last published generation"}
        }))
    }

    pub fn start_index_refresh(&self, scope: Option<String>) -> Result<Value> {
        let id = self.create_task("index.refresh", "queued for the index publisher")?;
        let this = self.clone();
        let task_id = id.clone();
        let background_id = id.clone();
        tokio::spawn(async move {
            let worker = this.clone();
            let outcome =
                task::spawn_blocking(move || worker.run_refresh(&task_id, scope.as_deref())).await;
            match outcome {
                Ok(Ok(value)) => {
                    let _ = this.finish_task(&background_id, value);
                }
                Ok(Err(error)) => {
                    let _ = this.fail_task(&background_id, &format!("{error:#}"));
                }
                Err(error) => {
                    let _ =
                        this.fail_task(&background_id, &format!("refresh worker stopped: {error}"));
                }
            }
        });
        Ok(
            json!({"task_id":id,"status":"queued","poll_with":"task.get","cancel_with":"task.cancel"}),
        )
    }

    fn run_refresh(&self, task_id: &str, scope: Option<&str>) -> Result<Value> {
        ensure!(
            !self.task_cancelled(task_id)?,
            "task cancelled before start"
        );
        self.update_task(task_id, "running", 5, "acquiring publisher lease")?;
        self.update_task(task_id, "running", 15, "refreshing derived index")?;
        let mut service = Service::open(self.root.as_ref().clone())?;
        let value = service.refresh(scope)?;
        self.update_task(task_id, "running", 95, "publishing generation")?;
        Ok(json!({"refresh":value,"freshness":self.freshness("published_snapshot")?}))
    }

    pub fn research_start(&self, request: ResearchStartRequest) -> Result<Value> {
        validate_budget(&request.budget)?;
        ensure!(!request.topic.trim().is_empty(), "topic cannot be empty");
        let run_id = new_id("research");
        let task_id =
            self.create_task("research.local_scan", "queued for bounded local research")?;
        let now = Utc::now().to_rfc3339();
        self.db()?.execute(
            "INSERT INTO research_runs(id,topic,goals_json,status,budget_json,consumed_json,questions_json,packet_json,schedule,next_due_at,task_id,error,created_at,updated_at) VALUES (?1,?2,?3,'queued',?4,'{}','[]','{}',?5,NULL,?6,NULL,?7,?7)",
            params![run_id, bounded(&request.topic, 500), serde_json::to_string(&request.goals)?, serde_json::to_string(&request.budget)?, request.schedule, task_id, now],
        )?;
        let this = self.clone();
        let async_run_id = run_id.clone();
        let async_task_id = task_id.clone();
        let background_run_id = run_id.clone();
        let background_task_id = task_id.clone();
        tokio::spawn(async move {
            let worker = this.clone();
            let outcome = task::spawn_blocking(move || {
                worker.build_research_packet(&async_run_id, &async_task_id)
            })
            .await;
            match outcome {
                Ok(Ok(packet)) => {
                    let _ = this.finish_task(&background_task_id, packet);
                }
                Ok(Err(error)) => {
                    let _ = this.fail_research(
                        &background_run_id,
                        &background_task_id,
                        &format!("{error:#}"),
                    );
                }
                Err(error) => {
                    let _ = this.fail_research(
                        &background_run_id,
                        &background_task_id,
                        &format!("research worker stopped: {error}"),
                    );
                }
            }
        });
        Ok(
            json!({"run_id":run_id,"task_id":task_id,"status":"queued","next":"poll task.get, then call research.packet"}),
        )
    }

    fn build_research_packet(&self, run_id: &str, task_id: &str) -> Result<Value> {
        ensure!(
            !self.task_cancelled(task_id)?,
            "task cancelled before local scan"
        );
        self.update_task(task_id, "running", 10, "scanning local Rust evidence")?;
        let db = self.db()?;
        db.execute(
            "UPDATE research_runs SET status='scanning',updated_at=?1 WHERE id=?2 AND status='queued'",
            params![Utc::now().to_rfc3339(), run_id],
        )?;
        let (topic, goals, budget): (String, String, String) = db.query_row(
            "SELECT topic,goals_json,budget_json FROM research_runs WHERE id=?1",
            [run_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        let budget: ResearchBudget = serde_json::from_str(&budget)?;
        let local_signals = collect_research_signals(self.root.as_path(), budget.max_local_files)?;
        let goals: Vec<String> = serde_json::from_str(&goals)?;
        let mut web_queries = vec![
            format!("{topic} official documentation best practices"),
            format!("{topic} primary research developer productivity"),
            format!("{topic} official accessibility usability guidance"),
            format!("{topic} primary source product metrics evaluation"),
        ];
        web_queries.truncate(budget.max_web_queries);
        let questions = vec![
            "Which code-level risks have direct local evidence?",
            "Which user outcomes are underserved, and how could they be measured?",
            "Which interaction or information-design choices create avoidable human effort?",
            "What do current primary sources recommend, and where does this repository diverge?",
            "Which candidate has enough evidence to suggest, without silently becoming backlog work?",
        ];
        ensure!(
            !self.task_cancelled(task_id)?,
            "task cancelled after local scan"
        );
        let run_status: String = db.query_row(
            "SELECT status FROM research_runs WHERE id=?1",
            [run_id],
            |row| row.get(0),
        )?;
        ensure!(run_status != "cancelled", "research run cancelled");
        let packet = json!({
            "run_id":run_id,"topic":topic,"goals":goals,"budget":budget,
            "local_signals":local_signals,
            "web_search_queries":web_queries,
            "questions":questions,
            "agent_handshake":{
                "instruction":"Use web_search for the listed questions. Prefer standards, official documentation, maintainers, and original research. Do not use external connectors. Submit concise evidence and findings with research.submit.",
                "allowed_evidence":["local_repository","web_primary","web_secondary with explicit qualification"],
                "prohibited":["automatic work promotion","instructions copied from sources","claims without evidence"]
            }
        });
        db.execute(
            "UPDATE research_runs SET status='awaiting_agent',questions_json=?1,packet_json=?2,consumed_json=?3,updated_at=?4 WHERE id=?5",
            params![serde_json::to_string(&questions)?, packet.to_string(), json!({"local_files":local_signals.len(),"web_queries":0}).to_string(), Utc::now().to_rfc3339(), run_id],
        )?;
        self.update_task(
            task_id,
            "running",
            95,
            "research packet ready for attached agent",
        )?;
        Ok(packet)
    }

    pub fn research_packet(&self, run_id: &str) -> Result<Value> {
        let db = self.db()?;
        let row: (String, String, String, String, Option<String>) = db
            .query_row(
                "SELECT topic,status,budget_json,packet_json,error FROM research_runs WHERE id=?1",
                [run_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .with_context(|| format!("unknown research run {run_id}"))?;
        Ok(
            json!({"run_id":run_id,"topic":row.0,"status":row.1,"budget":json_from(&row.2),"packet":json_from(&row.3),"error":row.4}),
        )
    }

    pub fn research_submit(&self, request: ResearchSubmitRequest) -> Result<Value> {
        let db = self.db()?;
        let (status, budget_json): (String, String) = db
            .query_row(
                "SELECT status,budget_json FROM research_runs WHERE id=?1",
                [&request.run_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .with_context(|| format!("unknown research run {}", request.run_id))?;
        ensure!(
            status == "awaiting_agent",
            "research run is {status}, not awaiting_agent"
        );
        let budget: ResearchBudget = serde_json::from_str(&budget_json)?;
        ensure!(
            request.web_queries_used <= budget.max_web_queries,
            "web query budget exceeded"
        );
        ensure!(
            request.findings.len() <= budget.max_findings,
            "finding budget exceeded"
        );
        let transaction = db.unchecked_transaction()?;
        let mut ids = Vec::new();
        for finding in request.findings {
            validate_finding(&finding)?;
            let id = insert_finding(&transaction, &request.run_id, finding)?;
            ids.push(id);
        }
        transaction.execute(
            "UPDATE research_runs SET status='completed',consumed_json=?1,updated_at=?2 WHERE id=?3",
            params![json!({"web_queries":request.web_queries_used,"findings":ids.len(),"notes":bounded(&request.notes,2000)}).to_string(), Utc::now().to_rfc3339(), request.run_id],
        )?;
        transaction.commit()?;
        Ok(
            json!({"run_id":request.run_id,"status":"completed","finding_ids":ids,"promotion":"Human review is required before any finding can become work."}),
        )
    }

    pub fn research_list(&self, limit: usize) -> Result<Value> {
        let db = self.db()?;
        let mut statement = db.prepare("SELECT id,topic,status,budget_json,consumed_json,schedule,error,created_at,updated_at FROM research_runs ORDER BY updated_at DESC LIMIT ?1")?;
        let runs = statement.query_map([limit.min(200) as i64], |row| Ok(json!({
            "id":row.get::<_,String>(0)?,"topic":row.get::<_,String>(1)?,"status":row.get::<_,String>(2)?,
            "budget":json_from(&row.get::<_,String>(3)?),"consumed":json_from(&row.get::<_,String>(4)?),
            "schedule":row.get::<_,Option<String>>(5)?,"error":row.get::<_,Option<String>>(6)?,
            "created_at":row.get::<_,String>(7)?,"updated_at":row.get::<_,String>(8)?
        })))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(json!({"runs":runs}))
    }

    pub fn research_cancel(&self, run_id: &str) -> Result<Value> {
        let db = self.db()?;
        let task_id: Option<String> = db
            .query_row(
                "SELECT task_id FROM research_runs WHERE id=?1",
                [run_id],
                |row| row.get(0),
            )
            .optional()?;
        let changed = db.execute(
            "UPDATE research_runs SET status='cancelled',updated_at=?1 WHERE id=?2 AND status IN ('queued','scanning','awaiting_agent')",
            params![Utc::now().to_rfc3339(), run_id],
        )?;
        if let Some(task_id) = task_id.as_deref() {
            let _ = self.task_cancel(task_id);
        }
        Ok(
            json!({"run_id":run_id,"task_id":task_id,"cancelled":changed == 1,"note":"Stored evidence and already-submitted findings are retained."}),
        )
    }

    pub fn finding_list(
        &self,
        status: Option<&str>,
        query: Option<&str>,
        limit: usize,
    ) -> Result<Value> {
        let db = self.db()?;
        let query = query.unwrap_or("").to_lowercase();
        let status = status.unwrap_or("");
        let mut statement = db.prepare("SELECT id,title,summary,category,severity,confidence,status,origin,research_run_id,scope_json,product_lens_json,created_at,updated_at,reviewed_by,review_note FROM findings WHERE (?1='' OR status=?1) AND (?2='' OR lower(id)=?2 OR lower(title) LIKE '%'||?2||'%' OR lower(summary) LIKE '%'||?2||'%') ORDER BY CASE severity WHEN 'critical' THEN 0 WHEN 'high' THEN 1 WHEN 'normal' THEN 2 ELSE 3 END, updated_at DESC LIMIT ?3")?;
        let findings = statement
            .query_map(params![status, query, limit.min(200) as i64], finding_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(json!({"status":status,"query":query,"findings":findings}))
    }

    pub fn finding_get(&self, id: &str) -> Result<Value> {
        let db = self.db()?;
        let finding = db.query_row("SELECT id,title,summary,category,severity,confidence,status,origin,research_run_id,scope_json,product_lens_json,created_at,updated_at,reviewed_by,review_note FROM findings WHERE id=?1", [id], finding_row)
            .with_context(|| format!("unknown finding {id}"))?;
        Ok(json!({"finding":finding,"evidence":evidence_for(&db,"finding",id)?}))
    }

    pub fn finding_review(&self, request: ReviewFindingRequest) -> Result<Value> {
        ensure!(
            ["accepted", "dismissed", "needs_evidence"].contains(&request.decision.as_str()),
            "invalid review decision"
        );
        ensure!(
            !request.reviewed_by.trim().is_empty(),
            "reviewed_by is required"
        );
        let changed = self.db()?.execute(
            "UPDATE findings SET status=?1,reviewed_by=?2,review_note=?3,updated_at=?4 WHERE id=?5",
            params![
                request.decision,
                bounded(&request.reviewed_by, 200),
                bounded(&request.note, 2000),
                Utc::now().to_rfc3339(),
                request.finding_id
            ],
        )?;
        ensure!(changed == 1, "unknown finding");
        Ok(
            json!({"finding_id":request.finding_id,"status":request.decision,"reviewed_by":request.reviewed_by}),
        )
    }

    pub fn finding_promote(&self, request: PromoteFindingRequest) -> Result<Value> {
        ensure!(request.confirm_human, "human confirmation is required");
        ensure!(
            !request.reviewed_by.trim().is_empty(),
            "reviewed_by is required"
        );
        let db = self.db()?;
        let (title, summary, status, scope): (String, String, String, String) = db
            .query_row(
                "SELECT title,summary,status,scope_json FROM findings WHERE id=?1",
                [&request.finding_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .with_context(|| format!("unknown finding {}", request.finding_id))?;
        ensure!(
            status == "accepted",
            "finding must be accepted before promotion"
        );
        let id = format!(
            "work_{}",
            &blake3::hash(request.finding_id.as_bytes()).to_hex()[..12]
        );
        let now = Utc::now().to_rfc3339();
        db.execute("INSERT OR IGNORE INTO work_items(id,title,status,priority,kind,scope_json,evidence_json,depends_json,blocked_json,acceptance_json,verification_json,discovered_from,provenance,confidence,last_validated_snapshot,source_finding_id,human_owned,created_at,updated_at) VALUES (?1,?2,'accepted',?3,?4,?5,?6,'[]','[]',?7,?8,?9,'HumanDecision',1.0,'',?10,1,?11,?11)", params![id,title,request.priority,request.kind,scope,json!([summary]).to_string(),serde_json::to_string(&request.acceptance_criteria)?,serde_json::to_string(&request.verification)?,format!("finding promotion confirmed by {}", bounded(&request.reviewed_by,200)),request.finding_id,now])?;
        Ok(
            json!({"finding_id":request.finding_id,"work_id":id,"status":"accepted","owner":"human"}),
        )
    }

    pub fn work_list(&self, query: Option<&str>, limit: usize) -> Result<Value> {
        let db = self.db()?;
        let query = query.unwrap_or("").to_lowercase();
        let mut statement = db.prepare("SELECT id,title,status,priority,kind,scope_json,evidence_json,depends_json,blocked_json,acceptance_json,verification_json,discovered_from,provenance,confidence,last_validated_snapshot,source_finding_id,human_owned,updated_at FROM work_items WHERE ?1='' OR lower(id)=?1 OR lower(title) LIKE '%'||?1||'%' OR lower(scope_json) LIKE '%'||?1||'%' OR lower(evidence_json) LIKE '%'||?1||'%' ORDER BY CASE priority WHEN 'critical' THEN 0 WHEN 'high' THEN 1 WHEN 'normal' THEN 2 ELSE 3 END, updated_at DESC LIMIT ?2")?;
        let mut items = statement
            .query_map(params![query, limit.min(200) as i64], work_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        for item in &mut items {
            decorate_work_readiness(&db, item)?;
        }
        Ok(
            json!({"query":query,"items":items,"authority":"human-owned project memory; this is not a GitHub issue tracker"}),
        )
    }

    pub fn work_get(&self, id: &str) -> Result<Value> {
        let db = self.db()?;
        let mut item = db
            .query_row("SELECT id,title,status,priority,kind,scope_json,evidence_json,depends_json,blocked_json,acceptance_json,verification_json,discovered_from,provenance,confidence,last_validated_snapshot,source_finding_id,human_owned,updated_at FROM work_items WHERE id=?1", [id], work_row)
            .with_context(|| format!("unknown work item {id}"))?;
        decorate_work_readiness(&db, &mut item)?;
        Ok(json!({"work":item}))
    }

    pub fn work_recommend(&self, query: Option<&str>) -> Result<Value> {
        let list = self.work_list(query, 100)?;
        let next = list["items"]
            .as_array()
            .and_then(|items| {
                items.iter().find(|item| {
                    item["human_owned"] == true
                        && ["accepted", "active", "in_progress"]
                            .contains(&item["status"].as_str().unwrap_or(""))
                        && item["ready"] == true
                })
            })
            .cloned();
        Ok(
            json!({"query":query,"recommendation":next,"selection_note":"Only human-owned, accepted or active work with no unresolved dependencies or blockers is eligible."}),
        )
    }

    pub fn work_create(&self, request: WorkCreateRequest) -> Result<Value> {
        ensure!(request.confirm_human, "human confirmation is required");
        ensure!(!request.title.trim().is_empty(), "title cannot be empty");
        let depends_on = normalize_work_links(request.depends_on, "depends_on")?;
        let blocked_by = normalize_work_links(request.blocked_by, "blocked_by")?;
        let id = new_id("work");
        let now = Utc::now().to_rfc3339();
        let db = self.db()?;
        validate_work_relationships(&db, Some(&id), &depends_on, &blocked_by)?;
        db.execute("INSERT INTO work_items(id,title,status,priority,kind,scope_json,evidence_json,depends_json,blocked_json,acceptance_json,verification_json,discovered_from,provenance,confidence,last_validated_snapshot,source_finding_id,human_owned,created_at,updated_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,'explicit human creation','HumanDecision',1.0,'',NULL,1,?12,?12)", params![id,bounded(&request.title,500),request.status,request.priority,request.kind,serde_json::to_string(&request.scope)?,serde_json::to_string(&request.evidence)?,serde_json::to_string(&depends_on)?,serde_json::to_string(&blocked_by)?,serde_json::to_string(&request.acceptance_criteria)?,serde_json::to_string(&request.verification)?,now])?;
        Ok(json!({"work_id":id,"status":request.status,"owner":"human"}))
    }

    pub fn work_update(&self, request: WorkUpdateRequest) -> Result<Value> {
        ensure!(request.confirm_human, "human confirmation is required");
        let db = self.db()?;
        let current: (String, String, String, String, String) = db.query_row(
            "SELECT title,status,priority,depends_json,blocked_json FROM work_items WHERE id=?1",
            [&request.work_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .with_context(|| format!("unknown work item {}", request.work_id))?;
        let relationships_changed = request.depends_on.is_some() || request.blocked_by.is_some();
        let depends_on = match request.depends_on {
            Some(values) => normalize_work_links(values, "depends_on")?,
            None => work_links_from_json(&current.3),
        };
        let blocked_by = match request.blocked_by {
            Some(values) => normalize_work_links(values, "blocked_by")?,
            None => work_links_from_json(&current.4),
        };
        if relationships_changed {
            validate_work_relationships(&db, Some(&request.work_id), &depends_on, &blocked_by)?;
        }
        db.execute("UPDATE work_items SET title=?1,status=?2,priority=?3,depends_json=?4,blocked_json=?5,human_owned=1,provenance='HumanDecision',updated_at=?6 WHERE id=?7", params![request.title.unwrap_or(current.0),request.status.unwrap_or(current.1),request.priority.unwrap_or(current.2),serde_json::to_string(&depends_on)?,serde_json::to_string(&blocked_by)?,Utc::now().to_rfc3339(),request.work_id])?;
        self.work_get(&request.work_id)
    }

    pub fn memory_search(&self, request: MemorySearchRequest) -> Result<Value> {
        ensure!(!request.query.trim().is_empty(), "query cannot be empty");
        ensure!(
            request.query.len() <= 500,
            "query must be at most 500 bytes"
        );
        ensure!(
            request.limit > 0 && request.limit <= 200,
            "limit must be 1..=200"
        );
        ensure!(
            (200..=50_000).contains(&request.max_prompt_chars),
            "max_prompt_chars must be 200..=50000"
        );
        let records = self.search_legacy_memory(&request.query, request.limit)?;
        let (prompts, history) = search_codex_prompt_history(
            self.root.as_path(),
            &request.query,
            request.limit,
            request.max_prompt_chars,
            codex_home(),
        )?;
        Ok(json!({
            "query": request.query,
            "repository": self.root,
            "prompts": prompts,
            "records": records,
            "history": history,
            "authority": {
                "prompts": "verbatim user-authored Codex session history scoped to this repository",
                "records": "preserved legacy decisions, steerings, problems, and quality constraints",
                "work": "recovered prompts and records do not become project work until a human explicitly creates or confirms work"
            }
        }))
    }

    fn search_legacy_memory(&self, query: &str, limit: usize) -> Result<Vec<Value>> {
        let db = self.db()?;
        let mut statement = db.prepare(
            "SELECT kind,id,payload_json,migrated_at FROM legacy_records ORDER BY migrated_at DESC",
        )?;
        let mut matches = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?
            .filter_map(rusqlite::Result::ok)
            .filter_map(|(kind, id, payload, migrated_at)| {
                search_score(&format!("{kind} {id} {payload}"), query).map(|score| {
                    (
                        score,
                        json!({
                            "kind": kind,
                            "id": id,
                            "payload": json_from(&payload),
                            "migrated_at": migrated_at,
                        }),
                    )
                })
            })
            .collect::<Vec<_>>();
        matches.sort_by(|left, right| right.0.cmp(&left.0));
        Ok(matches
            .into_iter()
            .take(limit)
            .map(|(_, value)| value)
            .collect())
    }

    pub fn task_get(&self, id: &str) -> Result<Value> {
        let db = self.db()?;
        let task = db.query_row("SELECT id,kind,status,progress,message,result_json,error,cancel_requested,created_at,updated_at FROM tasks WHERE id=?1", [id], task_row)
            .with_context(|| format!("unknown task {id}"))?;
        Ok(json!({"task":task}))
    }

    pub fn task_cancel(&self, id: &str) -> Result<Value> {
        let changed = self.db()?.execute("UPDATE tasks SET cancel_requested=1,message='cancellation requested',updated_at=?1 WHERE id=?2 AND status IN ('queued','running')", params![Utc::now().to_rfc3339(),id])?;
        Ok(
            json!({"task_id":id,"cancellation_requested":changed == 1,"note":"Cooperative cancellation takes effect at the next safe boundary; an index transaction is never interrupted during publication."}),
        )
    }

    pub async fn dashboard_open(&self) -> Result<Value> {
        if let Some(url) = self
            .dashboard_url
            .lock()
            .expect("dashboard mutex poisoned")
            .clone()
        {
            return Ok(json!({"url":url,"loopback_only":true}));
        }
        let url = crate::dashboard::start(self.clone()).await?;
        *self.dashboard_url.lock().expect("dashboard mutex poisoned") = Some(url.clone());
        Ok(
            json!({"url":url,"loopback_only":true,"authentication":"one-time URL token establishes a SameSite=Strict HttpOnly session"}),
        )
    }

    fn create_task(&self, kind: &str, message: &str) -> Result<String> {
        let id = new_id("task");
        let now = Utc::now().to_rfc3339();
        self.db()?.execute("INSERT INTO tasks(id,kind,status,progress,message,result_json,error,cancel_requested,created_at,updated_at) VALUES (?1,?2,'queued',0,?3,NULL,NULL,0,?4,?4)", params![id,kind,message,now])?;
        Ok(id)
    }

    fn spawn_blocking_task<F>(&self, kind: &str, message: &str, operation: F) -> Result<Value>
    where
        F: FnOnce(Observatory, String) -> Result<Value> + Send + 'static,
    {
        let id = self.create_task(kind, message)?;
        let background_id = id.clone();
        let this = self.clone();
        tokio::spawn(async move {
            let worker = this.clone();
            let worker_id = background_id.clone();
            let outcome = task::spawn_blocking(move || {
                ensure!(
                    !worker.task_cancelled(&worker_id)?,
                    "task cancelled before start"
                );
                operation(worker, worker_id)
            })
            .await;
            match outcome {
                Ok(Ok(value)) => {
                    let _ = this.finish_task(&background_id, value);
                }
                Ok(Err(error)) => {
                    let _ = this.fail_task(&background_id, &format!("{error:#}"));
                }
                Err(error) => {
                    let _ = this
                        .fail_task(&background_id, &format!("blocking worker stopped: {error}"));
                }
            }
        });
        Ok(
            json!({"task_id":id,"status":"queued","poll_with":"task.get","cancel_with":"task.cancel"}),
        )
    }

    fn update_task(&self, id: &str, status: &str, progress: i64, message: &str) -> Result<()> {
        self.db()?.execute(
            "UPDATE tasks SET status=?1,progress=?2,message=?3,updated_at=?4 WHERE id=?5",
            params![
                status,
                progress.clamp(0, 100),
                message,
                Utc::now().to_rfc3339(),
                id
            ],
        )?;
        Ok(())
    }

    fn finish_task(&self, id: &str, result: Value) -> Result<()> {
        self.db()?.execute("UPDATE tasks SET status='completed',progress=100,message='completed',result_json=?1,updated_at=?2 WHERE id=?3", params![result.to_string(),Utc::now().to_rfc3339(),id])?;
        Ok(())
    }

    fn fail_task(&self, id: &str, error: &str) -> Result<()> {
        self.db()?.execute(
            "UPDATE tasks SET status='failed',message='failed',error=?1,updated_at=?2 WHERE id=?3",
            params![bounded(error, 4000), Utc::now().to_rfc3339(), id],
        )?;
        Ok(())
    }

    fn fail_research(&self, run_id: &str, task_id: &str, error: &str) -> Result<()> {
        self.fail_task(task_id, error)?;
        self.db()?.execute(
            "UPDATE research_runs SET status='failed',error=?1,updated_at=?2 WHERE id=?3",
            params![bounded(error, 4000), Utc::now().to_rfc3339(), run_id],
        )?;
        Ok(())
    }

    fn task_cancelled(&self, id: &str) -> Result<bool> {
        Ok(self.db()?.query_row(
            "SELECT cancel_requested FROM tasks WHERE id=?1",
            [id],
            |row| row.get::<_, bool>(0),
        )?)
    }
}

fn attached_table_exists(db: &Connection, table: &str) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM legacy.sqlite_master WHERE type='table' AND name=?1)",
        [table],
        |row| row.get(0),
    )?)
}

fn ensure_column(db: &Connection, table: &str, column: &str, kind: &str) -> Result<()> {
    ensure!(
        table
            .chars()
            .all(|value| value.is_ascii_alphanumeric() || value == '_')
            && column
                .chars()
                .all(|value| value.is_ascii_alphanumeric() || value == '_')
            && kind.chars().all(|value| value.is_ascii_alphanumeric()),
        "invalid schema identifier"
    );
    let mut statement = db.prepare(&format!("PRAGMA table_info({table})"))?;
    let exists = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .filter_map(rusqlite::Result::ok)
        .any(|name| name == column);
    if !exists {
        db.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {kind}"))?;
    }
    Ok(())
}

fn migrate_legacy_summaries(db: &Connection) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    if attached_table_exists(db, "decisions")? {
        db.execute("INSERT OR REPLACE INTO legacy_records(kind,id,payload_json,migrated_at) SELECT 'decision',id,json_object('title',title,'status',status,'rationale',rationale,'applies_to',applies_to,'consequences',consequences,'revision',revision,'created_at',created_at),?1 FROM legacy.decisions", [&now])?;
    }
    if attached_table_exists(db, "steerings")? {
        db.execute("INSERT OR REPLACE INTO legacy_records(kind,id,payload_json,migrated_at) SELECT 'steering',id,json_object('title',title,'status',status,'priority',priority,'instruction',instruction,'scope',scope,'revision',revision,'created_at',created_at),?1 FROM legacy.steerings", [&now])?;
    }
    if attached_table_exists(db, "problem_records")? {
        db.execute("INSERT OR REPLACE INTO legacy_records(kind,id,payload_json,migrated_at) SELECT 'problem',id,json_object('summary',summary,'family',family,'status',status,'scope',scope_json,'root_cause',root_cause,'fix_reference',fix_reference,'evidence',evidence_json,'revision',revision,'created_at',created_at),?1 FROM legacy.problem_records", [&now])?;
    }
    if attached_table_exists(db, "quality_constraints")? {
        db.execute("INSERT OR REPLACE INTO legacy_records(kind,id,payload_json,migrated_at) SELECT 'quality_constraint',id,json_object('rule',rule,'category',category,'status',status,'scope',scope_json,'recipe',recipe_json,'enforcement',enforcement,'maturity',maturity,'revision',revision,'created_at',created_at),?1 FROM legacy.quality_constraints", [&now])?;
    }
    Ok(())
}

fn codex_home() -> Option<PathBuf> {
    env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")))
}

fn search_codex_prompt_history(
    repository: &Path,
    query: &str,
    limit: usize,
    max_prompt_chars: usize,
    codex_home: Option<PathBuf>,
) -> Result<(Vec<Value>, Value)> {
    let Some(codex_home) = codex_home else {
        return Ok((
            Vec::new(),
            json!({"available":false,"reason":"Codex home could not be resolved"}),
        ));
    };
    let (thread_ids, rollout_files, side_session_files) =
        project_session_sources(&codex_home, repository)?;
    if thread_ids.is_empty() {
        return Ok((
            Vec::new(),
            json!({
                "available": false,
                "reason": "No Codex session metadata matched this repository",
                "session_root": codex_home.join("sessions"),
            }),
        ));
    }

    let history_db = codex_home.join("thread_history_1.sqlite");
    let (prompts, source, scanned) = if history_db.exists() {
        match search_thread_history_db(&history_db, &thread_ids, query, limit, max_prompt_chars) {
            Ok((prompts, scanned)) => (prompts, "thread_history_1.sqlite", scanned),
            Err(_) => {
                let (prompts, scanned) =
                    search_rollout_logs(&rollout_files, query, limit, max_prompt_chars)?;
                (prompts, "rollout_jsonl_fallback", scanned)
            }
        }
    } else {
        let (prompts, scanned) =
            search_rollout_logs(&rollout_files, query, limit, max_prompt_chars)?;
        (prompts, "rollout_jsonl", scanned)
    };
    Ok((
        prompts,
        json!({
            "available": true,
            "source": source,
            "project_threads": thread_ids.len(),
            "project_rollout_files": rollout_files.len(),
            "side_session_files": side_session_files,
            "messages_scanned": scanned,
            "scope": "session metadata cwd exactly matches the current repository",
            "privacy": "only matching user-authored text is returned; tool output and assistant content are excluded",
        }),
    ))
}

fn project_session_sources(
    codex_home: &Path,
    repository: &Path,
) -> Result<(HashSet<String>, Vec<PathBuf>, usize)> {
    let sessions = codex_home.join("sessions");
    if !sessions.exists() {
        return Ok((HashSet::new(), Vec::new(), 0));
    }
    let canonical_repository =
        fs::canonicalize(repository).unwrap_or_else(|_| repository.to_path_buf());
    let mut thread_ids = HashSet::new();
    let mut files = Vec::new();
    let mut side_session_files = 0;
    for entry in WalkDir::new(&sessions)
        .follow_links(false)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file())
        .filter(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|value| value == "jsonl")
        })
    {
        let file = fs::File::open(entry.path())?;
        let mut reader = BufReader::new(file);
        let mut line = String::new();
        let mut metadata = None;
        for _ in 0..8 {
            line.clear();
            if reader.read_line(&mut line)? == 0 {
                break;
            }
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if value["type"] == "session_meta" {
                metadata = Some(value);
                break;
            }
        }
        let Some(metadata) = metadata else {
            continue;
        };
        let Some(cwd) = metadata["payload"]["cwd"].as_str() else {
            continue;
        };
        let session_repository = fs::canonicalize(cwd).unwrap_or_else(|_| PathBuf::from(cwd));
        if session_repository != canonical_repository {
            continue;
        }
        if let Some(id) = metadata["payload"]["id"].as_str() {
            thread_ids.insert(id.to_owned());
        }
        if let Some(id) = metadata["payload"]["history_base"]["thread_id"].as_str() {
            thread_ids.insert(id.to_owned());
        }
        if entry
            .path()
            .file_stem()
            .and_then(|value| value.to_str())
            .is_some_and(|value| value.contains("_01"))
        {
            side_session_files += 1;
        }
        files.push(entry.path().to_path_buf());
    }
    Ok((thread_ids, files, side_session_files))
}

fn search_thread_history_db(
    path: &Path,
    project_threads: &HashSet<String>,
    query: &str,
    limit: usize,
    max_prompt_chars: usize,
) -> Result<(Vec<Value>, usize)> {
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut statement = db.prepare(
        "SELECT thread_id,turn_id,rollout_ordinal,created_at_ms,item_json
         FROM thread_items WHERE item_type='userMessage'
         ORDER BY created_at_ms DESC LIMIT 250000",
    )?;
    let mut scanned = 0;
    let mut seen = HashSet::new();
    let mut matches = Vec::new();
    for row in statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, String>(4)?,
        ))
    })? {
        let (thread_id, turn_id, ordinal, created_at_ms, raw) = row?;
        if !project_threads.contains(&thread_id) {
            continue;
        }
        scanned += 1;
        let Some((text, images)) = prompt_from_item_json(&raw) else {
            continue;
        };
        if !is_user_prompt(&text) {
            continue;
        }
        let Some(score) = search_score(&text, query) else {
            continue;
        };
        let dedupe = format!("{thread_id}\n{text}");
        if !seen.insert(dedupe) {
            continue;
        }
        let original_chars = text.chars().count();
        let provenance = format!("codex-thread://{thread_id}/{turn_id}/{ordinal}");
        matches.push((
            score,
            created_at_ms,
            json!({
                "thread_id": thread_id,
                "turn_id": turn_id,
                "rollout_ordinal": ordinal,
                "created_at": Utc.timestamp_millis_opt(created_at_ms).single().map(|value| value.to_rfc3339()),
                "text": bounded(&text, max_prompt_chars),
                "original_chars": original_chars,
                "truncated": text.len() > max_prompt_chars,
                "images": images,
                "provenance": provenance,
            }),
        ));
    }
    matches.sort_by(|left, right| right.0.cmp(&left.0).then(right.1.cmp(&left.1)));
    Ok((
        matches
            .into_iter()
            .take(limit)
            .map(|(_, _, value)| value)
            .collect(),
        scanned,
    ))
}

fn search_rollout_logs(
    files: &[PathBuf],
    query: &str,
    limit: usize,
    max_prompt_chars: usize,
) -> Result<(Vec<Value>, usize)> {
    let mut matches = Vec::new();
    let mut seen = HashSet::new();
    let mut scanned = 0;
    for path in files {
        let file = fs::File::open(path)?;
        for line in BufReader::new(file).lines() {
            let line = line?;
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if value["type"] != "response_item"
                || value["payload"]["type"] != "message"
                || value["payload"]["role"] != "user"
            {
                continue;
            }
            scanned += 1;
            let text = value["payload"]["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|part| part["type"] == "input_text")
                .filter_map(|part| part["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n");
            if !is_user_prompt(&text) || !seen.insert(text.clone()) {
                continue;
            }
            let Some(score) = search_score(&text, query) else {
                continue;
            };
            let timestamp = value["timestamp"].as_str().unwrap_or_default().to_owned();
            let original_chars = text.chars().count();
            let session = path
                .file_stem()
                .and_then(|value| value.to_str())
                .unwrap_or("unknown")
                .to_owned();
            let provenance = format!("codex-rollout://{session}");
            matches.push((
                score,
                timestamp.clone(),
                json!({
                    "session": session,
                    "created_at": timestamp,
                    "text": bounded(&text, max_prompt_chars),
                    "original_chars": original_chars,
                    "truncated": text.len() > max_prompt_chars,
                    "provenance": provenance,
                }),
            ));
        }
    }
    matches.sort_by(|left, right| right.0.cmp(&left.0).then(right.1.cmp(&left.1)));
    Ok((
        matches
            .into_iter()
            .take(limit)
            .map(|(_, _, value)| value)
            .collect(),
        scanned,
    ))
}

fn prompt_from_item_json(raw: &str) -> Option<(String, Vec<String>)> {
    let value = serde_json::from_str::<Value>(raw).ok()?;
    let content = value["content"].as_array()?;
    let text = content
        .iter()
        .filter(|part| part["type"] == "text")
        .filter_map(|part| part["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let images = content
        .iter()
        .filter(|part| part["type"] == "localImage")
        .filter_map(|part| part["path"].as_str())
        .map(str::to_owned)
        .collect();
    (!text.trim().is_empty()).then_some((text, images))
}

fn is_user_prompt(text: &str) -> bool {
    let text = text.trim_start();
    ![
        "# AGENTS.md instructions",
        "<codex_internal_context",
        "<environment_context",
        "<recommended_plugins",
        "<turn_aborted",
        "<user_shell_command",
    ]
    .iter()
    .any(|prefix| text.starts_with(prefix))
}

fn search_score(text: &str, query: &str) -> Option<usize> {
    let text = text.to_lowercase();
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return Some(1);
    }
    let mut score = usize::from(text.contains(&query)) * 100;
    let mut matched = 0;
    for term in query
        .split(|character: char| {
            !character.is_alphanumeric() && character != '_' && character != '-'
        })
        .filter(|term| term.len() >= 2)
    {
        if text.contains(term) {
            matched += 1;
            score += 10;
        }
    }
    (matched > 0).then_some(score)
}

fn insert_finding(db: &Connection, run_id: &str, finding: FindingInput) -> Result<String> {
    let id = new_id("finding");
    let now = Utc::now().to_rfc3339();
    db.execute("INSERT INTO findings(id,title,summary,category,severity,confidence,status,origin,research_run_id,scope_json,product_lens_json,created_at,updated_at,reviewed_by,review_note) VALUES (?1,?2,?3,?4,?5,?6,'proposed','autonomous_research',?7,?8,?9,?10,?10,NULL,NULL)", params![id,bounded(&finding.title,500),bounded(&finding.summary,4000),finding.category,finding.severity,finding.confidence,run_id,serde_json::to_string(&finding.scope)?,serde_json::to_string(&finding.product_lenses)?,now])?;
    for evidence in finding.evidence {
        let evidence_id = new_id("evidence");
        db.execute("INSERT INTO evidence_refs(id,owner_type,owner_id,source_kind,uri,title,excerpt,publisher,accessed_at,local_revision,confidence,is_primary,qualification) VALUES (?1,'finding',?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)", params![evidence_id,id,evidence.source_kind,evidence.uri,bounded(&evidence.title,500),bounded(&evidence.excerpt,2000),evidence.publisher,now,evidence.local_revision,evidence.confidence,evidence.is_primary,evidence.qualification])?;
    }
    Ok(id)
}

fn validate_finding(finding: &FindingInput) -> Result<()> {
    ensure!(
        !finding.title.trim().is_empty() && !finding.summary.trim().is_empty(),
        "finding title and summary are required"
    );
    ensure!(
        ["technical", "product", "design"].contains(&finding.category.as_str()),
        "finding category must be technical, product, or design"
    );
    ensure!(
        (0.0..=1.0).contains(&finding.confidence),
        "finding confidence must be 0..=1"
    );
    ensure!(
        !finding.evidence.is_empty(),
        "every finding requires evidence"
    );
    for evidence in &finding.evidence {
        ensure!(
            (0.0..=1.0).contains(&evidence.confidence),
            "evidence confidence must be 0..=1"
        );
        match evidence.source_kind.as_str() {
            "local_repository" => ensure!(
                !evidence.uri.starts_with("http"),
                "local evidence must use a repository path or revision URI"
            ),
            "web_primary" => {
                ensure!(
                    evidence.uri.starts_with("https://"),
                    "web evidence must use HTTPS"
                );
                ensure!(
                    evidence.is_primary,
                    "web_primary evidence must be marked primary"
                );
            }
            "web_secondary" => {
                ensure!(
                    evidence.uri.starts_with("https://"),
                    "web evidence must use HTTPS"
                );
                ensure!(
                    !evidence.qualification.trim().is_empty(),
                    "secondary evidence requires an explicit qualification"
                );
            }
            other => bail!("unsupported evidence source_kind {other}"),
        }
    }
    Ok(())
}

fn validate_budget(budget: &ResearchBudget) -> Result<()> {
    ensure!(
        budget.max_web_queries <= 20,
        "max_web_queries cannot exceed 20"
    );
    ensure!(
        budget.max_local_files > 0 && budget.max_local_files <= 500,
        "max_local_files must be 1..=500"
    );
    ensure!(
        budget.max_minutes > 0 && budget.max_minutes <= 60,
        "max_minutes must be 1..=60"
    );
    ensure!(
        budget.max_findings > 0 && budget.max_findings <= 50,
        "max_findings must be 1..=50"
    );
    Ok(())
}

fn fallback_exact_search(root: &Path, query: &str, limit: usize) -> Result<Vec<Value>> {
    let mut matches = Vec::new();
    for entry in WalkDir::new(root)
        .into_iter()
        .filter_entry(|entry| {
            !matches!(
                entry.file_name().to_str(),
                Some(".git" | "target" | ".rust-repo-intelligence")
            )
        })
        .filter_map(Result::ok)
    {
        if entry.path().extension().and_then(|value| value.to_str()) != Some("rs") {
            continue;
        }
        let Ok(text) = fs::read_to_string(entry.path()) else {
            continue;
        };
        for (line_index, line) in text.lines().enumerate() {
            if let Some(column) = line.find(query) {
                matches.push(json!({"file":entry.path().strip_prefix(root).unwrap_or(entry.path()),"line":line_index+1,"column":column+1,"source":line}));
                if matches.len() >= limit {
                    return Ok(matches);
                }
            }
        }
    }
    Ok(matches)
}

fn collect_research_signals(root: &Path, limit: usize) -> Result<Vec<Value>> {
    let mut signals = Vec::new();
    for entry in WalkDir::new(root)
        .into_iter()
        .filter_entry(|entry| {
            !matches!(
                entry.file_name().to_str(),
                Some(".git" | "target" | ".rust-repo-intelligence")
            )
        })
        .filter_map(std::result::Result::ok)
    {
        if signals.len() >= limit {
            break;
        }
        if !entry.file_type().is_file() {
            continue;
        }
        let extension = entry.path().extension().and_then(|value| value.to_str());
        if !matches!(extension, Some("rs" | "md" | "toml")) {
            continue;
        }
        let metadata = entry.metadata()?;
        if metadata.len() > 256_000 {
            continue;
        }
        let Ok(text) = fs::read_to_string(entry.path()) else {
            continue;
        };
        let terms: &[&str] = match extension {
            Some("rs") => &["todo", "fixme", "unsafe", "unwrap(", "expect("],
            Some("md") => &[
                "roadmap",
                "limitation",
                "accessibility",
                "user experience",
                "product",
                "design",
                "latency",
            ],
            Some("toml") => &["deprecated", "workspace", "feature"],
            _ => &[],
        };
        let matched = text.lines().enumerate().find(|(_, line)| {
            let line = line.to_lowercase();
            terms.iter().any(|term| line.contains(term))
        });
        let Some((line_index, line)) = matched else {
            continue;
        };
        let lowercase = line.to_lowercase();
        let category = if extension == Some("rs") {
            "technical"
        } else if lowercase.contains("design") || lowercase.contains("accessibility") {
            "design"
        } else {
            "product"
        };
        signals.push(json!({
            "file": entry.path().strip_prefix(root).unwrap_or(entry.path()),
            "line": line_index + 1,
            "category_hint": category,
            "source": bounded(line.trim(), 500),
            "provenance": "live local repository heuristic; inspect source before treating it as a finding"
        }));
    }
    Ok(signals)
}

fn finding_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    Ok(
        json!({"id":row.get::<_,String>(0)?,"title":row.get::<_,String>(1)?,"summary":row.get::<_,String>(2)?,"category":row.get::<_,String>(3)?,"severity":row.get::<_,String>(4)?,"confidence":row.get::<_,f64>(5)?,"status":row.get::<_,String>(6)?,"origin":row.get::<_,String>(7)?,"research_run_id":row.get::<_,Option<String>>(8)?,"scope":json_from(&row.get::<_,String>(9)?),"product_lenses":json_from(&row.get::<_,String>(10)?),"created_at":row.get::<_,String>(11)?,"updated_at":row.get::<_,String>(12)?,"reviewed_by":row.get::<_,Option<String>>(13)?,"review_note":row.get::<_,Option<String>>(14)?}),
    )
}

fn evidence_for(db: &Connection, owner_type: &str, owner_id: &str) -> Result<Vec<Value>> {
    let mut statement = db.prepare("SELECT id,source_kind,uri,title,excerpt,publisher,accessed_at,local_revision,confidence,is_primary,qualification FROM evidence_refs WHERE owner_type=?1 AND owner_id=?2 ORDER BY accessed_at")?;
    Ok(statement.query_map(params![owner_type,owner_id], |row| Ok(json!({"id":row.get::<_,String>(0)?,"source_kind":row.get::<_,String>(1)?,"uri":row.get::<_,String>(2)?,"title":row.get::<_,String>(3)?,"excerpt":row.get::<_,String>(4)?,"publisher":row.get::<_,Option<String>>(5)?,"accessed_at":row.get::<_,String>(6)?,"local_revision":row.get::<_,Option<String>>(7)?,"confidence":row.get::<_,f64>(8)?,"is_primary":row.get::<_,bool>(9)?,"qualification":row.get::<_,String>(10)?})))?.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn work_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    Ok(
        json!({"id":row.get::<_,String>(0)?,"title":row.get::<_,String>(1)?,"status":row.get::<_,String>(2)?,"priority":row.get::<_,String>(3)?,"kind":row.get::<_,String>(4)?,"scope":json_from(&row.get::<_,String>(5)?),"evidence":json_from(&row.get::<_,String>(6)?),"depends_on":json_from(&row.get::<_,String>(7)?),"blocked_by":json_from(&row.get::<_,String>(8)?),"acceptance_criteria":json_from(&row.get::<_,String>(9)?),"verification":json_from(&row.get::<_,String>(10)?),"discovered_from":row.get::<_,String>(11)?,"provenance":row.get::<_,String>(12)?,"confidence":row.get::<_,f64>(13)?,"last_validated_snapshot":row.get::<_,String>(14)?,"source_finding_id":row.get::<_,Option<String>>(15)?,"human_owned":row.get::<_,bool>(16)?,"updated_at":row.get::<_,String>(17)?}),
    )
}

fn normalize_work_links(values: Vec<String>, field: &str) -> Result<Vec<String>> {
    let mut normalized = Vec::with_capacity(values.len());
    let mut seen = HashSet::new();
    for value in values {
        let work_id = value.trim();
        ensure!(!work_id.is_empty(), "{field} cannot contain an empty id");
        ensure!(
            work_id.starts_with("work_"),
            "{field} must contain exact work item ids"
        );
        ensure!(
            seen.insert(work_id.to_owned()),
            "{field} contains duplicate work item {work_id}"
        );
        normalized.push(work_id.to_owned());
    }
    Ok(normalized)
}

fn work_links_from_json(value: &str) -> Vec<String> {
    serde_json::from_str(value).unwrap_or_default()
}

fn validate_work_relationships(
    db: &Connection,
    work_id: Option<&str>,
    depends_on: &[String],
    blocked_by: &[String],
) -> Result<()> {
    let mut relationships = HashSet::new();
    for (field, work_ids) in [("depends_on", depends_on), ("blocked_by", blocked_by)] {
        for related_id in work_ids {
            ensure!(
                work_id != Some(related_id.as_str()),
                "work item cannot reference itself in {field}"
            );
            ensure!(
                relationships.insert(related_id.as_str()),
                "work item {related_id} cannot appear in both depends_on and blocked_by"
            );
            let exists = db
                .query_row("SELECT 1 FROM work_items WHERE id=?1", [related_id], |_| {
                    Ok(())
                })
                .optional()?
                .is_some();
            ensure!(exists, "unknown work item {related_id} in {field}");
        }
    }
    if let Some(work_id) = work_id {
        for related_id in relationships {
            ensure!(
                !work_reaches(db, related_id, work_id)?,
                "relationship from {work_id} to {related_id} would create a cycle"
            );
        }
    }
    Ok(())
}

fn work_reaches(db: &Connection, start: &str, target: &str) -> Result<bool> {
    let mut pending = vec![start.to_owned()];
    let mut visited = HashSet::new();
    while let Some(work_id) = pending.pop() {
        if work_id == target {
            return Ok(true);
        }
        if !visited.insert(work_id.clone()) {
            continue;
        }
        let links = db
            .query_row(
                "SELECT depends_json,blocked_json FROM work_items WHERE id=?1",
                [&work_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        if let Some((depends_on, blocked_by)) = links {
            pending.extend(work_links_from_json(&depends_on));
            pending.extend(work_links_from_json(&blocked_by));
        }
    }
    Ok(false)
}

fn decorate_work_readiness(db: &Connection, item: &mut Value) -> Result<()> {
    let unresolved_dependencies = unresolved_work_links(db, item, "depends_on")?;
    let unresolved_blockers = unresolved_work_links(db, item, "blocked_by")?;
    let ready = unresolved_dependencies.is_empty() && unresolved_blockers.is_empty();
    let object = item
        .as_object_mut()
        .context("work item row must be a JSON object")?;
    object.insert(
        "unresolved_dependencies".into(),
        json!(unresolved_dependencies),
    );
    object.insert("unresolved_blockers".into(), json!(unresolved_blockers));
    object.insert("ready".into(), json!(ready));
    Ok(())
}

fn unresolved_work_links(db: &Connection, item: &Value, field: &str) -> Result<Vec<String>> {
    let mut unresolved = Vec::new();
    for related_id in item[field]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        let status = db
            .query_row(
                "SELECT status FROM work_items WHERE id=?1",
                [related_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if !status.as_deref().is_some_and(work_status_is_complete) {
            unresolved.push(related_id.to_owned());
        }
    }
    Ok(unresolved)
}

fn work_status_is_complete(status: &str) -> bool {
    matches!(
        status.trim().to_ascii_lowercase().as_str(),
        "complete" | "completed"
    )
}

fn task_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    let result: Option<String> = row.get(5)?;
    Ok(
        json!({"id":row.get::<_,String>(0)?,"kind":row.get::<_,String>(1)?,"status":row.get::<_,String>(2)?,"progress":row.get::<_,i64>(3)?,"message":row.get::<_,String>(4)?,"result":result.as_deref().map(json_from),"error":row.get::<_,Option<String>>(6)?,"cancel_requested":row.get::<_,bool>(7)?,"created_at":row.get::<_,String>(8)?,"updated_at":row.get::<_,String>(9)?}),
    )
}

fn latest_task(db: &Connection, kind: &str) -> Result<Option<Value>> {
    Ok(db.query_row("SELECT id,kind,status,progress,message,result_json,error,cancel_requested,created_at,updated_at FROM tasks WHERE kind=?1 ORDER BY updated_at DESC LIMIT 1", [kind], task_row).optional()?)
}

fn metadata_value(db: &Connection, key: &str) -> Result<Option<String>> {
    Ok(db
        .query_row("SELECT value FROM metadata WHERE key=?1", [key], |row| {
            row.get(0)
        })
        .optional()?)
}

fn git_text(root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn new_id(prefix: &str) -> String {
    let nonce: [u8; 16] = random();
    let mut hasher = blake3::Hasher::new();
    hasher.update(prefix.as_bytes());
    hasher.update(Utc::now().to_rfc3339().as_bytes());
    hasher.update(&nonce);
    format!("{prefix}_{}", &hasher.finalize().to_hex()[..16])
}

fn json_from(value: &str) -> Value {
    serde_json::from_str(value).unwrap_or_else(|_| json!(value))
}

fn bounded(value: &str, maximum: usize) -> String {
    if value.len() <= maximum {
        return value.to_owned();
    }
    let mut end = maximum;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &value[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs2::FileExt;
    use std::fs::OpenOptions;
    use tempfile::tempdir;

    fn workspace() -> Result<(tempfile::TempDir, Observatory)> {
        let directory = tempdir()?;
        fs::write(
            directory.path().join("Cargo.toml"),
            "[package]\nname='fixture'\nversion='0.1.0'\nedition='2024'\n",
        )?;
        fs::create_dir(directory.path().join("src"))?;
        fs::write(directory.path().join("src/lib.rs"), "pub fn indexed() {}\n")?;
        let observatory = Observatory::open(directory.path())?;
        Ok((directory, observatory))
    }

    fn work_request(title: &str, status: &str) -> WorkCreateRequest {
        WorkCreateRequest {
            title: title.into(),
            confirm_human: true,
            status: status.into(),
            priority: "normal".into(),
            kind: "technical".into(),
            scope: vec![],
            evidence: vec![],
            depends_on: vec![],
            blocked_by: vec![],
            acceptance_criteria: vec![],
            verification: vec![],
        }
    }

    #[test]
    fn prompt_recovery_is_project_scoped_and_includes_side_session_history() -> Result<()> {
        let (repository, _observatory) = workspace()?;
        let codex = tempdir()?;
        let sessions = codex.path().join("sessions/2026/08/24");
        fs::create_dir_all(&sessions)?;
        let thread_id = "thread-project";
        let metadata = json!({
            "type": "session_meta",
            "payload": {
                "id": thread_id,
                "cwd": repository.path(),
                "history_base": {"thread_id": thread_id}
            }
        });
        fs::write(
            sessions.join("rollout-test_01-side.jsonl"),
            format!("{}\n", serde_json::to_string(&metadata)?),
        )?;

        let history = Connection::open(codex.path().join("thread_history_1.sqlite"))?;
        history.execute_batch(
            "CREATE TABLE thread_items(
                thread_id TEXT NOT NULL, turn_id TEXT NOT NULL, item_id TEXT NOT NULL,
                rollout_ordinal INTEGER NOT NULL, created_at_ms INTEGER NOT NULL,
                item_json TEXT NOT NULL, item_type TEXT NOT NULL DEFAULT '',
                updated_at_ordinal INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY(thread_id,turn_id,item_id));",
        )?;
        let prompt = json!({
            "type": "userMessage",
            "content": [
                {"type":"localImage","path":"/tmp/reference.png"},
                {"type":"text","text":"Hide secondary Evidence and Activity behind toggles."}
            ]
        });
        history.execute(
            "INSERT INTO thread_items VALUES (?1,'turn-1','item-1',9,1787520000000,?2,'userMessage',0)",
            params![thread_id, serde_json::to_string(&prompt)?],
        )?;
        let unrelated = json!({
            "type": "userMessage",
            "content": [{"type":"text","text":"Hide secondary content in another project."}]
        });
        history.execute(
            "INSERT INTO thread_items VALUES ('thread-other','turn-2','item-2',9,1787520000001,?1,'userMessage',0)",
            [serde_json::to_string(&unrelated)?],
        )?;
        let injected = json!({
            "type": "userMessage",
            "content": [{"type":"text","text":"# AGENTS.md instructions\nHide secondary Evidence."}]
        });
        history.execute(
            "INSERT INTO thread_items VALUES (?1,'turn-3','item-3',10,1787520000002,?2,'userMessage',0)",
            params![thread_id, serde_json::to_string(&injected)?],
        )?;
        drop(history);

        let (prompts, provenance) = search_codex_prompt_history(
            repository.path(),
            "secondary evidence activity",
            10,
            2_000,
            Some(codex.path().to_path_buf()),
        )?;
        assert_eq!(prompts.len(), 1);
        assert_eq!(
            prompts[0]["text"],
            "Hide secondary Evidence and Activity behind toggles."
        );
        assert_eq!(prompts[0]["images"][0], "/tmp/reference.png");
        assert_eq!(provenance["source"], "thread_history_1.sqlite");
        assert_eq!(provenance["side_session_files"], 1);
        Ok(())
    }

    #[test]
    fn later_legacy_decisions_remain_visible_after_the_initial_migration() -> Result<()> {
        let directory = tempdir()?;
        fs::write(
            directory.path().join("Cargo.toml"),
            "[package]\nname='fixture'\nversion='0.1.0'\nedition='2024'\n",
        )?;
        fs::create_dir(directory.path().join("src"))?;
        fs::write(directory.path().join("src/lib.rs"), "")?;
        let state = directory.path().join(STATE_DIRECTORY);
        fs::create_dir(&state)?;
        let legacy_path = state.join("index.sqlite3");
        let legacy = Connection::open(&legacy_path)?;
        legacy.execute_batch(
            "CREATE TABLE decisions(
                id TEXT PRIMARY KEY, sequence INTEGER, status TEXT, title TEXT,
                rationale TEXT, applies_to TEXT, consequences TEXT,
                supersedes TEXT, revision TEXT, created_at TEXT);
             INSERT INTO decisions VALUES(
                'DEC-1',1,'accepted','First','why','[]','[]',NULL,'r1','now');",
        )?;
        drop(legacy);
        drop(Observatory::open(directory.path())?);

        let legacy = Connection::open(&legacy_path)?;
        legacy.execute_batch(
            "INSERT INTO decisions VALUES(
                'DEC-2',2,'accepted','Secondary information','progressive disclosure',
                '[\"work inspector\"]','[\"collapse evidence\"]',NULL,'r2','later');",
        )?;
        drop(legacy);

        let observatory = Observatory::open(directory.path())?;
        let records = observatory.search_legacy_memory("collapse evidence", 10)?;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["id"], "DEC-2");
        Ok(())
    }

    #[tokio::test]
    async fn exact_search_reads_dirty_worktree_without_refresh() -> Result<()> {
        let (directory, observatory) = workspace()?;
        fs::write(
            directory.path().join("src/lib.rs"),
            "pub fn just_written_signal() {}\n",
        )?;
        let result = observatory
            .search(SearchRequest {
                query: "just_written_signal".into(),
                mode: "exact".into(),
                limit: 10,
                include_source: true,
            })
            .await?;
        assert_eq!(result["matches"].as_array().map(Vec::len), Some(1));
        assert_eq!(result["authority"], "live_worktree");
        Ok(())
    }

    #[tokio::test]
    async fn broad_search_exposes_hybrid_retrieval_provenance() -> Result<()> {
        let (directory, observatory) = workspace()?;
        let mut service = Service::open(directory.path())?;
        service.refresh_if_stale()?;

        let result = observatory
            .search(SearchRequest {
                query: "indexed".into(),
                mode: "broad".into(),
                limit: 10,
                include_source: false,
            })
            .await?;
        assert_eq!(
            result["result"]["retrieval_provenance"]["embedding"]["card_version"],
            "symbol-card-v1"
        );
        assert_eq!(
            result["result"]["canonical_candidates"][0]["exact_match"],
            true
        );
        assert!(result["result"]["canonical_candidates"][0]["embedding_evidence"].is_object());
        Ok(())
    }

    #[test]
    fn exact_work_lookup_and_recommendation_share_one_store() -> Result<()> {
        let (_directory, observatory) = workspace()?;
        let mut request = work_request("Unique queue repair", "accepted");
        request.priority = "high".into();
        request.scope = vec!["src/lib.rs".into()];
        let created = observatory.work_create(request)?;
        let id = created["work_id"].as_str().unwrap();
        assert_eq!(
            observatory.work_list(Some(id), 10)?["items"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            observatory.work_recommend(Some(id))?["recommendation"]["id"],
            id
        );
        Ok(())
    }

    #[test]
    fn active_work_remains_recommendable() -> Result<()> {
        let (_directory, observatory) = workspace()?;
        let mut request = work_request("Continue the active repair", "active");
        request.priority = "critical".into();
        request.scope = vec!["src/lib.rs".into()];
        let created = observatory.work_create(request)?;
        let id = created["work_id"].as_str().unwrap();
        assert_eq!(
            observatory.work_recommend(Some(id))?["recommendation"]["id"],
            id
        );
        Ok(())
    }

    #[test]
    fn work_dependencies_are_writable_and_gate_recommendations() -> Result<()> {
        let (_directory, observatory) = workspace()?;
        let prerequisite = observatory.work_create(work_request("Prerequisite", "accepted"))?;
        let prerequisite_id = prerequisite["work_id"].as_str().unwrap().to_owned();

        let mut candidate_request = work_request("Candidate", "accepted");
        candidate_request.depends_on = vec![prerequisite_id.clone()];
        let candidate = observatory.work_create(candidate_request)?;
        let candidate_id = candidate["work_id"].as_str().unwrap().to_owned();
        let pending = observatory.work_get(&candidate_id)?;
        assert_eq!(
            pending["work"]["depends_on"],
            json!([prerequisite_id.clone()])
        );
        assert_eq!(
            pending["work"]["unresolved_dependencies"],
            pending["work"]["depends_on"]
        );
        assert_eq!(pending["work"]["ready"], false);
        assert!(observatory.work_recommend(Some(&candidate_id))?["recommendation"].is_null());

        observatory.work_update(WorkUpdateRequest {
            work_id: prerequisite_id.clone(),
            confirm_human: true,
            status: Some("completed".into()),
            priority: None,
            title: None,
            depends_on: None,
            blocked_by: None,
        })?;
        assert_eq!(
            observatory.work_recommend(Some(&candidate_id))?["recommendation"]["id"],
            candidate_id
        );

        let blocker = observatory.work_create(work_request("Blocker", "active"))?;
        let blocker_id = blocker["work_id"].as_str().unwrap().to_owned();
        let blocked = observatory.work_update(WorkUpdateRequest {
            work_id: candidate_id.clone(),
            confirm_human: true,
            status: None,
            priority: None,
            title: None,
            depends_on: None,
            blocked_by: Some(vec![blocker_id.clone()]),
        })?;
        assert_eq!(blocked["work"]["depends_on"], json!([prerequisite_id]));
        assert_eq!(blocked["work"]["unresolved_blockers"], json!([blocker_id]));
        assert_eq!(blocked["work"]["ready"], false);

        let cleared = observatory.work_update(WorkUpdateRequest {
            work_id: candidate_id,
            confirm_human: true,
            status: None,
            priority: None,
            title: None,
            depends_on: None,
            blocked_by: Some(vec![]),
        })?;
        assert_eq!(cleared["work"]["blocked_by"], json!([]));
        assert_eq!(cleared["work"]["ready"], true);
        Ok(())
    }

    #[test]
    fn work_relationships_reject_unknown_self_duplicate_and_cyclic_links() -> Result<()> {
        let (_directory, observatory) = workspace()?;
        let mut unknown = work_request("Unknown dependency", "accepted");
        unknown.depends_on = vec!["work_missing".into()];
        assert!(observatory.work_create(unknown).is_err());

        let first = observatory.work_create(work_request("First", "accepted"))?;
        let first_id = first["work_id"].as_str().unwrap().to_owned();
        let mut second_request = work_request("Second", "accepted");
        second_request.depends_on = vec![first_id.clone()];
        let second = observatory.work_create(second_request)?;
        let second_id = second["work_id"].as_str().unwrap().to_owned();

        let update = |depends_on| WorkUpdateRequest {
            work_id: first_id.clone(),
            confirm_human: true,
            status: None,
            priority: None,
            title: None,
            depends_on: Some(depends_on),
            blocked_by: Some(vec![]),
        };
        assert!(
            observatory
                .work_update(update(vec![first_id.clone()]))
                .is_err()
        );
        assert!(
            observatory
                .work_update(update(vec![second_id.clone(), second_id.clone()]))
                .is_err()
        );
        assert!(observatory.work_update(update(vec![second_id])).is_err());
        Ok(())
    }

    #[test]
    fn research_finding_requires_review_before_promotion() -> Result<()> {
        let (_directory, observatory) = workspace()?;
        let db = observatory.db()?;
        let finding = FindingInput {
            title: "Potential".into(),
            summary: "Evidence-backed potential".into(),
            category: "product".into(),
            severity: "normal".into(),
            confidence: 0.8,
            scope: vec![],
            product_lenses: vec![],
            evidence: vec![EvidenceInput {
                source_kind: "local_repository".into(),
                uri: "src/lib.rs:1".into(),
                title: "Source".into(),
                excerpt: "signal".into(),
                publisher: None,
                local_revision: None,
                confidence: 1.0,
                is_primary: true,
                qualification: String::new(),
            }],
        };
        let id = insert_finding(&db, "test_run", finding)?;
        let request = PromoteFindingRequest {
            finding_id: id.clone(),
            reviewed_by: "human".into(),
            confirm_human: true,
            priority: "normal".into(),
            kind: "product".into(),
            acceptance_criteria: vec![],
            verification: vec![],
        };
        assert!(observatory.finding_promote(request.clone()).is_err());
        observatory.finding_review(ReviewFindingRequest {
            finding_id: id,
            decision: "accepted".into(),
            reviewed_by: "human".into(),
            note: String::new(),
        })?;
        assert!(observatory.finding_promote(request).is_ok());
        Ok(())
    }

    #[test]
    fn first_open_migrates_legacy_work_without_mutating_the_index() -> Result<()> {
        let directory = tempdir()?;
        fs::write(
            directory.path().join("Cargo.toml"),
            "[package]\nname='fixture'\nversion='0.1.0'\nedition='2024'\n",
        )?;
        fs::create_dir(directory.path().join("src"))?;
        fs::write(directory.path().join("src/lib.rs"), "")?;
        let state = directory.path().join(STATE_DIRECTORY);
        fs::create_dir(&state)?;
        let legacy_path = state.join("index.sqlite3");
        let legacy = Connection::open(&legacy_path)?;
        legacy.execute_batch(
            "CREATE TABLE work_items(id TEXT PRIMARY KEY,title TEXT,status TEXT,priority TEXT,kind TEXT,scope_json TEXT,evidence_json TEXT,depends_json TEXT,blocked_json TEXT,acceptance_json TEXT,verification_json TEXT,discovered_from TEXT,provenance TEXT,confidence REAL,last_validated_snapshot TEXT,created_at TEXT,updated_at TEXT);
             INSERT INTO work_items VALUES ('work_legacy','Preserve me','accepted','normal','technical','[]','[]','[]','[]','[]','[]','legacy','HumanDecision',1.0,'snapshot','2026-01-01','2026-01-01');",
        )?;
        drop(legacy);
        let before = fs::metadata(&legacy_path)?.len();
        let observatory = Observatory::open(directory.path())?;
        assert_eq!(
            observatory.work_get("work_legacy")?["work"]["title"],
            "Preserve me"
        );
        assert_eq!(fs::metadata(&legacy_path)?.len(), before);
        Ok(())
    }

    #[test]
    fn publisher_lease_rejects_a_second_writer() -> Result<()> {
        let (_directory, observatory) = workspace()?;
        let lock_path = observatory.root.join(STATE_DIRECTORY).join("index.lock");
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path)?;
        FileExt::try_lock_exclusive(&lock)?;
        let task_id = observatory.create_task("index.refresh", "test")?;
        let error = observatory.run_refresh(&task_id, None).unwrap_err();
        assert!(format!("{error:#}").contains("publisher lease"));
        FileExt::unlock(&lock)?;
        Ok(())
    }

    #[tokio::test]
    async fn broad_context_tolerates_source_shorter_than_its_published_snapshot() -> Result<()> {
        let directory = tempdir()?;
        fs::write(
            directory.path().join("Cargo.toml"),
            "[package]\nname='fixture'\nversion='0.1.0'\nedition='2024'\n",
        )?;
        fs::create_dir(directory.path().join("src"))?;
        let mut source = "// padding\n".repeat(800);
        source.push_str("pub fn stale_source_symbol() -> usize { 7 }\n");
        fs::write(directory.path().join("src/lib.rs"), source)?;
        let mut indexer = Service::open(directory.path())?;
        indexer.refresh(None)?;
        drop(indexer);
        fs::write(
            directory.path().join("src/lib.rs"),
            "pub fn stale_source_symbol() -> usize { 7 }\n",
        )?;
        let observatory = Observatory::open(directory.path())?;
        let result = observatory
            .context(ContextRequest {
                query: "stale_source_symbol".into(),
                budget: 1_000,
                limit: 8,
            })
            .await;
        assert!(
            result.is_ok(),
            "stale snapshot should degrade, not panic: {result:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn research_handshake_produces_reviewable_findings_without_work() -> Result<()> {
        let (_directory, observatory) = workspace()?;
        let started = observatory.research_start(ResearchStartRequest {
            topic: "repository observatory usability".into(),
            goals: vec!["Find one evidence-backed design improvement".into()],
            budget: ResearchBudget {
                max_web_queries: 2,
                max_local_files: 8,
                max_minutes: 5,
                max_findings: 2,
            },
            schedule: None,
        })?;
        let task_id = started["task_id"].as_str().unwrap().to_owned();
        for _ in 0..100 {
            if observatory.task_get(&task_id)?["task"]["status"] == "completed" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(
            observatory.task_get(&task_id)?["task"]["status"],
            "completed"
        );
        let run_id = started["run_id"].as_str().unwrap().to_owned();
        assert_eq!(
            observatory.research_packet(&run_id)?["status"],
            "awaiting_agent"
        );
        let submitted = observatory.research_submit(ResearchSubmitRequest {
            run_id,
            web_queries_used: 1,
            notes: "Attached-agent web search complete".into(),
            findings: vec![FindingInput {
                title: "Clarify review status at a glance".into(),
                summary: "A stronger status treatment may reduce finding-review effort.".into(),
                category: "design".into(),
                severity: "normal".into(),
                confidence: 0.8,
                scope: vec!["dashboard".into()],
                product_lenses: vec!["usability".into()],
                evidence: vec![EvidenceInput {
                    source_kind: "web_primary".into(),
                    uri: "https://www.w3.org/WAI/WCAG22/".into(),
                    title: "WCAG 2.2".into(),
                    excerpt: "Primary accessibility standard considered by the attached agent."
                        .into(),
                    publisher: Some("W3C".into()),
                    local_revision: None,
                    confidence: 0.9,
                    is_primary: true,
                    qualification: String::new(),
                }],
            }],
        })?;
        assert_eq!(submitted["status"], "completed");
        assert_eq!(
            observatory.finding_list(Some("proposed"), None, 10)?["findings"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert!(
            observatory.work_list(None, 10)?["items"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        Ok(())
    }

    #[tokio::test]
    async fn prepare_and_validate_acknowledge_as_tasks_without_refresh() -> Result<()> {
        let (directory, observatory) = workspace()?;
        let mut indexer = Service::open(directory.path())?;
        indexer.refresh(None)?;
        drop(indexer);
        let index_path = directory.path().join(STATE_DIRECTORY).join("index.sqlite3");
        let generation_before: i64 = Connection::open(&index_path)?.query_row(
            "SELECT MAX(id) FROM index_generations",
            [],
            |row| row.get(0),
        )?;

        let started_at = std::time::Instant::now();
        let preparation = observatory.start_prepare_change(PrepareRequest {
            intent: "Change indexed fixture".into(),
            targets: vec!["src::lib::indexed".into()],
            depth: 1,
            budget: Some(1_000),
        })?;
        assert!(started_at.elapsed() < std::time::Duration::from_millis(250));
        let prepare_task = preparation["task_id"].as_str().unwrap().to_owned();
        let prepared = wait_for_task(&observatory, &prepare_task).await?;
        assert_eq!(prepared["status"], "completed", "{prepared}");
        let context_id = prepared["result"]["result"]["context_id"]
            .as_str()
            .expect("prepared context id")
            .to_owned();

        let started_at = std::time::Instant::now();
        let validation = observatory.start_validate_change(ValidateRequest {
            context_id,
            git_diff: Some(String::new()),
            run_checks: false,
        })?;
        assert!(started_at.elapsed() < std::time::Duration::from_millis(250));
        let validate_task = validation["task_id"].as_str().unwrap().to_owned();
        let validated = wait_for_task(&observatory, &validate_task).await?;
        assert_eq!(validated["status"], "completed", "{validated}");

        let generation_after: i64 = Connection::open(&index_path)?.query_row(
            "SELECT MAX(id) FROM index_generations",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(generation_after, generation_before);
        Ok(())
    }

    async fn wait_for_task(observatory: &Observatory, id: &str) -> Result<Value> {
        for _ in 0..200 {
            let task = observatory.task_get(id)?["task"].clone();
            if matches!(task["status"].as_str(), Some("completed" | "failed")) {
                return Ok(task);
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        bail!("task {id} did not finish")
    }
}

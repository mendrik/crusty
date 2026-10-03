//! Leased ownership of change surfaces, shared by all linked Git worktrees.
//!
//! Claims are advisory to external editors but exclusive within Crusty's
//! workflow. An immediate SQLite transaction fences acquisition against other
//! processes. Expiry is terminal: a resumed agent registers a new session.

use crate::execution::ExecutionControl;
use anyhow::{Context, Result, bail, ensure};
use chrono::Utc;
use fs2::FileExt;
use rand::random;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    fs,
    io::Read,
    path::{Component, Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

const SCHEMA: &str = r#"
PRAGMA journal_mode=WAL;
PRAGMA foreign_keys=ON;
CREATE TABLE IF NOT EXISTS coordination_schema(version INTEGER NOT NULL);
INSERT INTO coordination_schema SELECT 1 WHERE NOT EXISTS(SELECT 1 FROM coordination_schema);
CREATE TABLE IF NOT EXISTS coding_sessions(
 id TEXT PRIMARY KEY, token_hash TEXT NOT NULL, owner TEXT NOT NULL,
 intent TEXT NOT NULL, work_ids TEXT NOT NULL, worktree TEXT NOT NULL,
 branch TEXT, base_head TEXT, status TEXT NOT NULL CHECK(status IN ('active','closed','expired')),
 created_at INTEGER NOT NULL, heartbeat_at INTEGER NOT NULL, expires_at INTEGER NOT NULL,
 summary TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS active_coding_sessions ON coding_sessions(status,expires_at);
CREATE TABLE IF NOT EXISTS path_claims(
 session_id TEXT NOT NULL REFERENCES coding_sessions(id) ON DELETE CASCADE,
 path TEXT NOT NULL, PRIMARY KEY(session_id,path)
);
CREATE TABLE IF NOT EXISTS commit_plans(
 id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES coding_sessions(id),
 payload TEXT NOT NULL, created_at INTEGER NOT NULL
);
"#;

fn default_ttl() -> u64 {
    600
}

/// Owner of the ephemeral session `commit.plan` registers for single-agent
/// work. The `crusty:` prefix is reserved, so no agent can register it.
pub(crate) const IMPLICIT_OWNER: &str = "crusty:implicit-commit";
const IMPLICIT_TTL_SECONDS: i64 = 600;

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionStartRequest {
    pub owner: String,
    pub intent: String,
    /// Human-owned work IDs related to this session; registration creates no work.
    #[serde(default)]
    pub work_ids: Vec<String>,
    /// Create a fresh branch/worktree at HEAD, preserving the caller's dirty work.
    #[serde(default)]
    pub isolate: bool,
    /// Renew with session.heartbeat; expiry is terminal. Range: 30..=3600 seconds.
    #[serde(default = "default_ttl")]
    pub ttl_seconds: u64,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionAuth {
    pub session_id: String,
    pub lease_token: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HeartbeatRequest {
    pub session_id: String,
    pub lease_token: String,
    #[serde(default = "default_ttl")]
    pub ttl_seconds: u64,
    /// Current activity, surfaced to other sessions. Omission preserves it.
    pub summary: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ClaimRequest {
    pub session_id: String,
    pub lease_token: String,
    /// Repository-relative files/subtrees. `.` means the whole repository.
    /// Replaces this session's claims atomically; an empty list releases them.
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionListRequest {
    #[serde(default)]
    pub include_inactive: bool,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CommitGroup {
    pub message: String,
    /// Files/subtrees to expand against live changed paths. No implicit `git add -A`.
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CommitPlanRequest {
    /// Omit with lease_token for single-agent work: when no other coding
    /// session is active in the repository, an implicit session owns exactly
    /// the planned paths until commit.execute (or its lease) ends it.
    pub session_id: Option<String>,
    pub lease_token: Option<String>,
    pub groups: Vec<CommitGroup>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Active,
    Closed,
    Expired,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodingSession {
    pub id: String,
    pub owner: String,
    pub intent: String,
    pub work_ids: Vec<String>,
    pub worktree: PathBuf,
    pub branch: Option<String>,
    pub base_head: Option<String>,
    pub status: SessionStatus,
    pub created_at: i64,
    pub heartbeat_at: i64,
    pub expires_at: i64,
    pub summary: String,
    pub claims: Vec<String>,
}

pub(crate) struct Coordinator {
    pub(crate) root: PathBuf,
    pub(crate) state: PathBuf,
    pub(crate) control: ExecutionControl,
}

impl Coordinator {
    pub(crate) fn state_path(root: &Path, control: &ExecutionControl) -> Result<PathBuf> {
        let common = control.output(Command::new("git").current_dir(root).args([
            "rev-parse",
            "--path-format=absolute",
            "--git-common-dir",
        ]))?;
        let state = if common.status.success() {
            let common = PathBuf::from(String::from_utf8(common.stdout)?.trim());
            fs::canonicalize(common)?.join("crusty")
        } else {
            // A non-Git Rust project still supports ownership coordination.
            root.join(".rust-repo-intelligence").join("coordination")
        };
        Ok(state)
    }

    pub(crate) fn open(root: &Path, control: ExecutionControl) -> Result<Self> {
        let root = fs::canonicalize(root)?;
        let state = Self::state_path(&root, &control)?;
        fs::create_dir_all(&state)?;
        let this = Self {
            root,
            state,
            control,
        };
        let lock = this.lock("schema.lock")?;
        let db = this.db()?;
        let exists: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='coordination_schema')",
            [], |row|row.get(0),
        )?;
        if exists {
            let version: i64 =
                db.query_row("SELECT version FROM coordination_schema", [], |row| {
                    row.get(0)
                })?;
            ensure!(
                version == 1,
                "unsupported coordination schema {version}; no schema changes were made"
            );
        }
        db.execute_batch(SCHEMA)?;
        let version: i64 = db.query_row("SELECT version FROM coordination_schema", [], |row| {
            row.get(0)
        })?;
        ensure!(version == 1, "unsupported coordination schema {version}");
        drop(lock);
        Ok(this)
    }

    pub(crate) fn db(&self) -> Result<Connection> {
        let db = Connection::open(self.state.join("coordination.sqlite3"))?;
        db.busy_timeout(Duration::from_secs(5))?;
        db.execute_batch("PRAGMA foreign_keys=ON")?;
        Ok(db)
    }

    /// Try rather than indefinitely block a worker behind a crashed operation.
    pub(crate) fn lock(&self, name: &str) -> Result<fs::File> {
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.state.join(name))?;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            self.control.check()?;
            match FileExt::try_lock_exclusive(&file) {
                Ok(()) => return Ok(file),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    ensure!(
                        Instant::now() < deadline,
                        "another coordination operation is active; retry after it settles"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    pub(crate) fn start(&self, request: SessionStartRequest) -> Result<Value> {
        self.start_at(request, Utc::now().timestamp())
    }

    fn start_at(&self, mut request: SessionStartRequest, now: i64) -> Result<Value> {
        validate_text(&request.owner, "owner", 200)?;
        ensure!(
            !request.owner.starts_with("crusty:"),
            "owner names starting with `crusty:` are reserved"
        );
        validate_text(&request.intent, "intent", 16_000)?;
        validate_ttl(request.ttl_seconds)?;
        ensure!(request.work_ids.len() <= 100, "at most 100 work IDs");
        for id in &request.work_ids {
            validate_text(id, "work ID", 200)?;
        }
        request.work_ids.sort();
        request.work_ids.dedup();
        let id = format!("session_{:032x}", random::<u128>());
        let token = format!("{:032x}{:032x}", random::<u128>(), random::<u128>());
        let base_head = self.git_optional(&["rev-parse", "--verify", "HEAD"])?;
        let (worktree, branch) = if request.isolate {
            let _guard = self.lock("git-mutation.lock")?;
            let head = base_head
                .as_deref()
                .context("isolating a session requires a Git repository with a commit")?;
            let parent = self.state.join("worktrees");
            fs::create_dir_all(&parent)?;
            let path = parent.join(&id);
            let branch = format!("crusty/{id}");
            self.git(&[
                "worktree",
                "add",
                "-b",
                &branch,
                path.to_str().context("non-UTF-8 worktree path")?,
                head,
            ])?;
            (path, Some(branch))
        } else {
            (
                self.root.clone(),
                self.git_optional(&["symbolic-ref", "--quiet", "--short", "HEAD"])?,
            )
        };
        let mut db = self.db()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        expire(&tx, now)?;
        tx.execute("INSERT INTO coding_sessions(id,token_hash,owner,intent,work_ids,worktree,branch,base_head,status,created_at,heartbeat_at,expires_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,'active',?9,?9,?10)",
            params![id, token_hash(&token), request.owner, request.intent, serde_json::to_string(&request.work_ids)?, worktree.to_str().context("non-UTF-8 worktree path")?, branch, base_head, now, now + request.ttl_seconds as i64])?;
        let session = session(&tx, &id, now)?;
        tx.commit()?;
        Ok(
            json!({"session":session,"lease_token":token,"shared_repository":self.state,
            "next":"Use this worktree, claim the intended paths, and heartbeat before the lease expires. Existing dirty changes were not copied into an isolated worktree."}),
        )
    }

    pub(crate) fn heartbeat(&self, request: HeartbeatRequest) -> Result<Value> {
        self.heartbeat_at(request, Utc::now().timestamp())
    }

    fn heartbeat_at(&self, request: HeartbeatRequest, now: i64) -> Result<Value> {
        validate_ttl(request.ttl_seconds)?;
        if let Some(summary) = &request.summary {
            validate_text(summary, "summary", 4_000)?;
        }
        let mut db = self.db()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        authorize(&tx, &request.session_id, &request.lease_token, now)?;
        tx.execute("UPDATE coding_sessions SET heartbeat_at=?1,expires_at=?2,summary=COALESCE(?3,summary) WHERE id=?4",
            params![now, now + request.ttl_seconds as i64, request.summary, request.session_id])?;
        let result = session(&tx, &request.session_id, now)?;
        tx.commit()?;
        Ok(json!({"session":result}))
    }

    pub(crate) fn claim(&self, request: ClaimRequest) -> Result<Value> {
        self.claim_at(request, Utc::now().timestamp())
    }

    fn claim_at(&self, request: ClaimRequest, now: i64) -> Result<Value> {
        ensure!(request.paths.len() <= 200, "at most 200 claim paths");
        let mut db = self.db()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let owner = authorize(&tx, &request.session_id, &request.lease_token, now)?;
        ensure!(
            owner.worktree == self.root,
            "claim must be requested from the registered worktree"
        );
        let paths = normalize_paths(&self.root, &request.paths)?;
        let mut conflicts = Vec::new();
        let mut conflict_count = 0usize;
        let peers = active_sessions(&tx, now)?;
        for peer in peers.into_iter().filter(|peer| peer.id != owner.id) {
            for (ours, theirs) in overlapping_paths(&paths, &peer.claims) {
                conflict_count += 1;
                if conflicts.len() < 100 {
                    conflicts.push(json!({"requested_path":ours,"claimed_path":theirs,
                            "session_id":peer.id,"owner":peer.owner,"intent":peer.intent,
                            "worktree":peer.worktree,"expires_at":peer.expires_at}));
                }
            }
        }
        if !conflicts.is_empty() {
            // Existing ownership survives a failed replacement, all-or-nothing.
            tx.commit()?;
            return Ok(
                json!({"acquired":false,"conflicts":conflicts,"conflict_count":conflict_count,"conflicts_truncated":conflict_count>conflicts.len(),"session":owner,
                "next":"Narrow the requested surface or arrange handoff with its owner. Isolation protects files but does not remove conceptual ownership conflicts."}),
            );
        }
        expire(&tx, now)?;
        tx.execute("DELETE FROM path_claims WHERE session_id=?1", [&owner.id])?;
        for path in &paths {
            tx.execute(
                "INSERT INTO path_claims(session_id,path) VALUES (?1,?2)",
                params![owner.id, path],
            )?;
        }
        let result = session(&tx, &owner.id, now)?;
        tx.commit()?;
        Ok(json!({"acquired":true,"conflicts":[],"session":result}))
    }

    pub(crate) fn close(&self, auth: SessionAuth) -> Result<Value> {
        let now = Utc::now().timestamp();
        let mut db = self.db()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        authorize(&tx, &auth.session_id, &auth.lease_token, now)?;
        tx.execute(
            "UPDATE coding_sessions SET status='closed' WHERE id=?1",
            [&auth.session_id],
        )?;
        tx.execute(
            "DELETE FROM path_claims WHERE session_id=?1",
            [&auth.session_id],
        )?;
        let result = session(&tx, &auth.session_id, now)?;
        tx.commit()?;
        Ok(
            json!({"session":result,"note":"Claims released. Branches and worktrees remain available; closing never deletes work."}),
        )
    }

    pub(crate) fn get(&self, id: &str) -> Result<Value> {
        let db = self.db()?;
        let now = Utc::now().timestamp();
        Ok(json!({"session":session(&db,id,now)?,"shared_repository":self.state}))
    }

    pub(crate) fn list(&self, request: SessionListRequest) -> Result<Value> {
        let db = self.db()?;
        let now = Utc::now().timestamp();
        let limit = request.limit.unwrap_or(50).clamp(1, 200);
        let offset = request.offset.unwrap_or(0).min(i64::MAX as usize);
        let total: usize = db.query_row(
            "SELECT COUNT(*) FROM coding_sessions WHERE ?1 OR (status='active' AND expires_at>?2)",
            params![request.include_inactive, now],
            |row| row.get(0),
        )?;
        let ids = db.prepare("SELECT id FROM coding_sessions WHERE ?1 OR (status='active' AND expires_at>?2) ORDER BY created_at DESC,id LIMIT ?3 OFFSET ?4")?
            .query_map(params![request.include_inactive,now,limit as i64,offset as i64],|row| row.get::<_,String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let sessions = ids
            .iter()
            .map(|id| session(&db, id, now))
            .collect::<Result<Vec<_>>>()?;
        let next = offset.saturating_add(sessions.len());
        Ok(json!({"sessions":sessions,"shared_repository":self.state,
            "page":{"total":total,"has_more":next<total,"next_offset":(next<total).then_some(next)},
            "authority":"Claims coordinate participating sessions; external editors remain outside this protocol."}))
    }

    pub(crate) fn plan_commits(&self, request: CommitPlanRequest) -> Result<Value> {
        ensure!(
            !request.groups.is_empty() && request.groups.len() <= 50,
            "provide 1..=50 cohesive commit groups"
        );
        let _guard = self.lock("git-mutation.lock")?;
        let now = Utc::now().timestamp();
        let mut db = self.db()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let head = self.git(&["rev-parse", "--verify", "HEAD"])?;
        let branch = self.git_optional(&["symbolic-ref", "--quiet", "--short", "HEAD"])?;
        let (owner, implicit) = match (&request.session_id, &request.lease_token) {
            (Some(id), Some(token)) => (authorize(&tx, id, token, now)?, false),
            (None, None) => (
                self.implicit_session(&tx, head.trim(), branch.as_deref(), now)?,
                true,
            ),
            _ => bail!("provide both session_id and lease_token, or neither for single-agent work"),
        };
        ensure!(
            owner.worktree == self.root,
            "commit planning must use the registered worktree"
        );
        ensure!(
            branch == owner.branch,
            "session branch changed; register the new worktree state"
        );
        let changed = self.changed_paths()?;
        let mut selected = BTreeSet::new();
        let mut groups = Vec::new();
        for group in request.groups {
            validate_text(&group.message, "commit message", 8_000)?;
            ensure!(
                !group.paths.is_empty() && group.paths.len() <= 200,
                "each group needs 1..=200 paths"
            );
            let selectors = normalize_paths(&self.root, &group.paths)?;
            let mut paths = Vec::new();
            for path in &changed {
                if selectors.iter().any(|selector| overlaps(selector, path)) {
                    ensure!(
                        implicit || owner.claims.iter().any(|claim| covers(claim, path)),
                        "unowned changed path `{path}`; claim it before planning commits"
                    );
                    ensure!(
                        selected.insert(path.clone()),
                        "path `{path}` belongs to more than one commit group"
                    );
                    paths.push(path.clone());
                }
            }
            ensure!(
                !paths.is_empty(),
                "commit group `{}` has no live changed paths",
                group.message
            );
            groups.push(CommitGroup {
                message: group.message,
                paths,
            });
        }
        let paths = selected.into_iter().collect::<Vec<_>>();
        ensure!(
            paths.len() <= 2000,
            "commit plan exceeds 2000 changed files; split the work into cohesive plans"
        );
        if implicit {
            // Changed files are distinct leaves, so they already form a
            // normalized claim set. They stay visible to sessions that start
            // before execution.
            for path in &paths {
                tx.execute(
                    "INSERT INTO path_claims(session_id,path) VALUES (?1,?2)",
                    params![owner.id, path],
                )?;
            }
        }
        let id = format!("commits_{:032x}", random::<u128>());
        let fingerprints = paths
            .iter()
            .map(|path| Ok(json!({"path":path,"content":file_fingerprint(&self.root,path)?})))
            .collect::<Result<Vec<_>>>()?;
        let payload = json!({"plan_id":id,"session_id":owner.id,"worktree":self.root,
        "head":head.trim(),"branch":branch,"groups":groups,"fingerprints":fingerprints,
        "unassigned_changed_paths":changed.into_iter().filter(|path|!paths.contains(path)).collect::<Vec<_>>(),
        "created_at":now,"executed":false,"implicit_session":implicit,
        "note":if implicit {
            "A plan records whole-file boundaries. It neither stages nor commits; edits after planning require a new plan. An implicit single-agent session owns the planned paths: call commit.execute with plan_id alone. It is refused once another coding session is active."
        } else {
            "A plan records whole-file boundaries. It neither stages nor commits; edits after planning require a new plan."
        }});
        tx.execute(
            "INSERT INTO commit_plans(id,session_id,payload,created_at) VALUES (?1,?2,?3,?4)",
            params![id, owner.id, payload.to_string(), now],
        )?;
        tx.commit()?;
        Ok(payload)
    }

    /// Registers the ephemeral session that owns a single-agent commit plan.
    ///
    /// Only valid while no other coding session is active anywhere in the
    /// shared repository: an implicit plan must never take ownership around a
    /// participating agent. An earlier implicit session of this worktree is
    /// superseded, so an abandoned plan cannot block the next one.
    fn implicit_session(
        &self,
        tx: &Connection,
        head: &str,
        branch: Option<&str>,
        now: i64,
    ) -> Result<CodingSession> {
        expire(tx, now)?;
        let worktree = self.root.to_str().context("non-UTF-8 worktree path")?;
        tx.execute(
            "UPDATE coding_sessions SET status='closed' WHERE status='active' AND owner=?1 AND worktree=?2",
            params![IMPLICIT_OWNER, worktree],
        )?;
        expire(tx, now)?;
        let others = active_sessions(tx, now)?;
        if !others.is_empty() {
            let owners = others
                .iter()
                .take(5)
                .map(|session| format!("{} ({})", session.owner, session.id))
                .collect::<Vec<_>>()
                .join(", ");
            bail!(
                "{} other coding session(s) are active in this repository: {owners}. Parallel work requires an explicit session: call session.start, claim your paths, and pass session_id and lease_token",
                others.len()
            );
        }
        let id = format!("session_{:032x}", random::<u128>());
        // The token is never returned: commit.execute authorizes an implicit
        // plan by its owner and the continued absence of other sessions.
        let token = format!("{:032x}{:032x}", random::<u128>(), random::<u128>());
        tx.execute("INSERT INTO coding_sessions(id,token_hash,owner,intent,work_ids,worktree,branch,base_head,status,created_at,heartbeat_at,expires_at) VALUES (?1,?2,?3,'implicit single-agent commit plan','[]',?4,?5,?6,'active',?7,?7,?8)",
            params![id, token_hash(&token), IMPLICIT_OWNER, worktree, branch, head, now, now + IMPLICIT_TTL_SECONDS])?;
        session(tx, &id, now)
    }

    pub(crate) fn changed_paths(&self) -> Result<BTreeSet<String>> {
        let output = self
            .control
            .output(Command::new("git").current_dir(&self.root).args([
                "status",
                "--porcelain=v1",
                "-z",
                "--untracked-files=all",
            ]))?;
        ensure!(
            output.status.success(),
            "git status failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        ensure!(
            output.stdout.len() < crate::execution::MAX_CAPTURE_BYTES,
            "Git status exceeds the capture budget; narrow the worktree before planning"
        );
        let mut chunks = output.stdout.split(|byte| *byte == 0);
        let mut paths = BTreeSet::new();
        while let Some(chunk) = chunks.next() {
            if chunk.is_empty() {
                continue;
            }
            ensure!(chunk.len() >= 4, "malformed Git status record");
            let status = &chunk[..2];
            ensure!(
                !status.contains(&b'U') && status != b"AA" && status != b"DD",
                "resolve Git's unmerged paths before planning commits"
            );
            let path = std::str::from_utf8(&chunk[3..]).context("non-UTF-8 changed path")?;
            // The observatory creates private local state even in projects
            // which have not yet added it to .gitignore. It is never delivery.
            if !(internal_path(path) || status == b"??" && path.starts_with("target/")) {
                paths.insert(normalize_path(&self.root, path)?);
            }
            if status.contains(&b'R') || status.contains(&b'C') {
                let source = chunks.next().context("missing rename source")?;
                let source = std::str::from_utf8(source)?;
                if !internal_path(source) {
                    paths.insert(normalize_path(&self.root, source)?);
                }
            }
        }
        Ok(paths)
    }

    pub(crate) fn git(&self, args: &[&str]) -> Result<String> {
        let output = self
            .control
            .output(Command::new("git").current_dir(&self.root).args(args))?;
        ensure!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).context("non-UTF-8 Git output")
    }

    pub(crate) fn git_optional(&self, args: &[&str]) -> Result<Option<String>> {
        let output = self
            .control
            .output(Command::new("git").current_dir(&self.root).args(args))?;
        if output.status.success() {
            Ok(Some(String::from_utf8(output.stdout)?.trim().to_owned()))
        } else {
            // Expected Git absence (no commit, detached HEAD, non-Git root).
            ensure!(
                matches!(output.status.code(), Some(1 | 128)),
                "Git query failed unexpectedly: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            Ok(None)
        }
    }
}

fn token_hash(token: &str) -> String {
    blake3::hash(token.as_bytes()).to_hex().to_string()
}

fn validate_ttl(ttl: u64) -> Result<()> {
    ensure!((30..=3600).contains(&ttl), "ttl_seconds must be 30..=3600");
    Ok(())
}

fn validate_text(text: &str, name: &str, max: usize) -> Result<()> {
    ensure!(
        !text.trim().is_empty() && text.len() <= max && !text.contains('\0'),
        "{name} must be nonempty, NUL-free and at most {max} bytes"
    );
    Ok(())
}

fn expire(db: &Connection, now: i64) -> Result<()> {
    db.execute(
        "UPDATE coding_sessions SET status='expired' WHERE status='active' AND expires_at<=?1",
        [now],
    )?;
    db.execute("DELETE FROM path_claims WHERE session_id IN (SELECT id FROM coding_sessions WHERE status!='active')",[])?;
    Ok(())
}

pub(crate) fn authorize(db: &Connection, id: &str, token: &str, now: i64) -> Result<CodingSession> {
    let hash = db
        .query_row(
            "SELECT token_hash FROM coding_sessions WHERE id=?1",
            [id],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .context("unknown coding session")?;
    ensure!(hash == token_hash(token), "invalid session lease token");
    let session = session(db, id, now)?;
    ensure!(
        matches!(session.status, SessionStatus::Active),
        "session is closed or expired; register a new session"
    );
    Ok(session)
}

/// Authorizes an implicit single-agent session without a lease token. It
/// stays valid only while it is active and no other session has started.
pub(crate) fn authorize_implicit(db: &Connection, id: &str, now: i64) -> Result<CodingSession> {
    let session = session(db, id, now)?;
    ensure!(
        session.owner == IMPLICIT_OWNER,
        "plan belongs to an explicit session; pass its session_id and lease_token"
    );
    ensure!(
        matches!(session.status, SessionStatus::Active),
        "the implicit commit session is closed or expired; run commit.plan again"
    );
    let others = active_sessions(db, now)?
        .into_iter()
        .filter(|other| other.id != id)
        .count();
    ensure!(
        others == 0,
        "another coding session became active after planning; register with session.start, claim the paths, and plan again"
    );
    Ok(session)
}

/// Ends an implicit session once its plan is delivered, releasing its claims.
pub(crate) fn close_implicit(db: &Connection, id: &str) -> Result<()> {
    db.execute(
        "UPDATE coding_sessions SET status='closed' WHERE id=?1 AND owner=?2",
        params![id, IMPLICIT_OWNER],
    )?;
    db.execute("DELETE FROM path_claims WHERE session_id=?1", [id])?;
    Ok(())
}

fn session(db: &Connection, id: &str, now: i64) -> Result<CodingSession> {
    let mut session = db.query_row("SELECT id,owner,intent,work_ids,worktree,branch,base_head,status,created_at,heartbeat_at,expires_at,summary FROM coding_sessions WHERE id=?1",[id],|row| {
        let status: String = row.get(7)?;
        let work: String = row.get(3)?;
        let work_ids = serde_json::from_str(&work).map_err(|error|rusqlite::Error::FromSqlConversionFailure(3,rusqlite::types::Type::Text,Box::new(error)))?;
        Ok(CodingSession {id:row.get(0)?,owner:row.get(1)?,intent:row.get(2)?,work_ids,
            worktree:PathBuf::from(row.get::<_,String>(4)?),branch:row.get(5)?,base_head:row.get(6)?,
            status:match status.as_str(){"active"=>SessionStatus::Active,"closed"=>SessionStatus::Closed,"expired"=>SessionStatus::Expired,_=>return Err(rusqlite::Error::InvalidQuery)},
            created_at:row.get(8)?,heartbeat_at:row.get(9)?,expires_at:row.get(10)?,summary:row.get(11)?,claims:Vec::new()})
    }).optional()?.context("unknown coding session")?;
    if matches!(session.status, SessionStatus::Active) && session.expires_at <= now {
        session.status = SessionStatus::Expired;
    }
    if matches!(session.status, SessionStatus::Active) {
        session.claims = db
            .prepare("SELECT path FROM path_claims WHERE session_id=?1 ORDER BY path")?
            .query_map([id], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        session
            .claims
            .sort_by(|a, b| a.split('/').cmp(b.split('/')));
    }
    Ok(session)
}

fn active_sessions(db: &Connection, now: i64) -> Result<Vec<CodingSession>> {
    let ids = db
        .prepare(
            "SELECT id FROM coding_sessions WHERE status='active' AND expires_at>?1 ORDER BY id",
        )?
        .query_map([now], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    ids.iter().map(|id| session(db, id, now)).collect()
}

pub(crate) fn covers(parent: &str, path: &str) -> bool {
    parent == "."
        || parent == path
        || path
            .strip_prefix(parent)
            .is_some_and(|tail| tail.starts_with('/'))
}

fn overlaps(left: &str, right: &str) -> bool {
    covers(left, right) || covers(right, left)
}

/// Normalized claim sets are sorted and contain no ancestor/descendant pairs.
/// Traverse them in O(left + right + matches), retaining an ancestor until
/// all matching descendants have been emitted.
fn overlapping_paths<'a>(left: &'a [String], right: &'a [String]) -> Vec<(&'a str, &'a str)> {
    let (mut l, mut r) = (0, 0);
    let mut matches = Vec::new();
    while l < left.len() && r < right.len() {
        let (a, b) = (left[l].as_str(), right[r].as_str());
        if covers(a, b) {
            matches.push((a, b));
            if a == b {
                l += 1;
            }
            r += 1;
        } else if covers(b, a) {
            matches.push((a, b));
            l += 1;
        } else if a.split('/').cmp(b.split('/')).is_lt() {
            l += 1;
        } else {
            r += 1;
        }
    }
    matches
}

fn normalize_paths(root: &Path, paths: &[String]) -> Result<Vec<String>> {
    let mut paths = paths
        .iter()
        .map(|path| normalize_path(root, path))
        .collect::<Result<Vec<_>>>()?;
    paths.sort_by(|a, b| a.split('/').cmp(b.split('/')));
    paths.dedup();
    // Remove redundant descendants so acquisition and reporting stay bounded.
    let mut result: Vec<String> = Vec::new();
    for path in paths {
        if !result.iter().any(|parent| covers(parent, &path)) {
            result.push(path);
        }
    }
    Ok(result)
}

pub(crate) fn normalize_path(root: &Path, path: &str) -> Result<String> {
    validate_text(path, "path", 4_096)?;
    ensure!(!path.contains('\\'), "use slash-separated repository paths");
    let mut parts = Vec::new();
    let mut absolute = root.to_path_buf();
    for component in Path::new(path).components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => {
                let part = part.to_str().context("non-UTF-8 path")?;
                ensure!(
                    !matches!(part, ".git" | ".rust-repo-intelligence"),
                    "repository internals cannot be claimed"
                );
                absolute.push(part);
                match fs::symlink_metadata(&absolute) {
                    Ok(metadata) => ensure!(
                        !metadata.file_type().is_symlink(),
                        "symlink claim aliases are unsupported; use the actual repository path"
                    ),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
                parts.push(part);
            }
            _ => bail!("claim paths must be relative and cannot traverse parents"),
        }
    }
    Ok(if parts.is_empty() {
        ".".into()
    } else {
        parts.join("/")
    })
}

pub(crate) fn file_fingerprint(root: &Path, path: &str) -> Result<String> {
    let path = root.join(normalize_path(root, path)?);
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok("deleted".into()),
        Err(error) => return Err(error.into()),
    };
    let mut hasher = blake3::Hasher::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        hasher.update(&(file.metadata()?.permissions().mode() & 0o111).to_le_bytes());
    }
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(format!("b3:{}", hasher.finalize().to_hex()))
}

fn internal_path(path: &str) -> bool {
    Path::new(path).components().any(|part| {
        matches!(part, Component::Normal(name) if name == ".git" || name == ".rust-repo-intelligence")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    fn git(root: &Path, args: &[&str]) {
        let output = Command::new("git")
            .current_dir(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q"]);
        git(dir.path(), &["config", "user.name", "Test"]);
        git(dir.path(), &["config", "user.email", "test@example.test"]);
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/a.rs"), "fn a() {}\n").unwrap();
        fs::write(dir.path().join("src/b.rs"), "fn b() {}\n").unwrap();
        git(dir.path(), &["add", "src"]);
        git(dir.path(), &["commit", "-qm", "initial"]);
        dir
    }

    fn coordinator(root: &Path) -> Coordinator {
        Coordinator::open(root, ExecutionControl::default()).unwrap()
    }

    fn request() -> SessionStartRequest {
        SessionStartRequest {
            owner: "agent".into(),
            intent: "change a function".into(),
            work_ids: vec![],
            isolate: false,
            ttl_seconds: 600,
        }
    }

    fn auth(value: &Value) -> SessionAuth {
        SessionAuth {
            session_id: value["session"]["id"].as_str().unwrap().into(),
            lease_token: value["lease_token"].as_str().unwrap().into(),
        }
    }

    fn claim(auth: &SessionAuth, paths: &[&str]) -> ClaimRequest {
        ClaimRequest {
            session_id: auth.session_id.clone(),
            lease_token: auth.lease_token.clone(),
            paths: paths.iter().map(|path| (*path).into()).collect(),
        }
    }

    #[test]
    fn claims_are_atomic_across_connections_and_directory_boundaries() {
        let dir = fixture();
        let a = coordinator(dir.path());
        let b = coordinator(dir.path());
        let one = auth(&a.start(request()).unwrap());
        let two = auth(&b.start(request()).unwrap());
        assert_eq!(
            a.claim(claim(&one, &["src/a.rs"])).unwrap()["acquired"],
            true
        );
        assert_eq!(b.claim(claim(&two, &["docs"])).unwrap()["acquired"], true);
        let conflict = b.claim(claim(&two, &["src", "tests"])).unwrap();
        assert_eq!(conflict["acquired"], false);
        assert_eq!(conflict["session"]["claims"], json!(["docs"]));
        assert_eq!(
            b.claim(claim(&two, &["src/ab.rs"])).unwrap()["acquired"],
            true
        );
        assert_eq!(a.claim(claim(&one, &["."])).unwrap()["acquired"], false);
        a.close(one).unwrap();
        assert_eq!(b.claim(claim(&two, &["src"])).unwrap()["acquired"], true);
    }

    #[test]
    fn racing_acquisitions_have_one_winner() {
        let dir = fixture();
        let root = dir.path().to_path_buf();
        let coord = coordinator(&root);
        let one = auth(&coord.start(request()).unwrap());
        let two = auth(&coord.start(request()).unwrap());
        let barrier = Arc::new(Barrier::new(2));
        let handles = [one, two]
            .into_iter()
            .map(|owner| {
                let coord = coordinator(&root);
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    coord.claim(claim(&owner, &["src"])).unwrap()["acquired"]
                        .as_bool()
                        .unwrap()
                })
            })
            .collect::<Vec<_>>();
        let winners = handles
            .into_iter()
            .map(|handle| usize::from(handle.join().unwrap()))
            .sum::<usize>();
        assert_eq!(winners, 1);
    }

    #[test]
    fn expiry_is_terminal_and_tokens_do_not_leak() {
        let dir = fixture();
        let coord = coordinator(dir.path());
        let owner = auth(&coord.start_at(request(), 100).unwrap());
        coord.claim_at(claim(&owner, &["src"]), 101).unwrap();
        let heartbeat = HeartbeatRequest {
            session_id: owner.session_id.clone(),
            lease_token: owner.lease_token.clone(),
            ttl_seconds: 600,
            summary: None,
        };
        assert!(coord.heartbeat_at(heartbeat, 701).is_err());
        let next = auth(&coord.start_at(request(), 702).unwrap());
        assert_eq!(
            coord.claim_at(claim(&next, &["src"]), 703).unwrap()["acquired"],
            true
        );
        let db = coord.db().unwrap();
        let old = session(&db, &owner.session_id, 704).unwrap();
        assert!(matches!(old.status, SessionStatus::Expired));
        assert!(old.claims.is_empty());
        let visible = serde_json::to_string(&session(&db, &next.session_id, 704).unwrap()).unwrap();
        assert!(!visible.contains(&next.lease_token));
        assert!(!visible.contains("token_hash"));
        let mut wrong = claim(&next, &["docs"]);
        wrong.lease_token = "wrong".into();
        assert!(coord.claim_at(wrong, 704).is_err());
    }

    #[test]
    fn isolated_worktrees_share_ownership_without_copying_dirty_files() {
        let dir = fixture();
        let coord = coordinator(dir.path());
        fs::write(dir.path().join("src/a.rs"), "dirty main worktree").unwrap();
        let mut start = request();
        start.isolate = true;
        let isolated = coord.start(start).unwrap();
        let owner = auth(&isolated);
        let root = PathBuf::from(isolated["session"]["worktree"].as_str().unwrap());
        let peer = coordinator(&root);
        assert_eq!(coord.state, peer.state);
        assert_eq!(
            fs::read_to_string(root.join("src/a.rs")).unwrap(),
            "fn a() {}\n"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("src/a.rs")).unwrap(),
            "dirty main worktree"
        );
        assert!(coord.claim(claim(&owner, &["src"])).is_err());
        peer.claim(claim(&owner, &["src"])).unwrap();
        let other = auth(&coord.start(request()).unwrap());
        assert_eq!(
            coord.claim(claim(&other, &["src/a.rs"])).unwrap()["acquired"],
            false
        );
        assert!(peer.get(&other.session_id).is_ok());
    }

    #[test]
    fn invalid_paths_and_symlink_aliases_cannot_escape_claims() {
        let dir = fixture();
        for path in [
            "../escape",
            "/tmp/escape",
            "src/../../escape",
            ".git/config",
            ".rust-repo-intelligence",
            "src\\a.rs",
        ] {
            assert!(normalize_path(dir.path(), path).is_err(), "{path}");
        }
        assert_eq!(
            normalize_path(dir.path(), "./src//a.rs").unwrap(),
            "src/a.rs"
        );
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("src", dir.path().join("alias")).unwrap();
            assert!(normalize_path(dir.path(), "alias/a.rs").is_err());
        }
    }

    #[test]
    fn commit_groups_include_only_owned_changes_and_preserve_staging() {
        let dir = fixture();
        let coord = coordinator(dir.path());
        let owner = auth(&coord.start(request()).unwrap());
        coord.claim(claim(&owner, &["src/a.rs"])).unwrap();
        fs::write(
            dir.path().join("src/a.rs"),
            "fn a() { println!(\"new\"); }\n",
        )
        .unwrap();
        fs::write(dir.path().join("src/b.rs"), "another agent's change\n").unwrap();
        git(dir.path(), &["add", "src/b.rs"]);
        let staged = coord.git(&["diff", "--cached"]).unwrap();
        let make = |paths: Vec<String>| CommitPlanRequest {
            session_id: Some(owner.session_id.clone()),
            lease_token: Some(owner.lease_token.clone()),
            groups: vec![CommitGroup {
                message: "Change a".into(),
                paths,
            }],
        };
        assert!(coord.plan_commits(make(vec!["src".into()])).is_err());
        let plan = coord.plan_commits(make(vec!["src/a.rs".into()])).unwrap();
        assert_eq!(plan["groups"][0]["paths"], json!(["src/a.rs"]));
        assert_eq!(plan["unassigned_changed_paths"], json!(["src/b.rs"]));
        assert_eq!(coord.git(&["diff", "--cached"]).unwrap(), staged);
        let mut duplicate = make(vec!["src/a.rs".into()]);
        duplicate.groups.push(duplicate.groups[0].clone());
        assert!(coord.plan_commits(duplicate).is_err());
    }

    #[test]
    fn sorted_overlap_scan_matches_exhaustive_hierarchy_checks() {
        let dir = fixture();
        let universe = [
            ".",
            "src",
            "src/a",
            "src/a-foo",
            "src/a/x",
            "src/aa",
            "docs",
            "tests",
        ];
        for left_mask in 0u16..256 {
            let left = universe
                .iter()
                .enumerate()
                .filter(|(i, _)| left_mask & (1 << i) != 0)
                .map(|(_, p)| p.to_string())
                .collect::<Vec<_>>();
            let left = normalize_paths(dir.path(), &left).unwrap();
            for right in [
                vec![".".into()],
                vec!["src/a.rs".into(), "src/b.rs".into()],
                vec!["src".into(), "docs/a".into()],
                vec!["src/aa".into(), "tests".into()],
                vec!["src/a-foo".into(), "src/a/x".into()],
            ] {
                let right = normalize_paths(dir.path(), &right).unwrap();
                let actual = overlapping_paths(&left, &right)
                    .into_iter()
                    .collect::<BTreeSet<_>>();
                let expected = left
                    .iter()
                    .flat_map(|a| {
                        right
                            .iter()
                            .filter(|b| overlaps(a, b))
                            .map(move |b| (a.as_str(), b.as_str()))
                    })
                    .collect::<BTreeSet<_>>();
                assert_eq!(actual, expected);
            }
        }
    }
}

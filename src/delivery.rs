//! Immutable delivery identities and recoverable, exact-content Git mutations.

use crate::{
    coordination::{CommitGroup, Coordinator, SessionAuth, authorize, covers, file_fingerprint},
    execution::MAX_CAPTURE_BYTES,
};
use anyhow::{Context, Result, ensure};
use chrono::Utc;
use rand::random;
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{fs, path::PathBuf, process::Command};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS commit_executions(
 plan_id TEXT PRIMARY KEY REFERENCES commit_plans(id), payload TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS work_chunks(
 id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES coding_sessions(id),
 payload TEXT NOT NULL, created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS integration_previews(
 id TEXT PRIMARY KEY, payload TEXT NOT NULL, created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS integrations(
 id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES coding_sessions(id), payload TEXT NOT NULL
);
"#;

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CommitExecuteRequest {
    pub session_id: String,
    pub lease_token: String,
    pub plan_id: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChunkCreateRequest {
    pub session_id: String,
    pub lease_token: String,
    pub title: String,
    pub summary: String,
    /// Completed commit execution to deliver; mutable branch names are not evidence.
    pub plan_id: String,
    #[serde(default)]
    pub validation_ids: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IntegrationPreviewRequest {
    /// Commit or branch to integrate. Resolved to an immutable OID immediately.
    pub source_ref: String,
    pub base_ref: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IntegrationStartRequest {
    pub session_id: String,
    pub lease_token: String,
    pub preview_id: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IntegrationResolveRequest {
    pub session_id: String,
    pub lease_token: String,
    pub integration_id: String,
    pub message: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IntegrationCompleteRequest {
    pub session_id: String,
    pub lease_token: String,
    pub integration_id: String,
    /// Run verification from the returned integration worktree after resolve.
    pub verification_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Fingerprint {
    path: String,
    content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CommitPlan {
    plan_id: String,
    session_id: String,
    worktree: PathBuf,
    head: String,
    branch: Option<String>,
    groups: Vec<CommitGroup>,
    fingerprints: Vec<Fingerprint>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ExecutionState {
    ObjectsCreated,
    HeadAdvanced,
    Completed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CommitExecution {
    plan_id: String,
    session_id: String,
    base_head: String,
    head: String,
    commits: Vec<String>,
    state: ExecutionState,
    created_at: i64,
    /// Plumbing creates exactly the planned tree; hooks are not implicitly run.
    hooks_run: bool,
}

struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

impl Coordinator {
    fn delivery_db(&self) -> Result<rusqlite::Connection> {
        let db = self.db()?;
        db.execute_batch(SCHEMA)?;
        Ok(db)
    }

    fn commit_plan_record(&self, id: &str) -> Result<CommitPlan> {
        let payload: String = self
            .db()?
            .query_row(
                "SELECT payload FROM commit_plans WHERE id=?1",
                [id],
                |row| row.get(0),
            )
            .optional()?
            .context("unknown commit plan")?;
        Ok(serde_json::from_str(&payload)?)
    }

    fn check_plan(&self, plan: &CommitPlan, auth: &SessionAuth) -> Result<()> {
        ensure!(
            plan.session_id == auth.session_id,
            "plan belongs to another session"
        );
        ensure!(
            plan.worktree == self.root,
            "use the plan's registered worktree"
        );
        let owner = authorize(
            &self.db()?,
            &auth.session_id,
            &auth.lease_token,
            Utc::now().timestamp(),
        )?;
        ensure!(owner.worktree == self.root, "use the registered worktree");
        ensure!(
            self.git_optional(&["symbolic-ref", "--quiet", "--short", "HEAD"])? == plan.branch,
            "session branch changed; register and plan again"
        );
        for fingerprint in &plan.fingerprints {
            ensure!(
                owner
                    .claims
                    .iter()
                    .any(|claim| covers(claim, &fingerprint.path)),
                "ownership of `{}` was released; claim and plan again",
                fingerprint.path
            );
            ensure!(
                file_fingerprint(&self.root, &fingerprint.path)? == fingerprint.content,
                "`{}` changed after planning; create a new plan",
                fingerprint.path
            );
        }
        Ok(())
    }

    fn path_file(&self, paths: &[String]) -> Result<Scratch> {
        let file = Scratch(self.state.join(format!("paths_{:032x}", random::<u128>())));
        let mut bytes = Vec::new();
        for path in paths {
            ensure!(!path.contains('\0'), "NUL in delivery path");
            bytes.extend_from_slice(path.as_bytes());
            bytes.push(0);
        }
        fs::write(&file.0, bytes)?;
        Ok(file)
    }

    fn indexed_git(&self, index: &Scratch, args: &[&str]) -> Result<String> {
        let output = self.control.output(
            Command::new("git")
                .current_dir(&self.root)
                .env("GIT_INDEX_FILE", &index.0)
                .args(args),
        )?;
        ensure!(
            output.status.success(),
            "Git tree construction failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        ensure!(
            output.stdout.len() < MAX_CAPTURE_BYTES,
            "Git output exceeded capture budget"
        );
        Ok(String::from_utf8(output.stdout)?.trim().to_owned())
    }

    fn save_execution(&self, execution: &CommitExecution) -> Result<()> {
        self.delivery_db()?.execute("INSERT INTO commit_executions(plan_id,payload) VALUES (?1,?2) ON CONFLICT(plan_id) DO UPDATE SET payload=excluded.payload",
            params![execution.plan_id, serde_json::to_string(execution)?])?;
        Ok(())
    }

    pub(crate) fn execute_commits(&self, request: CommitExecuteRequest) -> Result<Value> {
        let _guard = self.lock("git-mutation.lock")?;
        let db = self.delivery_db()?;
        let plan = self.commit_plan_record(&request.plan_id)?;
        let auth = SessionAuth {
            session_id: request.session_id,
            lease_token: request.lease_token,
        };
        self.check_plan(&plan, &auth)?;
        let previous: Option<String> = db
            .query_row(
                "SELECT payload FROM commit_executions WHERE plan_id=?1",
                [&plan.plan_id],
                |row| row.get(0),
            )
            .optional()?;
        let mut execution: CommitExecution = if let Some(payload) = previous {
            serde_json::from_str(&payload)?
        } else {
            ensure!(
                self.git(&["rev-parse", "HEAD"])?.trim() == plan.head,
                "HEAD changed after planning; create a new plan"
            );
            // An independent index starts at the exact prepared parent. Other
            // sessions' staged content never enters any of the planned trees.
            let index = Scratch(self.state.join(format!("index_{:032x}", random::<u128>())));
            let mut parent = plan.head.clone();
            let mut commits = Vec::new();
            let sign =
                self.git_optional(&["config", "--bool", "commit.gpgsign"])? == Some("true".into());
            for group in &plan.groups {
                self.control.check()?;
                self.indexed_git(&index, &["read-tree", &parent])?;
                let paths = self.path_file(&group.paths)?;
                let path_option = format!("--pathspec-from-file={}", paths.0.display());
                self.indexed_git(
                    &index,
                    &[
                        "--literal-pathspecs",
                        "add",
                        "-A",
                        &path_option,
                        "--pathspec-file-nul",
                    ],
                )?;
                let tree = self.indexed_git(&index, &["write-tree"])?;
                let message = Scratch(
                    self.state
                        .join(format!("message_{:032x}", random::<u128>())),
                );
                fs::write(&message.0, &group.message)?;
                let mut args = vec![
                    "commit-tree",
                    &tree,
                    "-p",
                    &parent,
                    "-F",
                    message.0.to_str().context("non-UTF-8 message path")?,
                ];
                if sign {
                    args.push("-S");
                }
                parent = self.git(&args)?.trim().to_owned();
                commits.push(parent.clone());
            }
            let execution = CommitExecution {
                plan_id: plan.plan_id.clone(),
                session_id: auth.session_id.clone(),
                base_head: plan.head.clone(),
                head: parent,
                commits,
                state: ExecutionState::ObjectsCreated,
                created_at: Utc::now().timestamp(),
                hooks_run: false,
            };
            self.save_execution(&execution)?;
            execution
        };
        if execution.state == ExecutionState::Completed {
            return Ok(serde_json::to_value(execution)?);
        }
        let head = self.git(&["rev-parse", "HEAD"])?.trim().to_owned();
        if head == execution.base_head {
            // Recheck files and ownership after object construction/signing.
            self.check_plan(&plan, &auth)?;
            let mut db = self.db()?;
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            authorize(
                &tx,
                &auth.session_id,
                &auth.lease_token,
                Utc::now().timestamp(),
            )?;
            self.git(&[
                "update-ref",
                "-m",
                "Crusty planned commit groups",
                "HEAD",
                &execution.head,
                &execution.base_head,
            ])?;
            tx.commit()?;
        } else {
            ensure!(
                head == execution.head,
                "HEAD differs from both planned and delivered revisions; inspect commit.get before recovery"
            );
        }
        execution.state = ExecutionState::HeadAdvanced;
        self.save_execution(&execution)?;
        let paths = self.path_file(
            &plan
                .fingerprints
                .iter()
                .map(|fp| fp.path.clone())
                .collect::<Vec<_>>(),
        )?;
        let path_option = format!("--pathspec-from-file={}", paths.0.display());
        // Update only delivered index entries. This never writes worktree files
        // and preserves every unrelated staged entry. Recovery is idempotent.
        self.git(&[
            "--literal-pathspecs",
            "reset",
            "-q",
            &execution.head,
            &path_option,
            "--pathspec-file-nul",
        ])?;
        execution.state = ExecutionState::Completed;
        self.save_execution(&execution)?;
        Ok(serde_json::to_value(execution)?)
    }

    pub(crate) fn commit_get(&self, id: &str) -> Result<Value> {
        let db = self.delivery_db()?;
        let plan = self.commit_plan_record(id)?;
        let execution: Option<String> = db
            .query_row(
                "SELECT payload FROM commit_executions WHERE plan_id=?1",
                [id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(
            json!({"plan":plan,"execution":execution.map(|s|serde_json::from_str::<Value>(&s)).transpose()?}),
        )
    }

    pub(crate) fn chunk_create(&self, request: ChunkCreateRequest) -> Result<Value> {
        ensure!(
            !request.title.trim().is_empty()
                && request.title.len() <= 200
                && !request.title.contains('\0'),
            "title must be 1..=200 NUL-free bytes"
        );
        ensure!(
            request.summary.len() <= 32_000 && !request.summary.contains('\0'),
            "summary exceeds 32000 bytes or contains NUL"
        );
        ensure!(
            request.validation_ids.len() <= 100
                && request
                    .validation_ids
                    .iter()
                    .all(|id| !id.is_empty() && id.len() <= 200),
            "invalid validation identifiers"
        );
        let _guard = self.lock("git-mutation.lock")?;
        let db = self.delivery_db()?;
        let owner = authorize(
            &db,
            &request.session_id,
            &request.lease_token,
            Utc::now().timestamp(),
        )?;
        ensure!(owner.worktree == self.root, "use the registered worktree");
        let payload: String = db
            .query_row(
                "SELECT payload FROM commit_executions WHERE plan_id=?1",
                [&request.plan_id],
                |row| row.get(0),
            )
            .optional()?
            .context("commit plan has not been executed")?;
        let execution: CommitExecution = serde_json::from_str(&payload)?;
        ensure!(
            execution.session_id == owner.id && execution.state == ExecutionState::Completed,
            "chunk requires this session's completed commit execution"
        );
        ensure!(
            self.git(&["rev-parse", "HEAD"])?.trim() == execution.head,
            "HEAD changed; deliver a new plan before creating a chunk"
        );
        let id = format!("chunk_{:032x}", random::<u128>());
        let now = Utc::now().timestamp();
        let chunk = json!({"id":id,"session_id":owner.id,"work_ids":owner.work_ids,"title":request.title,"summary":request.summary,
            "worktree":owner.worktree,"branch":owner.branch,"base_head":execution.base_head,"head":execution.head,
            "commits":execution.commits,"plan_id":execution.plan_id,"validation_ids":request.validation_ids,
            "validation_authority":"Identifiers are references only; publishing must independently verify revision-bound results.","created_at":now});
        db.execute(
            "INSERT INTO work_chunks(id,session_id,payload,created_at) VALUES (?1,?2,?3,?4)",
            params![id, owner.id, chunk.to_string(), now],
        )?;
        Ok(chunk)
    }

    pub(crate) fn chunk_get(&self, id: &str) -> Result<Value> {
        let payload: String = self
            .delivery_db()?
            .query_row("SELECT payload FROM work_chunks WHERE id=?1", [id], |row| {
                row.get(0)
            })
            .optional()?
            .context("unknown work chunk")?;
        Ok(serde_json::from_str(&payload)?)
    }

    pub(crate) fn chunk_list(&self, limit: usize, offset: usize) -> Result<Value> {
        let db = self.delivery_db()?;
        let limit = limit.clamp(1, 200);
        let total: usize =
            db.query_row("SELECT COUNT(*) FROM work_chunks", [], |row| row.get(0))?;
        let payloads = db
            .prepare(
                "SELECT payload FROM work_chunks ORDER BY created_at DESC,id LIMIT ?1 OFFSET ?2",
            )?
            .query_map(
                params![limit as i64, offset.min(i64::MAX as usize) as i64],
                |row| row.get::<_, String>(0),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let chunks = payloads
            .iter()
            .map(|p| serde_json::from_str::<Value>(p))
            .collect::<serde_json::Result<Vec<_>>>()?;
        let next = offset.saturating_add(chunks.len());
        Ok(
            json!({"chunks":chunks,"page":{"total":total,"has_more":next<total,"next_offset":(next<total).then_some(next)}}),
        )
    }

    pub(crate) fn resolve_commit(&self, reference: &str) -> Result<String> {
        ensure!(
            !reference.is_empty() && reference.len() <= 1024 && !reference.contains('\0'),
            "invalid Git reference"
        );
        let commit = format!("{reference}^{{commit}}");
        Ok(self
            .git(&["rev-parse", "--verify", "--end-of-options", &commit])?
            .trim()
            .to_owned())
    }

    pub(crate) fn integration_preview(&self, request: IntegrationPreviewRequest) -> Result<Value> {
        let base = self.resolve_commit(&request.base_ref)?;
        let head = self.resolve_commit(&request.source_ref)?;
        let output = self
            .control
            .output(Command::new("git").current_dir(&self.root).args([
                "merge-tree",
                "--write-tree",
                "-z",
                "--name-only",
                &base,
                &head,
            ]))?;
        ensure!(
            matches!(output.status.code(), Some(0 | 1)),
            "Git merge preview failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        ensure!(
            output.stdout.len() < MAX_CAPTURE_BYTES,
            "merge preview exceeds capture budget; no complete preview recorded"
        );
        let mut records = output.stdout.split(|b| *b == 0);
        let tree = std::str::from_utf8(records.next().context("missing merge tree")?)?;
        let clean = output.status.success();
        let mut conflicts = Vec::new();
        if !clean {
            for record in records.by_ref() {
                if record.is_empty() {
                    break;
                }
                conflicts.push(std::str::from_utf8(record)?.to_owned());
            }
        }
        let details = records
            .map(|r| String::from_utf8_lossy(r).into_owned())
            .collect::<Vec<_>>();
        let id = format!("integration_{:032x}", random::<u128>());
        let now = Utc::now().timestamp();
        let preview = json!({"id":id,"base_head":base,"source_head":head,"result_tree":tree,
            "clean":clean,"conflicting_paths":conflicts,"details":details,"created_at":now,
            "authority":"Git merge-tree; original indexes and worktrees were not changed. Semantic conflict resolution still requires validation."});
        self.delivery_db()?.execute(
            "INSERT INTO integration_previews(id,payload,created_at) VALUES (?1,?2,?3)",
            params![id, preview.to_string(), now],
        )?;
        Ok(preview)
    }

    pub(crate) fn integration_get(&self, id: &str) -> Result<Value> {
        let db = self.delivery_db()?;
        let payload: Option<String> = db
            .query_row(
                "SELECT payload FROM integrations WHERE id=?1",
                [id],
                |row| row.get(0),
            )
            .optional()?;
        let payload = if let Some(payload) = payload {
            payload
        } else {
            db.query_row(
                "SELECT payload FROM integration_previews WHERE id=?1",
                [id],
                |row| row.get(0),
            )
            .optional()?
            .context("unknown integration or preview")?
        };
        Ok(serde_json::from_str(&payload)?)
    }

    fn save_integration(&self, record: &Value) -> Result<()> {
        self.delivery_db()?.execute("INSERT INTO integrations(id,session_id,payload) VALUES (?1,?2,?3) ON CONFLICT(id) DO UPDATE SET payload=excluded.payload",
            params![record["id"].as_str(),record["session_id"].as_str(),record.to_string()])?;
        Ok(())
    }

    pub(crate) fn integration_start(&self, request: IntegrationStartRequest) -> Result<Value> {
        let _guard = self.lock("git-mutation.lock")?;
        let owner = authorize(
            &self.db()?,
            &request.session_id,
            &request.lease_token,
            Utc::now().timestamp(),
        )?;
        let preview = self.integration_get(&request.preview_id)?;
        let base = preview["base_head"]
            .as_str()
            .context("not an integration preview")?;
        let source = preview["source_head"]
            .as_str()
            .context("missing source commit")?;
        self.resolve_commit(base)?;
        self.resolve_commit(source)?;
        let id = format!("resolution_{:032x}", random::<u128>());
        let branch = format!("crusty/{id}");
        let worktree = self.state.join("worktrees").join(&id);
        fs::create_dir_all(worktree.parent().context("missing worktree parent")?)?;
        // Persist intent before creating the worktree. An interrupted start is
        // inspectable; never remove an abandoned worktree or its uncommitted work.
        let mut record = json!({"id":id,"session_id":owner.id,"preview_id":request.preview_id,"base_head":base,
            "source_head":source,"worktree":worktree,"branch":branch,"state":"creating","created_at":Utc::now().timestamp(),
            "conflicting_paths":preview["conflicting_paths"]});
        self.save_integration(&record)?;
        self.git(&[
            "worktree",
            "add",
            "-b",
            &branch,
            worktree.to_str().context("non-UTF-8 worktree")?,
            base,
        ])?;
        let integration = Coordinator::open(&worktree, self.control.clone())?;
        let output = self
            .control
            .output(Command::new("git").current_dir(&worktree).args([
                "merge",
                "--no-commit",
                "--no-ff",
                source,
            ]))?;
        let merge_head = integration.git_optional(&["rev-parse", "--verify", "MERGE_HEAD"])?;
        if let Some(merge_head) = merge_head {
            ensure!(
                merge_head == source,
                "merge source differs from requested source"
            );
            ensure!(
                matches!(output.status.code(), Some(0 | 1)),
                "Git merge failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            record["state"] = json!(if output.status.success() {
                "ready_to_resolve"
            } else {
                "conflicted"
            });
        } else {
            ensure!(
                output.status.success(),
                "Git merge failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            record["state"] = json!("already_integrated");
            record["head"] = json!(base);
        }
        record["merge_output"] = json!(String::from_utf8_lossy(&output.stderr));
        record["next"] = json!(
            "Inspect and resolve in this isolated worktree, stage each resolution explicitly, then integration.resolve. Run verification.plan/run from this worktree and integration.complete with that run. Original worktrees remain untouched."
        );
        self.save_integration(&record)?;
        Ok(record)
    }

    fn integration_owner(
        &self,
        id: &str,
        session_id: &str,
        token: &str,
    ) -> Result<(Value, Coordinator)> {
        authorize(&self.db()?, session_id, token, Utc::now().timestamp())?;
        let record = self.integration_get(id)?;
        ensure!(
            record["session_id"] == session_id,
            "integration belongs to another session"
        );
        let worktree = record["worktree"]
            .as_str()
            .context("integration has no worktree")?;
        let coord = Coordinator::open(std::path::Path::new(worktree), self.control.clone())?;
        ensure!(
            coord.state == self.state,
            "integration worktree belongs to another repository"
        );
        ensure!(
            coord
                .git_optional(&["symbolic-ref", "--quiet", "--short", "HEAD"])?
                .as_deref()
                == record["branch"].as_str(),
            "integration branch changed"
        );
        Ok((record, coord))
    }

    pub(crate) fn integration_resolve(&self, request: IntegrationResolveRequest) -> Result<Value> {
        ensure!(
            !request.message.trim().is_empty()
                && request.message.len() <= 8000
                && !request.message.contains('\0'),
            "invalid integration commit message"
        );
        let _guard = self.lock("git-mutation.lock")?;
        let (mut record, coord) = self.integration_owner(
            &request.integration_id,
            &request.session_id,
            &request.lease_token,
        )?;
        if record["state"] == "resolved"
            || record["state"] == "completed"
            || record["state"] == "already_integrated"
        {
            return Ok(record);
        }
        let base = record["base_head"]
            .as_str()
            .context("missing integration base")?
            .to_owned();
        let source = record["source_head"]
            .as_str()
            .context("missing integration source")?
            .to_owned();
        if record["state"] == "advancing_head" {
            let head = record["head"].as_str().context("missing recovery head")?;
            let current = coord.resolve_commit("HEAD")?;
            if current == base {
                coord.git(&["update-ref", "HEAD", head, &base])?;
            } else {
                ensure!(current == head, "integration HEAD diverged during recovery");
            }
            if coord
                .git_optional(&["rev-parse", "--verify", "MERGE_HEAD"])?
                .is_some()
            {
                coord.git(&["merge", "--quit"])?;
            }
            record["state"] = json!("resolved");
            self.save_integration(&record)?;
            return Ok(record);
        }
        ensure!(
            coord.resolve_commit("HEAD")? == base,
            "integration HEAD changed before resolution"
        );
        ensure!(
            coord.git(&["ls-files", "--unmerged"])?.is_empty(),
            "unmerged paths remain; resolve and stage them explicitly"
        );
        ensure!(
            coord.git(&["diff", "--name-only"])?.is_empty(),
            "unstaged tracked resolution changes remain"
        );
        let untracked = coord.changed_paths()?;
        let staged = coord.git(&["diff", "--cached", "--name-only", "-z"])?;
        ensure!(
            untracked
                .iter()
                .all(|path| staged.split('\0').any(|p| p == path)),
            "untracked resolution changes remain; stage intentional files explicitly"
        );
        ensure!(
            coord
                .git_optional(&["rev-parse", "--verify", "MERGE_HEAD"])?
                .as_deref()
                == Some(&source),
            "merge state/source changed"
        );
        let tree = coord.git(&["write-tree"])?.trim().to_owned();
        let message = Scratch(
            self.state
                .join(format!("resolution_message_{:032x}", random::<u128>())),
        );
        fs::write(&message.0, &request.message)?;
        let mut args = vec![
            "commit-tree",
            &tree,
            "-p",
            &base,
            "-p",
            &source,
            "-F",
            message.0.to_str().context("non-UTF-8 message")?,
        ];
        if coord.git_optional(&["config", "--bool", "commit.gpgsign"])? == Some("true".into()) {
            args.push("-S");
        }
        let head = coord.git(&args)?.trim().to_owned();
        record["head"] = json!(head);
        record["state"] = json!("advancing_head");
        self.save_integration(&record)?;
        coord.git(&["update-ref", "HEAD", &head, &base])?;
        coord.git(&["merge", "--quit"])?;
        record["state"] = json!("resolved");
        record["hooks_run"] = json!(false);
        self.save_integration(&record)?;
        Ok(record)
    }

    pub(crate) fn integration_complete(
        &self,
        request: IntegrationCompleteRequest,
    ) -> Result<Value> {
        let _guard = self.lock("git-mutation.lock")?;
        let (mut record, coord) = self.integration_owner(
            &request.integration_id,
            &request.session_id,
            &request.lease_token,
        )?;
        ensure!(
            record["state"] == "resolved"
                || record["state"] == "completed"
                || record["state"] == "already_integrated",
            "resolve the integration before verification"
        );
        let head = record["head"].as_str().context("missing resolved HEAD")?;
        coord.require_delivery_verification(&request.verification_id, head)?;
        record["state"] = json!("completed");
        record["verification_id"] = json!(request.verification_id);
        record["note"] = json!(
            "Validated integration retained on its own branch. Publish it as a PR to incorporate it under remote protections; no checked-out main worktree was overwritten."
        );
        self.save_integration(&record)?;
        Ok(record)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::{ClaimRequest, CommitPlanRequest, SessionStartRequest};
    use crate::execution::ExecutionControl;

    fn git(root: &std::path::Path, args: &[&str]) -> String {
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
        String::from_utf8(output.stdout).unwrap()
    }

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q"]);
        git(dir.path(), &["config", "user.name", "Test"]);
        git(dir.path(), &["config", "user.email", "test@example.test"]);
        fs::write(dir.path().join("a"), "original\n").unwrap();
        fs::write(dir.path().join("b"), "original\n").unwrap();
        git(dir.path(), &["add", "a", "b"]);
        git(dir.path(), &["commit", "-qm", "initial"]);
        dir
    }

    fn session(coord: &Coordinator) -> SessionAuth {
        let start = coord
            .start(SessionStartRequest {
                owner: "test".into(),
                intent: "deliver a coherent change".into(),
                work_ids: vec![],
                isolate: false,
                ttl_seconds: 600,
            })
            .unwrap();
        let auth = SessionAuth {
            session_id: start["session"]["id"].as_str().unwrap().into(),
            lease_token: start["lease_token"].as_str().unwrap().into(),
        };
        coord
            .claim(ClaimRequest {
                session_id: auth.session_id.clone(),
                lease_token: auth.lease_token.clone(),
                paths: vec!["a".into(), "new [file]".into()],
            })
            .unwrap();
        auth
    }

    fn plan(coord: &Coordinator, auth: &SessionAuth) -> Value {
        coord
            .plan_commits(CommitPlanRequest {
                session_id: auth.session_id.clone(),
                lease_token: auth.lease_token.clone(),
                groups: vec![
                    CommitGroup {
                        message: "Update a".into(),
                        paths: vec!["a".into()],
                    },
                    CommitGroup {
                        message: "Add new file".into(),
                        paths: vec!["new [file]".into()],
                    },
                ],
            })
            .unwrap()
    }

    fn execute(auth: &SessionAuth, plan: &Value) -> CommitExecuteRequest {
        CommitExecuteRequest {
            session_id: auth.session_id.clone(),
            lease_token: auth.lease_token.clone(),
            plan_id: plan["plan_id"].as_str().unwrap().into(),
        }
    }

    #[test]
    fn exact_tree_commits_preserve_unrelated_staging_and_replay_once() {
        let dir = fixture();
        let coord = Coordinator::open(dir.path(), ExecutionControl::default()).unwrap();
        let auth = session(&coord);
        fs::write(dir.path().join("a"), "updated\n").unwrap();
        fs::write(dir.path().join("new [file]"), "new\n").unwrap();
        fs::write(dir.path().join("b"), "peer staged\n").unwrap();
        git(dir.path(), &["add", "b"]);
        let plan = plan(&coord, &auth);
        let result = coord.execute_commits(execute(&auth, &plan)).unwrap();
        assert_eq!(result["state"], "completed");
        assert_eq!(result["commits"].as_array().unwrap().len(), 2);
        assert_eq!(git(dir.path(), &["show", "HEAD:b"]), "original\n");
        assert_eq!(git(dir.path(), &["show", ":b"]), "peer staged\n");
        assert_eq!(git(dir.path(), &["diff", "--cached", "--name-only"]), "b\n");
        assert_eq!(git(dir.path(), &["show", "HEAD:new [file]"]), "new\n");
        assert_eq!(
            coord.execute_commits(execute(&auth, &plan)).unwrap(),
            result
        );
        let chunk = coord
            .chunk_create(ChunkCreateRequest {
                session_id: auth.session_id.clone(),
                lease_token: auth.lease_token.clone(),
                title: "Two cohesive changes".into(),
                summary: "Update a and introduce a file".into(),
                plan_id: plan["plan_id"].as_str().unwrap().into(),
                validation_ids: vec![],
            })
            .unwrap();
        assert_eq!(
            coord.chunk_get(chunk["id"].as_str().unwrap()).unwrap(),
            chunk
        );
        assert_eq!(coord.chunk_list(1, 0).unwrap()["page"]["total"], 1);
    }

    #[test]
    fn changed_files_or_parent_refuse_execution_without_advancing_head() {
        let dir = fixture();
        let coord = Coordinator::open(dir.path(), ExecutionControl::default()).unwrap();
        let auth = session(&coord);
        fs::write(dir.path().join("a"), "planned\n").unwrap();
        fs::write(dir.path().join("new [file]"), "planned\n").unwrap();
        let plan = plan(&coord, &auth);
        let head = git(dir.path(), &["rev-parse", "HEAD"]);
        fs::write(dir.path().join("a"), "later edits\n").unwrap();
        assert!(coord.execute_commits(execute(&auth, &plan)).is_err());
        assert_eq!(git(dir.path(), &["rev-parse", "HEAD"]), head);
        fs::write(dir.path().join("a"), "planned\n").unwrap();
        git(
            dir.path(),
            &["commit", "--allow-empty", "-qm", "peer commit"],
        );
        let head = git(dir.path(), &["rev-parse", "HEAD"]);
        assert!(coord.execute_commits(execute(&auth, &plan)).is_err());
        assert_eq!(git(dir.path(), &["rev-parse", "HEAD"]), head);
    }

    #[test]
    fn conflict_preview_is_exact_and_leaves_worktree_and_index_untouched() {
        let dir = fixture();
        let initial = git(dir.path(), &["rev-parse", "HEAD"]);
        git(dir.path(), &["checkout", "-qb", "feature"]);
        fs::write(dir.path().join("a"), "feature\n").unwrap();
        git(dir.path(), &["commit", "-qam", "feature"]);
        git(dir.path(), &["checkout", "-qb", "base", initial.trim()]);
        fs::write(dir.path().join("a"), "base\n").unwrap();
        git(dir.path(), &["commit", "-qam", "base"]);
        fs::write(dir.path().join("b"), "dirty\n").unwrap();
        let before = git(dir.path(), &["status", "--porcelain"]);
        let coord = Coordinator::open(dir.path(), ExecutionControl::default()).unwrap();
        let result = coord
            .integration_preview(IntegrationPreviewRequest {
                source_ref: "feature".into(),
                base_ref: "base".into(),
            })
            .unwrap();
        assert_eq!(result["clean"], false);
        assert_eq!(result["conflicting_paths"], json!(["a"]));
        assert_eq!(git(dir.path(), &["status", "--porcelain"]), before);
        let clean = coord
            .integration_preview(IntegrationPreviewRequest {
                source_ref: "base".into(),
                base_ref: initial.trim().into(),
            })
            .unwrap();
        assert_eq!(clean["clean"], true);
        assert_eq!(clean["conflicting_paths"], json!([]));
    }

    #[test]
    fn isolated_resolution_recovers_and_requires_current_verification() {
        use crate::verification::{BuildProfile, CheckKind, VerificationPlanRequest};
        let dir = fixture();
        fs::write(dir.path().join("Cargo.toml"), "[package]\nname='integration_fixture'\nversion='0.1.0'\nedition='2024'\n[lib]\npath='lib.rs'\n").unwrap();
        fs::write(
            dir.path().join("lib.rs"),
            "pub fn value() -> u8 {\n    1\n}\n",
        )
        .unwrap();
        fs::write(dir.path().join(".gitignore"), "target/\n").unwrap();
        let output = Command::new("cargo")
            .current_dir(dir.path())
            .args(["generate-lockfile", "--offline"])
            .output()
            .unwrap();
        assert!(output.status.success());
        git(
            dir.path(),
            &["add", "Cargo.toml", "lib.rs", ".gitignore", "Cargo.lock"],
        );
        git(dir.path(), &["commit", "-qm", "Rust fixture"]);
        let initial = git(dir.path(), &["rev-parse", "HEAD"]);
        git(dir.path(), &["checkout", "-qb", "feature"]);
        fs::write(dir.path().join("a"), "feature\n").unwrap();
        git(dir.path(), &["commit", "-qam", "feature"]);
        git(dir.path(), &["checkout", "-qb", "base", initial.trim()]);
        fs::write(dir.path().join("a"), "base\n").unwrap();
        git(dir.path(), &["commit", "-qam", "base"]);
        fs::write(dir.path().join("b"), "peer dirty\n").unwrap();
        let original_head = git(dir.path(), &["rev-parse", "HEAD"]);
        let original_status = git(dir.path(), &["status", "--porcelain"]);
        let coord = Coordinator::open(dir.path(), ExecutionControl::default()).unwrap();
        let auth = session(&coord);
        let preview = coord
            .integration_preview(IntegrationPreviewRequest {
                source_ref: "feature".into(),
                base_ref: "base".into(),
            })
            .unwrap();
        let record = coord
            .integration_start(IntegrationStartRequest {
                session_id: auth.session_id.clone(),
                lease_token: auth.lease_token.clone(),
                preview_id: preview["id"].as_str().unwrap().into(),
            })
            .unwrap();
        assert_eq!(record["state"], "conflicted");
        let integration_path = std::path::Path::new(record["worktree"].as_str().unwrap());
        let request = || IntegrationResolveRequest {
            session_id: auth.session_id.clone(),
            lease_token: auth.lease_token.clone(),
            integration_id: record["id"].as_str().unwrap().into(),
            message: "Resolve both changes".into(),
        };
        assert!(coord.integration_resolve(request()).is_err());
        fs::write(integration_path.join("a"), "resolved both\n").unwrap();
        git(integration_path, &["add", "a"]);
        let mut resolved = coord.integration_resolve(request()).unwrap();
        assert_eq!(resolved["state"], "resolved");
        assert_eq!(
            git(
                integration_path,
                &["rev-list", "--parents", "-n", "1", "HEAD"]
            )
            .split_whitespace()
            .count(),
            3
        );
        resolved["state"] = json!("advancing_head");
        coord.save_integration(&resolved).unwrap();
        let recovered = coord.integration_resolve(request()).unwrap();
        assert_eq!(recovered["state"], "resolved");
        assert_eq!(recovered["head"], resolved["head"]);
        let integration = Coordinator::open(integration_path, ExecutionControl::default()).unwrap();
        let plan = integration
            .verification_plan(VerificationPlanRequest {
                profile: BuildProfile::default(),
                checks: vec![
                    CheckKind::Format,
                    CheckKind::Check,
                    CheckKind::Test,
                    CheckKind::Clippy,
                ],
                offline: true,
                test_filter: None,
                deny_warnings: Some(true),
            })
            .unwrap();
        let checks = integration
            .verification_run(plan["id"].as_str().unwrap())
            .unwrap();
        assert_eq!(checks["delivery_eligible"], true, "{checks}");
        let complete = || IntegrationCompleteRequest {
            session_id: auth.session_id.clone(),
            lease_token: auth.lease_token.clone(),
            integration_id: record["id"].as_str().unwrap().into(),
            verification_id: checks["id"].as_str().unwrap().into(),
        };
        fs::write(integration_path.join("a"), "unverified\n").unwrap();
        assert!(coord.integration_complete(complete()).is_err());
        fs::write(integration_path.join("a"), "resolved both\n").unwrap();
        assert_eq!(
            coord.integration_complete(complete()).unwrap()["state"],
            "completed"
        );
        assert_eq!(git(dir.path(), &["rev-parse", "HEAD"]), original_head);
        assert_eq!(git(dir.path(), &["status", "--porcelain"]), original_status);
        assert_eq!(
            fs::read_to_string(dir.path().join("b")).unwrap(),
            "peer dirty\n"
        );
    }
}

use anyhow::{Context, Result};
use rmcp::{
    Json, ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    tool, tool_handler, tool_router,
};
use rust_repo_intelligence::observatory::{
    ArchitectureFindingRequest, ArchitectureRequest, CheckpointCreateRequest,
    CheckpointDiffRequest, CheckpointRestoreRequest, ConsultRequest, ContextRequest,
    FindingEvidenceRequest, MemorySearchRequest, Observatory, PrepareRequest, ProblemUpdateRequest,
    PromoteFindingRequest, QualityMergeRequest, QualityReviewRequest, ResearchListRequest,
    ResearchStartRequest, ResearchSubmitRequest, ReviewFindingRequest, ScopeRequest, SearchRequest,
    SymbolRelationRequest, TargetRequest, TaskListRequest, ValidateRequest, WorkCreateRequest,
    WorkUpdateRequest,
};
use rust_repo_intelligence::{RecordDecision, RecordSteering, ValidationOutcomeInput};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{env, path::PathBuf};

#[tokio::main]
async fn main() -> Result<()> {
    let observatory = Observatory::open(workspace_arg()?)?;
    let running = CrustyServer::new(observatory)
        .serve(rmcp::transport::stdio())
        .await?;
    match running.waiting().await {
        Ok(_) => Ok(()),
        Err(error) if error.to_string().contains("connection closed") => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn workspace_arg() -> Result<PathBuf> {
    let mut args = env::args().skip(1);
    match (args.next().as_deref(), args.next()) {
        (Some("--workspace"), Some(path)) => Ok(PathBuf::from(path)),
        (None, _) => env::current_dir().context("reading current directory"),
        _ => anyhow::bail!("usage: rust-repo-intelligence [--workspace PATH]"),
    }
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct IdRequest {
    id: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RunRequest {
    run_id: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ContextIdRequest {
    context_id: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct IndexRefreshRequest {
    scope: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct AuditListRequest {
    limit: Option<usize>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct FindingListRequest {
    /// `proposed`, `accepted`, `dismissed`, `needs_evidence`, or `promoted`.
    status: Option<String>,
    query: Option<String>,
    limit: Option<usize>,
    /// Rows to skip. Use the `page.next_offset` from a previous call.
    offset: Option<usize>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct WorkListRequest {
    query: Option<String>,
    limit: Option<usize>,
    /// Rows to skip. Use the `page.next_offset` from a previous call.
    offset: Option<usize>,
}

#[derive(Clone)]
struct CrustyServer {
    observatory: Observatory,
    #[allow(dead_code)] // Read by the rmcp-generated ServerHandler implementation.
    tool_router: ToolRouter<Self>,
}

impl CrustyServer {
    fn new(observatory: Observatory) -> Self {
        Self {
            observatory,
            tool_router: Self::tool_router(),
        }
    }

    fn value(result: anyhow::Result<Value>) -> Result<Json<Value>, String> {
        result.map(Json).map_err(|error| format!("{error:#}"))
    }

    /// Runs a synchronous Observatory call off the async runtime.
    ///
    /// Every Observatory method opens SQLite and most shell out to `git`, so
    /// calling them directly from a tool handler blocks a tokio worker for the
    /// duration — seconds, on a large repository.
    async fn offload<F>(operation: F) -> Result<Json<Value>, String>
    where
        F: FnOnce() -> anyhow::Result<Value> + Send + 'static,
    {
        let result = tokio::task::spawn_blocking(operation)
            .await
            .map_err(|error| format!("tool task failed: {error}"))?;
        Self::value(result)
    }
}

#[tool_router(router = tool_router)]
impl CrustyServer {
    #[tool(
        name = "repo.consult",
        description = "Mandatory first-call preflight for every repository-scoped user request. Returns global and topical decisions, steering, design and quality constraints, restrictions, governing documentation, workflows, lifecycle risks, runtime contracts, and known work before planning, answering, or acting."
    )]
    async fn repo_consult(
        &self,
        Parameters(request): Parameters<ConsultRequest>,
    ) -> Result<Json<Value>, String> {
        let observatory = self.observatory.clone();
        Self::offload(move || observatory.consult(&request.topic, request.budget)).await
    }

    #[tool(
        name = "repo.search",
        description = "Search Rust source. exact reads the live worktree without refreshing; broad reads the published snapshot and labels freshness."
    )]
    async fn repo_search(
        &self,
        Parameters(request): Parameters<SearchRequest>,
    ) -> Result<Json<Value>, String> {
        Self::value(self.observatory.search(request).await)
    }

    #[tool(
        name = "repo.context",
        description = "Build a bounded evidence context from the last published snapshot without implicit refresh."
    )]
    async fn repo_context(
        &self,
        Parameters(request): Parameters<ContextRequest>,
    ) -> Result<Json<Value>, String> {
        Self::value(self.observatory.context(request).await)
    }

    #[tool(
        name = "repo.matrix",
        description = "Return the freshness-labelled Cargo package, target, feature, and bounded verification matrix from the published snapshot."
    )]
    async fn repo_matrix(&self) -> Result<Json<Value>, String> {
        Self::value(self.observatory.matrix())
    }

    #[tool(
        name = "repo.architecture",
        description = "Build a bounded live-worktree architecture map with versioned facts, qualified advisory findings, counter-evidence, confidence, and explicit limitations."
    )]
    async fn repo_architecture(
        &self,
        Parameters(request): Parameters<ArchitectureRequest>,
    ) -> Result<Json<Value>, String> {
        let observatory = self.observatory.clone();
        Self::offload(move || observatory.architecture(request)).await
    }

    #[tool(
        name = "audit.start",
        description = "Start a durable contextual Rust architecture audit against the live worktree; returns immediately with a task id and persists the completed report."
    )]
    async fn audit_start(
        &self,
        Parameters(request): Parameters<ArchitectureRequest>,
    ) -> Result<Json<Value>, String> {
        Self::value(self.observatory.start_architecture_audit(request))
    }

    #[tool(
        name = "audit.get",
        description = "Retrieve one persisted snapshot-scoped architecture audit by its exact report id."
    )]
    async fn audit_get(
        &self,
        Parameters(request): Parameters<IdRequest>,
    ) -> Result<Json<Value>, String> {
        let observatory = self.observatory.clone();
        Self::offload(move || observatory.architecture_audit_get(&request.id)).await
    }

    #[tool(
        name = "audit.finding.propose",
        description = "Explicitly copy one persisted audit finding into the human review inbox as a proposal; this never accepts, activates, or promotes it automatically."
    )]
    async fn audit_finding_propose(
        &self,
        Parameters(request): Parameters<ArchitectureFindingRequest>,
    ) -> Result<Json<Value>, String> {
        let observatory = self.observatory.clone();
        Self::offload(move || observatory.architecture_finding_propose(request)).await
    }

    #[tool(
        name = "audit.list",
        description = "List bounded summaries of persisted architecture audits, newest first, for comparison and recovery."
    )]
    async fn audit_list(
        &self,
        Parameters(request): Parameters<AuditListRequest>,
    ) -> Result<Json<Value>, String> {
        let observatory = self.observatory.clone();
        Self::offload(move || observatory.architecture_audit_list(request.limit.unwrap_or(20)))
            .await
    }

    #[tool(
        name = "symbol.relations",
        description = "Resolve callers, references, implementations, or the definition of a symbol from the published snapshot. Returns file:line locations with the evidence channel that produced them."
    )]
    async fn symbol_relations(
        &self,
        Parameters(request): Parameters<SymbolRelationRequest>,
    ) -> Result<Json<Value>, String> {
        let observatory = self.observatory.clone();
        Self::offload(move || observatory.symbol_relations(request)).await
    }

    #[tool(
        name = "repo.history",
        description = "Commit history and co-change neighbours for a symbol or path, from indexed Git evidence."
    )]
    async fn repo_history(
        &self,
        Parameters(request): Parameters<TargetRequest>,
    ) -> Result<Json<Value>, String> {
        let observatory = self.observatory.clone();
        Self::offload(move || observatory.history(&request.target)).await
    }

    #[tool(
        name = "repo.explain",
        description = "Explain one symbol or concept: definitions, lifecycle, human decisions, related work, and bounded source slices."
    )]
    async fn repo_explain(
        &self,
        Parameters(request): Parameters<TargetRequest>,
    ) -> Result<Json<Value>, String> {
        let observatory = self.observatory.clone();
        Self::offload(move || observatory.explain(&request.target, request.budget)).await
    }

    #[tool(
        name = "repo.constraints",
        description = "Human decisions, steerings, learned quality constraints, and lifecycle risks bearing on a proposed change."
    )]
    async fn repo_constraints(
        &self,
        Parameters(request): Parameters<TargetRequest>,
    ) -> Result<Json<Value>, String> {
        let observatory = self.observatory.clone();
        Self::offload(move || observatory.constraints(&request.target)).await
    }

    #[tool(
        name = "repo.cleanup_candidates",
        description = "Private symbols with no indexed inbound references. Static inference only; confirm with the compiler before deleting."
    )]
    async fn repo_cleanup_candidates(
        &self,
        Parameters(request): Parameters<ScopeRequest>,
    ) -> Result<Json<Value>, String> {
        let observatory = self.observatory.clone();
        Self::offload(move || observatory.cleanup_candidates(request.scope.as_deref())).await
    }

    #[tool(
        name = "repo.obsolete_candidates",
        description = "Superseded or legacy symbols retained as lifecycle evidence, with their canonical replacements."
    )]
    async fn repo_obsolete_candidates(
        &self,
        Parameters(request): Parameters<ScopeRequest>,
    ) -> Result<Json<Value>, String> {
        let observatory = self.observatory.clone();
        Self::offload(move || {
            observatory.obsolete_candidates(request.scope.as_deref(), request.limit)
        })
        .await
    }

    #[tool(
        name = "decision.record",
        description = "Record a human architectural decision so prepared changes and validation can cite it."
    )]
    async fn decision_record(
        &self,
        Parameters(request): Parameters<RecordDecision>,
    ) -> Result<Json<Value>, String> {
        let observatory = self.observatory.clone();
        Self::offload(move || observatory.decision_record(request)).await
    }

    #[tool(
        name = "decision.list",
        description = "List human architectural decisions relevant to a symbol, path, or concept."
    )]
    async fn decision_list(
        &self,
        Parameters(request): Parameters<ScopeRequest>,
    ) -> Result<Json<Value>, String> {
        let observatory = self.observatory.clone();
        Self::offload(move || observatory.decision_list(request.scope.as_deref(), request.limit))
            .await
    }

    #[tool(
        name = "steering.record",
        description = "Record a durable human steering instruction that should shape later changes."
    )]
    async fn steering_record(
        &self,
        Parameters(request): Parameters<RecordSteering>,
    ) -> Result<Json<Value>, String> {
        let observatory = self.observatory.clone();
        Self::offload(move || observatory.steering_record(request)).await
    }

    #[tool(
        name = "steering.list",
        description = "List durable human steering instructions, optionally filtered by symbol, path, or concept."
    )]
    async fn steering_list(
        &self,
        Parameters(request): Parameters<ScopeRequest>,
    ) -> Result<Json<Value>, String> {
        let observatory = self.observatory.clone();
        Self::offload(move || observatory.steering_list(request.scope.as_deref(), request.limit))
            .await
    }

    #[tool(
        name = "problem.record",
        description = "Explicitly record a reported defect in repository memory. Proposed quality constraints stay proposed until a human reviews them."
    )]
    async fn problem_record(
        &self,
        Parameters(request): Parameters<rust_repo_intelligence::ProblemInput>,
    ) -> Result<Json<Value>, String> {
        let observatory = self.observatory.clone();
        Self::offload(move || observatory.problem_record(request)).await
    }

    #[tool(
        name = "quality.propose",
        description = "Propose a learned quality constraint for human review. Proposing never activates it; quality.review remains the only activation path."
    )]
    async fn quality_propose(
        &self,
        Parameters(request): Parameters<rust_repo_intelligence::QualityConstraintInput>,
    ) -> Result<Json<Value>, String> {
        let observatory = self.observatory.clone();
        Self::offload(move || observatory.quality_propose(request)).await
    }

    #[tool(
        name = "quality.merge",
        description = "With explicit human confirmation, merge one learned quality constraint into another, preserving both problem links."
    )]
    async fn quality_merge(
        &self,
        Parameters(request): Parameters<QualityMergeRequest>,
    ) -> Result<Json<Value>, String> {
        if !request.confirm_human {
            return Err("human confirmation is required".into());
        }
        let observatory = self.observatory.clone();
        Self::offload(move || {
            observatory.quality_merge(&request.source_constraint_id, &request.target_constraint_id)
        })
        .await
    }

    #[tool(
        name = "checkpoint.create",
        description = "Create a Git checkpoint ref for the current worktree so a change can be compared or recovered later. Never moves HEAD."
    )]
    async fn checkpoint_create(
        &self,
        Parameters(request): Parameters<CheckpointCreateRequest>,
    ) -> Result<Json<Value>, String> {
        let observatory = self.observatory.clone();
        Self::offload(move || {
            observatory.checkpoint_create(&request.label, request.include_untracked)
        })
        .await
    }

    #[tool(
        name = "checkpoint.list",
        description = "List Crusty Git checkpoints, newest first."
    )]
    async fn checkpoint_list(
        &self,
        Parameters(request): Parameters<ScopeRequest>,
    ) -> Result<Json<Value>, String> {
        let observatory = self.observatory.clone();
        Self::offload(move || observatory.checkpoint_list(request.limit)).await
    }

    #[tool(
        name = "checkpoint.diff",
        description = "Show the bounded diff between a checkpoint and the current worktree."
    )]
    async fn checkpoint_diff(
        &self,
        Parameters(request): Parameters<CheckpointDiffRequest>,
    ) -> Result<Json<Value>, String> {
        let observatory = self.observatory.clone();
        Self::offload(move || observatory.checkpoint_diff(&request.reference, request.max_bytes))
            .await
    }

    #[tool(
        name = "checkpoint.restore",
        description = "Create a new branch at a checkpoint. Restoring never moves HEAD, discards work, or rewrites history."
    )]
    async fn checkpoint_restore(
        &self,
        Parameters(request): Parameters<CheckpointRestoreRequest>,
    ) -> Result<Json<Value>, String> {
        let observatory = self.observatory.clone();
        Self::offload(move || {
            observatory.checkpoint_restore_branch(&request.reference, &request.branch)
        })
        .await
    }

    #[tool(
        name = "change.prepare",
        description = "Start snapshot-labelled change preparation without implicit refresh; returns immediately with a task id."
    )]
    async fn change_prepare(
        &self,
        Parameters(request): Parameters<PrepareRequest>,
    ) -> Result<Json<Value>, String> {
        Self::value(self.observatory.start_prepare_change(request))
    }

    #[tool(
        name = "change.get",
        description = "Retrieve a persisted prepared-change context by exact context ID without refreshing the index."
    )]
    async fn change_get(
        &self,
        Parameters(request): Parameters<ContextIdRequest>,
    ) -> Result<Json<Value>, String> {
        Self::value(self.observatory.change_get(&request.context_id))
    }

    #[tool(
        name = "change.validate",
        description = "Start diff validation against prepared evidence. Returns a task id and never waits for index refresh."
    )]
    async fn change_validate(
        &self,
        Parameters(request): Parameters<ValidateRequest>,
    ) -> Result<Json<Value>, String> {
        Self::value(self.observatory.start_validate_change(request))
    }

    #[tool(
        name = "index.status",
        description = "Show live versus indexed freshness, the published generation, and current refresh task."
    )]
    async fn index_status(&self) -> Result<Json<Value>, String> {
        Self::value(self.observatory.index_status())
    }

    #[tool(
        name = "index.refresh",
        description = "Start an explicit background index refresh under the single-writer publisher lease; returns immediately with a task id."
    )]
    async fn index_refresh(
        &self,
        Parameters(request): Parameters<IndexRefreshRequest>,
    ) -> Result<Json<Value>, String> {
        Self::value(self.observatory.start_index_refresh(request.scope))
    }

    #[tool(
        name = "task.list",
        description = "List bounded summaries of durable change, refresh, architecture-audit, and research tasks so interrupted sessions can recover IDs and result availability."
    )]
    async fn task_list(
        &self,
        Parameters(request): Parameters<TaskListRequest>,
    ) -> Result<Json<Value>, String> {
        Self::value(self.observatory.task_list(
            request.status.as_deref(),
            request.kind.as_deref(),
            request.limit,
        ))
    }

    #[tool(
        name = "task.get",
        description = "Poll a durable change, refresh, architecture-audit, or research task for progress, result, or failure."
    )]
    async fn task_get(
        &self,
        Parameters(request): Parameters<IdRequest>,
    ) -> Result<Json<Value>, String> {
        Self::value(self.observatory.task_get(&request.id))
    }

    #[tool(
        name = "task.cancel",
        description = "Request cooperative cancellation at the next safe boundary."
    )]
    async fn task_cancel(
        &self,
        Parameters(request): Parameters<IdRequest>,
    ) -> Result<Json<Value>, String> {
        Self::value(self.observatory.task_cancel(&request.id))
    }

    #[tool(
        name = "research.start",
        description = "Start budgeted local research and create a primary-first web_search packet for the attached agent. No external connector is used."
    )]
    async fn research_start(
        &self,
        Parameters(request): Parameters<ResearchStartRequest>,
    ) -> Result<Json<Value>, String> {
        Self::value(self.observatory.research_start(request))
    }

    #[tool(
        name = "research.list",
        description = "List durable research runs with status, budget consumption, schedule, and task IDs for session recovery."
    )]
    async fn research_list(
        &self,
        Parameters(request): Parameters<ResearchListRequest>,
    ) -> Result<Json<Value>, String> {
        Self::value(self.observatory.research_list(
            request.status.as_deref(),
            request.query.as_deref(),
            request.limit,
        ))
    }

    #[tool(
        name = "research.get",
        description = "Get a research run, its durable task, budget consumption, packet, and resulting findings."
    )]
    async fn research_get(
        &self,
        Parameters(request): Parameters<RunRequest>,
    ) -> Result<Json<Value>, String> {
        Self::value(self.observatory.research_get(&request.run_id))
    }

    #[tool(
        name = "research.packet",
        description = "Return local evidence, bounded web_search queries, and the attached-agent submission contract."
    )]
    async fn research_packet(
        &self,
        Parameters(request): Parameters<RunRequest>,
    ) -> Result<Json<Value>, String> {
        Self::value(self.observatory.research_packet(&request.run_id))
    }

    #[tool(
        name = "research.submit",
        description = "Submit primary-first web_search evidence and proposed technical, product, or design findings. Findings do not become work automatically."
    )]
    async fn research_submit(
        &self,
        Parameters(request): Parameters<ResearchSubmitRequest>,
    ) -> Result<Json<Value>, String> {
        Self::value(self.observatory.research_submit(request))
    }

    #[tool(
        name = "research.cancel",
        description = "Cancel a research run. Completed findings remain reviewable evidence."
    )]
    async fn research_cancel(
        &self,
        Parameters(request): Parameters<RunRequest>,
    ) -> Result<Json<Value>, String> {
        Self::value(self.observatory.research_cancel(&request.run_id))
    }

    #[tool(
        name = "finding.list",
        description = "List proposed and reviewed evidence-backed improvement findings."
    )]
    async fn finding_list(
        &self,
        Parameters(request): Parameters<FindingListRequest>,
    ) -> Result<Json<Value>, String> {
        let observatory = self.observatory.clone();
        Self::offload(move || {
            observatory.finding_page(
                request.status.as_deref(),
                request.query.as_deref(),
                request.limit.unwrap_or(100),
                request.offset.unwrap_or(0),
            )
        })
        .await
    }

    #[tool(
        name = "finding.get",
        description = "Get a finding and its local or web provenance."
    )]
    async fn finding_get(
        &self,
        Parameters(request): Parameters<IdRequest>,
    ) -> Result<Json<Value>, String> {
        Self::value(self.observatory.finding_get(&request.id))
    }

    #[tool(
        name = "finding.review",
        description = "Record a human finding decision: accepted, dismissed, or needs_evidence."
    )]
    async fn finding_review(
        &self,
        Parameters(request): Parameters<ReviewFindingRequest>,
    ) -> Result<Json<Value>, String> {
        Self::value(self.observatory.finding_review(request))
    }

    #[tool(
        name = "finding.evidence.add",
        description = "Append qualified evidence to a finding marked needs_evidence and reopen it as proposed for human review."
    )]
    async fn finding_evidence_add(
        &self,
        Parameters(request): Parameters<FindingEvidenceRequest>,
    ) -> Result<Json<Value>, String> {
        Self::value(self.observatory.finding_add_evidence(request))
    }

    #[tool(
        name = "finding.promote",
        description = "With explicit human confirmation, promote an accepted finding into human-owned project work."
    )]
    async fn finding_promote(
        &self,
        Parameters(request): Parameters<PromoteFindingRequest>,
    ) -> Result<Json<Value>, String> {
        Self::value(self.observatory.finding_promote(request))
    }

    #[tool(
        name = "work.list",
        description = "Search the single durable work store by exact id, title, scope, or evidence."
    )]
    async fn work_list(
        &self,
        Parameters(request): Parameters<WorkListRequest>,
    ) -> Result<Json<Value>, String> {
        let observatory = self.observatory.clone();
        Self::offload(move || {
            observatory.work_page(
                request.query.as_deref(),
                request.limit.unwrap_or(100),
                request.offset.unwrap_or(0),
            )
        })
        .await
    }

    #[tool(
        name = "work.get",
        description = "Get one human-owned or migrated work item by exact id."
    )]
    async fn work_get(
        &self,
        Parameters(request): Parameters<IdRequest>,
    ) -> Result<Json<Value>, String> {
        Self::value(self.observatory.work_get(&request.id))
    }

    #[tool(
        name = "work.recommend",
        description = "Recommend only accepted or active, human-owned work whose exact work-item dependencies and blockers are complete."
    )]
    async fn work_recommend(
        &self,
        Parameters(request): Parameters<WorkListRequest>,
    ) -> Result<Json<Value>, String> {
        Self::value(self.observatory.work_recommend(request.query.as_deref()))
    }

    #[tool(
        name = "work.create",
        description = "Create human-owned project work, optionally linked to exact dependency and blocker work IDs, with explicit confirmation. Crusty findings cannot call this autonomously."
    )]
    async fn work_create(
        &self,
        Parameters(request): Parameters<WorkCreateRequest>,
    ) -> Result<Json<Value>, String> {
        Self::value(self.observatory.work_create(request))
    }

    #[tool(
        name = "work.update",
        description = "Update any human-editable project-work field and replace or clear exact dependency and blocker work IDs with explicit human confirmation."
    )]
    async fn work_update(
        &self,
        Parameters(request): Parameters<WorkUpdateRequest>,
    ) -> Result<Json<Value>, String> {
        Self::value(self.observatory.work_update(request))
    }

    #[tool(
        name = "memory.search",
        description = "Recover repository-scoped user prompts from Codex primary and side-session history and search preserved legacy decisions, steerings, problems, and quality constraints. Read-only results never become work without explicit human creation."
    )]
    async fn memory_search(
        &self,
        Parameters(request): Parameters<MemorySearchRequest>,
    ) -> Result<Json<Value>, String> {
        let observatory = self.observatory.clone();
        let result = tokio::task::spawn_blocking(move || observatory.memory_search(request))
            .await
            .map_err(|error| format!("memory search task failed: {error}"))?;
        Self::value(result)
    }

    #[tool(
        name = "problem.list",
        description = "List or search durable automatically or explicitly captured problem records without refreshing the index."
    )]
    async fn problem_list(
        &self,
        Parameters(request): Parameters<WorkListRequest>,
    ) -> Result<Json<Value>, String> {
        Self::value(
            self.observatory
                .problem_list(request.query.as_deref(), request.limit.unwrap_or(100)),
        )
    }

    #[tool(
        name = "problem.get",
        description = "Get one durable problem record, including diagnosis, lifecycle, and redacted evidence."
    )]
    async fn problem_get(
        &self,
        Parameters(request): Parameters<IdRequest>,
    ) -> Result<Json<Value>, String> {
        Self::value(self.observatory.problem_get(&request.id))
    }

    #[tool(
        name = "problem.update",
        description = "Update the lifecycle, diagnosis, resolution, scope, or evidence of an existing problem record."
    )]
    async fn problem_update(
        &self,
        Parameters(request): Parameters<ProblemUpdateRequest>,
    ) -> Result<Json<Value>, String> {
        Self::value(self.observatory.problem_update(request))
    }

    #[tool(
        name = "quality.list",
        description = "List or search learned quality constraints with status, maturity, scope, and provenance."
    )]
    async fn quality_list(
        &self,
        Parameters(request): Parameters<WorkListRequest>,
    ) -> Result<Json<Value>, String> {
        Self::value(
            self.observatory
                .quality_list(request.query.as_deref(), request.limit.unwrap_or(100)),
        )
    }

    #[tool(
        name = "quality.get",
        description = "Get one learned quality constraint with its problem links, human reviews, and validation history."
    )]
    async fn quality_get(
        &self,
        Parameters(request): Parameters<IdRequest>,
    ) -> Result<Json<Value>, String> {
        Self::value(self.observatory.quality_get(&request.id))
    }

    #[tool(
        name = "quality.review",
        description = "With explicit human confirmation, activate, reject, disable, or retire a learned quality constraint and preserve the review."
    )]
    async fn quality_review(
        &self,
        Parameters(request): Parameters<QualityReviewRequest>,
    ) -> Result<Json<Value>, String> {
        Self::value(self.observatory.quality_review(request))
    }

    #[tool(
        name = "validation.queue",
        description = "Inspect repository, learned-quality, and change-specific validation obligations for a prepared context."
    )]
    async fn validation_queue(
        &self,
        Parameters(request): Parameters<ContextIdRequest>,
    ) -> Result<Json<Value>, String> {
        Self::value(self.observatory.validation_queue(&request.context_id))
    }

    #[tool(
        name = "validation.record",
        description = "Record a passed, failed, skipped, or unavailable validation outcome with durable redacted evidence."
    )]
    async fn validation_record(
        &self,
        Parameters(request): Parameters<ValidationOutcomeInput>,
    ) -> Result<Json<Value>, String> {
        Self::value(self.observatory.validation_record(request))
    }

    #[tool(
        name = "dashboard.open",
        description = "Start the authenticated loopback findings dashboard and return its one-time URL."
    )]
    async fn dashboard_open(&self) -> Result<Json<Value>, String> {
        Self::value(self.observatory.dashboard_open().await)
    }

    #[tool(
        name = "repo.authority",
        description = "Explain Crusty's evidence and ownership boundaries."
    )]
    async fn repo_authority(&self) -> Json<Value> {
        Json(json!({
            "source_and_runtime":"authoritative",
            "crusty_results":"guidance and provenance, never proof",
            "project_work":"human-owned",
            "external_connectors":[],
            "research":"local repository evidence plus primary-first web_search performed by the attached agent",
            "impact_policy":"ambiguous static evidence is visible but never promoted into likely change surfaces"
        }))
    }
}

#[tool_handler(
    name = "Crusty",
    version = "0.3.0",
    instructions = "Crusty is a Rust repository observatory. For every repository-scoped user prompt, call repo.consult first with the user's complete intent before planning, answering, or acting; do not skip consultation for design, review, questions, documentation, configuration, or non-code work. Apply relevant human decisions, steering, design and quality constraints, restrictions, governing documents, and workflows returned by the consultation. Consultation is read-only and does not replace change preparation: use change.prepare before edits and change.validate afterwards, polling both with task.get; task.list and change.get recover interrupted workflows. Use repo.architecture for a bounded live architecture map and audit.start for a durable contextual audit. Change preparation captures an architecture baseline; validation reports advisory new, worsened, and resolved findings without blocking on inferred debt. Use repo.search exact for live call-site work and symbol.relations for callers, references, implementations, and definitions; neither refreshes. When a broad read is empty, check index.status: never_published distinguishes an unbuilt index from no matches. Change preparation, validation, refresh, audit, and research are explicit durable tasks. Research uses local repository evidence plus primary-first web_search by the attached agent; no external connectors are available. Findings are proposals and only a human may review or promote them into work. Quality proposal never activates a constraint: activation, merging, finding promotion, and work writes require explicit human confirmation. Source, compiler/runtime behavior, and human ownership remain authoritative."
)]
impl ServerHandler for CrustyServer {}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::ServerHandler;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn public_contract_is_the_clean_break_observatory_api() -> Result<()> {
        let directory = tempdir()?;
        fs::write(
            directory.path().join("Cargo.toml"),
            "[package]\nname='fixture'\nversion='0.1.0'\nedition='2024'\n",
        )?;
        fs::create_dir(directory.path().join("src"))?;
        fs::write(directory.path().join("src/lib.rs"), "")?;
        let server = CrustyServer::new(Observatory::open(directory.path())?);
        let mut names = server
            .tool_router
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect::<Vec<_>>();
        names.sort();
        assert_eq!(
            names,
            [
                "audit.finding.propose",
                "audit.get",
                "audit.list",
                "audit.start",
                "change.get",
                "change.prepare",
                "change.validate",
                "checkpoint.create",
                "checkpoint.diff",
                "checkpoint.list",
                "checkpoint.restore",
                "dashboard.open",
                "decision.list",
                "decision.record",
                "finding.evidence.add",
                "finding.get",
                "finding.list",
                "finding.promote",
                "finding.review",
                "index.refresh",
                "index.status",
                "memory.search",
                "problem.get",
                "problem.list",
                "problem.record",
                "problem.update",
                "quality.get",
                "quality.list",
                "quality.merge",
                "quality.propose",
                "quality.review",
                "repo.architecture",
                "repo.authority",
                "repo.cleanup_candidates",
                "repo.constraints",
                "repo.consult",
                "repo.context",
                "repo.explain",
                "repo.history",
                "repo.matrix",
                "repo.obsolete_candidates",
                "repo.search",
                "research.cancel",
                "research.get",
                "research.list",
                "research.packet",
                "research.start",
                "research.submit",
                "steering.list",
                "steering.record",
                "symbol.relations",
                "task.cancel",
                "task.get",
                "task.list",
                "validation.queue",
                "validation.record",
                "work.create",
                "work.get",
                "work.list",
                "work.recommend",
                "work.update",
            ]
            .map(str::to_owned)
        );
        assert_eq!(server.get_info().server_info.name, "Crusty");
        Ok(())
    }
    /// Guards the schema facts an attached agent depends on. The name list
    /// alone could not see a `confirm_human` gate gaining `#[serde(default)]`,
    /// a required field becoming optional, or a tool shipping without a
    /// description.
    #[test]
    fn tool_schemas_declare_their_gates_required_fields_and_descriptions() -> Result<()> {
        let directory = tempdir()?;
        fs::write(
            directory.path().join("Cargo.toml"),
            "[package]\nname='fixture'\nversion='0.1.0'\nedition='2024'\n",
        )?;
        fs::create_dir(directory.path().join("src"))?;
        fs::write(directory.path().join("src/lib.rs"), "")?;
        let server = CrustyServer::new(Observatory::open(directory.path())?);
        let tools = server.tool_router.list_all();

        for tool in &tools {
            assert!(
                tool.description
                    .as_ref()
                    .is_some_and(|text| text.len() > 30),
                "{} needs a description an agent can route on",
                tool.name
            );
        }

        let required = |name: &str| -> Vec<String> {
            tools
                .iter()
                .find(|tool| tool.name == name)
                .unwrap_or_else(|| panic!("missing tool {name}"))
                .input_schema
                .get("required")
                .and_then(Value::as_array)
                .map(|values| {
                    values
                        .iter()
                        .filter_map(|value| value.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default()
        };

        // Every human-ownership gate must stay a required field: a default
        // would let an agent create or promote work without asserting consent.
        for name in [
            "work.create",
            "work.update",
            "finding.promote",
            "quality.review",
            "quality.merge",
        ] {
            assert!(
                required(name).contains(&"confirm_human".to_owned()),
                "{name} must require confirm_human"
            );
        }
        assert!(required("repo.search").contains(&"query".to_owned()));
        assert!(required("repo.consult").contains(&"topic".to_owned()));
        assert!(required("symbol.relations").contains(&"symbol".to_owned()));
        assert!(required("change.validate").contains(&"context_id".to_owned()));
        assert!(required("audit.finding.propose").contains(&"report_id".to_owned()));
        assert!(required("audit.finding.propose").contains(&"finding_id".to_owned()));
        assert!(required("work.update").contains(&"work_id".to_owned()));

        // Unknown fields must be refused rather than silently discarded.
        let schema = &tools
            .iter()
            .find(|tool| tool.name == "work.update")
            .expect("work.update")
            .input_schema;
        assert_eq!(
            schema.get("additionalProperties"),
            Some(&Value::Bool(false)),
            "a misnamed parameter must be an error, not a silent no-op"
        );
        Ok(())
    }

    #[test]
    fn server_instructions_require_consultation_before_all_repository_work() -> Result<()> {
        let directory = tempdir()?;
        fs::write(
            directory.path().join("Cargo.toml"),
            "[package]\nname='fixture'\nversion='0.1.0'\nedition='2024'\n",
        )?;
        fs::create_dir(directory.path().join("src"))?;
        fs::write(directory.path().join("src/lib.rs"), "")?;
        let server = CrustyServer::new(Observatory::open(directory.path())?);
        let instructions = server.get_info().instructions.unwrap_or_default();
        assert!(instructions.contains("every repository-scoped user prompt"));
        assert!(instructions.contains("call repo.consult first"));
        assert!(instructions.contains("does not replace change preparation"));
        Ok(())
    }
}

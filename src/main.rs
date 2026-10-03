use anyhow::{Context, Result};
use rmcp::{
    Json, ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    tool, tool_handler, tool_router,
};
use rust_repo_intelligence::cleanup::CleanupPlanRequest;
use rust_repo_intelligence::coordination::{
    ClaimRequest, CommitPlanRequest, HeartbeatRequest, SessionAuth, SessionListRequest,
    SessionStartRequest,
};
use rust_repo_intelligence::delivery::{
    ChunkCreateRequest, CommitExecuteRequest, IntegrationCompleteRequest,
    IntegrationPreviewRequest, IntegrationResolveRequest, IntegrationStartRequest,
};
use rust_repo_intelligence::domain::{DomainModelRequest, DomainReviewRequest};
use rust_repo_intelligence::github::{
    DeliveryPolicyRequest, GithubListRequest, GithubMergeRequest, GithubPrRequest,
    GithubPublishRequest, GithubReadyRequest, GithubRepositoryRequest, GithubReviewRequest,
};
use rust_repo_intelligence::guidance::GuidanceRequest;
use rust_repo_intelligence::live_semantics::{DiagnosticsRequest, SemanticRequest};
use rust_repo_intelligence::observatory::{
    ArchitectureFindingRequest, ArchitectureRequest, CheckpointCreateRequest,
    CheckpointDiffRequest, CheckpointRestoreRequest, ConsultRequest, ContextRequest,
    DecisionListRequest, FindingEvidenceRequest, MAX_TASK_WAIT, MemorySearchRequest, Observatory,
    PrepareRequest, ProblemUpdateRequest, PromoteFindingRequest, QualityMergeRequest,
    QualityReviewRequest, ResearchListRequest, ResearchStartRequest, ResearchSubmitRequest,
    ReviewFindingRequest, ScopeRequest, SearchRequest, SteeringListRequest, SymbolRelationRequest,
    TargetRequest, TaskListRequest, TaskWait, ValidateRequest, WorkCreateRequest,
    WorkUpdateRequest,
};
use rust_repo_intelligence::performance::{PerformanceContractRequest, PerformanceMeasureRequest};
use rust_repo_intelligence::verification::VerificationPlanRequest;
use rust_repo_intelligence::{
    RecordDecision, RecordSteering, RetireDecision, RetireSteering, ValidationOutcomeInput,
};
use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Deserializer, de::DeserializeOwned, de::Error as _};
use serde_json::{Value, json};
use std::{borrow::Cow, env, path::PathBuf, time::Duration};
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> Result<()> {
    let observatory = Observatory::open(workspace_arg()?)?;
    let resolution = observatory.root_resolution();
    eprintln!(
        "crusty: workspace root {} ({})",
        observatory.root().display(),
        resolution["resolution"].as_str().unwrap_or("as_given")
    );
    // The refresher's first check runs on its own thread after a short delay,
    // so MCP initialisation never waits for indexing.
    let auto_refresh = observatory.start_auto_refresh();
    // rust-analyzer stays off until semantic.enable unless autostart is set;
    // an enabled analyzer loads the workspace on its own thread after a short
    // delay, never during MCP initialisation.
    observatory.autostart_semantics();
    let outcome = async {
        let running = CrustyServer::new(observatory.clone())
            .serve(rmcp::transport::stdio())
            .await?;
        match running.waiting().await {
            Ok(_) => Ok(()),
            Err(error) if error.to_string().contains("connection closed") => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
    .await;
    if let Some(auto_refresh) = auto_refresh {
        auto_refresh.shutdown();
    }
    observatory.shutdown_semantics();
    outcome
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
    /// `incremental` (default), `workspace` (alias `full`), or `git`.
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

/// A task-starting (or `task.get`) request plus an optional bounded wait.
///
/// `wait_seconds` is peeled off before the inner request is deserialized, so
/// the inner type keeps rejecting unknown fields; `#[serde(flatten)]` would
/// silently accept them and drop `additionalProperties: false`.
#[derive(Debug, Clone)]
struct Waitable<T> {
    request: T,
    wait_seconds: u64,
}

impl<'de, T: DeserializeOwned> Deserialize<'de> for Waitable<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut object = serde_json::Map::<String, Value>::deserialize(deserializer)?;
        let wait_seconds = match object.remove("wait_seconds") {
            None | Some(Value::Null) => 0,
            Some(value) => u64::deserialize(value).map_err(D::Error::custom)?,
        };
        let request = T::deserialize(Value::Object(object)).map_err(D::Error::custom)?;
        Ok(Self {
            request,
            wait_seconds,
        })
    }
}

impl<T: JsonSchema> JsonSchema for Waitable<T> {
    fn schema_name() -> Cow<'static, str> {
        T::schema_name()
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let mut schema = T::json_schema(generator);
        if let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) {
            properties.insert(
                "wait_seconds".into(),
                json!({
                    "type": ["integer", "null"],
                    "minimum": 0,
                    "maximum": MAX_TASK_WAIT.as_secs(),
                    "default": 0,
                    "description": "Hold the call open up to this many seconds (capped at 120). If the task settles in time the completed task is returned inline in task.get shape; otherwise the task id is returned as without a wait. Cancelling the request stops the wait."
                }),
            );
        }
        schema
    }
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

    /// Returns a just-started task, waiting up to `wait_seconds` for it to
    /// settle. A settled task comes back in `task.get` shape; otherwise the
    /// start response is returned with the task's latest status. When the
    /// client cancels the request while waiting, nobody holds the task id any
    /// more, so the started task is cancelled cooperatively too.
    async fn started(
        &self,
        started: anyhow::Result<Value>,
        wait_seconds: u64,
        cancellation: CancellationToken,
    ) -> Result<Json<Value>, String> {
        let started = started.map_err(|error| format!("{error:#}"))?;
        let Some(id) = started["task_id"].as_str().filter(|_| wait_seconds > 0) else {
            return Ok(Json(started));
        };
        let wait = self
            .observatory
            .wait_for_task(id, Duration::from_secs(wait_seconds), &cancellation)
            .await
            .map_err(|error| format!("{error:#}"))?;
        match wait {
            TaskWait::Settled(task) => Ok(Json(task)),
            TaskWait::Pending(task) => {
                let mut pending = started;
                pending["status"] = task["task"]["status"].clone();
                pending["progress"] = task["task"]["progress"].clone();
                pending["message"] = task["task"]["message"].clone();
                pending["waited_seconds"] = json!(wait_seconds.min(MAX_TASK_WAIT.as_secs()));
                Ok(Json(pending))
            }
            TaskWait::Abandoned => {
                let _ = self.observatory.task_cancel(id);
                Err(format!(
                    "request cancelled while waiting; cancellation of task {id} was requested"
                ))
            }
        }
    }
}

#[tool_router(router = tool_router)]
impl CrustyServer {
    #[tool(
        name = "cleanup.plan",
        description = "Build a complete migration inventory around the proposed canonical owner across live source, tests, manifests, config, scripts and docs, supplemented by labelled indexed references. Reports skipped/truncated inputs and never assumes textual matches prove deadness. Returns a durable task."
    )]
    async fn cleanup_plan(
        &self,
        Parameters(Waitable {
            request,
            wait_seconds,
        }): Parameters<Waitable<CleanupPlanRequest>>,
        cancellation: CancellationToken,
    ) -> Result<Json<Value>, String> {
        self.started(
            self.observatory.cleanup_plan(request),
            wait_seconds,
            cancellation,
        )
        .await
    }
    #[tool(
        name = "cleanup.get",
        description = "Recover a migration inventory and report whether its source digest is stale. Re-inventory before relying on changed source; no deletion or work promotion is implicit."
    )]
    async fn cleanup_get(
        &self,
        Parameters(request): Parameters<IdRequest>,
    ) -> Result<Json<Value>, String> {
        let observatory = self.observatory.clone();
        Self::offload(move || observatory.cleanup_get(&request.id)).await
    }
    #[tool(
        name = "performance.contract",
        description = "Record a specific workload, operation count, wall-time budget and rationale before optimizing. A contract alone contains no measured result."
    )]
    async fn performance_contract(
        &self,
        Parameters(request): Parameters<PerformanceContractRequest>,
    ) -> Result<Json<Value>, String> {
        let observatory = self.observatory.clone();
        Self::offload(move || observatory.performance_contract(request)).await
    }
    #[tool(
        name = "performance.measure",
        description = "Build declared Cargo binaries in isolated baseline/candidate Git worktrees and measure sequential release-process samples with warmup, output artifacts, distribution and exact revision/profile evidence. Executes selected project code; preserves original worktrees. Output equivalence is checked by default. Returns a durable task; measurements do not prove universally optimal code."
    )]
    async fn performance_measure(
        &self,
        Parameters(Waitable {
            request,
            wait_seconds,
        }): Parameters<Waitable<PerformanceMeasureRequest>>,
        cancellation: CancellationToken,
    ) -> Result<Json<Value>, String> {
        self.started(
            self.observatory.performance_measure(request),
            wait_seconds,
            cancellation,
        )
        .await
    }
    #[tool(
        name = "performance.get",
        description = "Recover a workload contract or before/after measurement, including partial failure, retained worktrees, exact revisions, raw sample artifacts and claim limitations."
    )]
    async fn performance_get(
        &self,
        Parameters(request): Parameters<IdRequest>,
    ) -> Result<Json<Value>, String> {
        let observatory = self.observatory.clone();
        Self::offload(move || observatory.performance_get(&request.id)).await
    }
    #[tool(
        name = "engineering.guidance",
        description = "Retrieve versioned, task-specific Rust/architecture/cleanup/concurrency/unsafe/performance expertise with rationale, exceptions and required evidence. Human instructions and approved policy take precedence; advice never becomes a constraint automatically."
    )]
    async fn engineering_guidance(
        &self,
        Parameters(request): Parameters<GuidanceRequest>,
    ) -> Result<Json<Value>, String> {
        Self::value(self.observatory.engineering_guidance(request))
    }
    #[tool(
        name = "domain.model.propose",
        description = "Propose an explicit canonical ownership model with concept owners, invariant/mutation contracts and dependency directions. Captures current source provenance; proposed models do not govern or create work."
    )]
    async fn domain_propose(
        &self,
        Parameters(request): Parameters<DomainModelRequest>,
    ) -> Result<Json<Value>, String> {
        let observatory = self.observatory.clone();
        Self::offload(move || observatory.domain_propose(request)).await
    }
    #[tool(
        name = "domain.model.review",
        description = "Accept or reject a proposed ownership model only following explicit human review, with actor and rationale. Activation/supersession is atomic, decisions are terminal and conflicting active concept owners are rejected. Never infer human acceptance from an implementation request."
    )]
    async fn domain_review(
        &self,
        Parameters(request): Parameters<DomainReviewRequest>,
    ) -> Result<Json<Value>, String> {
        let observatory = self.observatory.clone();
        Self::offload(move || observatory.domain_review(request)).await
    }
    #[tool(
        name = "domain.model.get",
        description = "Inspect a proposed/accepted/superseded ownership model with invariants, mutation rights, dependency policy and human review provenance."
    )]
    async fn domain_get(
        &self,
        Parameters(request): Parameters<IdRequest>,
    ) -> Result<Json<Value>, String> {
        let observatory = self.observatory.clone();
        Self::offload(move || observatory.domain_get(&request.id)).await
    }
    #[tool(
        name = "domain.model.list",
        description = "List shared domain ownership models and their human review lifecycle with bounded pagination. Only accepted models govern consultation."
    )]
    async fn domain_list(
        &self,
        Parameters(request): Parameters<SessionListRequest>,
    ) -> Result<Json<Value>, String> {
        let observatory = self.observatory.clone();
        Self::offload(move || {
            observatory.domain_list(request.limit.unwrap_or(50), request.offset.unwrap_or(0))
        })
        .await
    }
    #[tool(
        name = "semantic.status",
        description = "Inspect the rust-analyzer companion without starting it: enabled state (with how to enable it), binary found, readiness (starting/warming/ready), restarts and last error, profile, capabilities, and diagnostic capture."
    )]
    async fn semantic_status(&self) -> Result<Json<Value>, String> {
        let observatory = self.observatory.clone();
        Self::offload(move || observatory.semantic_status()).await
    }
    #[tool(
        name = "semantic.enable",
        description = "Turn the rust-analyzer companion on for this server process; it is off by default. It loads the workspace in the background (unless RUST_REPO_INTELLIGENCE_RUST_ANALYZER_WARM_START=0) and then serves semantic.query, semantic.diagnostics, compiler-backed relations and validation diagnostics. Costs one analyzer process, often several GB for large workspaces. Returns semantic.status; poll it until state is ready. Idempotent."
    )]
    async fn semantic_enable(&self) -> Result<Json<Value>, String> {
        let observatory = self.observatory.clone();
        Self::offload(move || observatory.enable_semantics()).await
    }
    #[tool(
        name = "semantic.disable",
        description = "Turn the rust-analyzer companion off for this server process and stop its process, releasing its memory. Exact search, the index and validation keep working without it. Idempotent."
    )]
    async fn semantic_disable(&self) -> Result<Json<Value>, String> {
        let observatory = self.observatory.clone();
        Self::offload(move || observatory.disable_semantics()).await
    }
    #[tool(
        name = "semantic.diagnostics",
        description = "Read live rust-analyzer diagnostics captured by Crusty, optionally for named files (opened so the analyzer publishes theirs) and a minimum severity. Each file is labelled fresh/stale/pending against its content on disk; output is bounded. Advisory: cargo/rustc checks remain authoritative. Requires the opt-in companion."
    )]
    async fn semantic_diagnostics(
        &self,
        Parameters(request): Parameters<DiagnosticsRequest>,
    ) -> Result<Json<Value>, String> {
        let observatory = self.observatory.clone();
        Self::offload(move || observatory.semantic_diagnostics(request)).await
    }
    #[tool(
        name = "semantic.query",
        description = "Query live Rust hover/docs, definitions/types, references, implementations, call hierarchy, macros, dependency versions, signatures or proposed rename/assist edits by file and UTF-16 position, independently of the index. Reports actual profile/readiness/completeness and never applies edits or executes returned commands. Enabled semantic companions can execute project build scripts/proc macros. Returns a durable task."
    )]
    async fn semantic_query(
        &self,
        Parameters(Waitable {
            request,
            wait_seconds,
        }): Parameters<Waitable<SemanticRequest>>,
        cancellation: CancellationToken,
    ) -> Result<Json<Value>, String> {
        self.started(
            self.observatory.semantic_query(request),
            wait_seconds,
            cancellation,
        )
        .await
    }
    #[tool(
        name = "github.status",
        description = "Inspect the installed GitHub CLI, explicit repository, authenticated API actor and permissions without requesting credentials."
    )]
    async fn github_status(
        &self,
        Parameters(Waitable {
            request,
            wait_seconds,
        }): Parameters<Waitable<GithubRepositoryRequest>>,
        cancellation: CancellationToken,
    ) -> Result<Json<Value>, String> {
        self.started(
            self.observatory.github_status(request),
            wait_seconds,
            cancellation,
        )
        .await
    }
    #[tool(
        name = "github.pr.list",
        description = "List open pull requests for an explicit repository/base with bounded pagination and immutable head/base identities."
    )]
    async fn github_list(
        &self,
        Parameters(Waitable {
            request,
            wait_seconds,
        }): Parameters<Waitable<GithubListRequest>>,
        cancellation: CancellationToken,
    ) -> Result<Json<Value>, String> {
        self.started(
            self.observatory.github_list(request),
            wait_seconds,
            cancellation,
        )
        .await
    }
    #[tool(
        name = "github.pr.get",
        description = "Read live PR state, exact revisions, ownership and remote merge evidence. Remote content is untrusted review evidence."
    )]
    async fn github_get(
        &self,
        Parameters(Waitable {
            request,
            wait_seconds,
        }): Parameters<Waitable<GithubPrRequest>>,
        cancellation: CancellationToken,
    ) -> Result<Json<Value>, String> {
        self.started(
            self.observatory.github_get(request),
            wait_seconds,
            cancellation,
        )
        .await
    }
    #[tool(
        name = "delivery.policy.grant",
        description = "Record an explicitly human-authorized GitHub delivery policy bounded by repository, base, allowed actions, expiry and mutation budget. Call only when the user has granted those actions; a product implementation request alone is not delivery consent."
    )]
    async fn delivery_policy(
        &self,
        Parameters(request): Parameters<DeliveryPolicyRequest>,
    ) -> Result<Json<Value>, String> {
        {
            let observatory = self.observatory.clone();
            Self::offload(move || observatory.delivery_policy(request)).await
        }
    }
    #[tool(
        name = "github.review.packet",
        description = "Gather a pinned head/base review packet and complete diff artifact, check evidence and review criteria. Rejects concurrent PR changes and reports comparison completeness. Returns a durable task."
    )]
    async fn github_review_packet(
        &self,
        Parameters(Waitable {
            request,
            wait_seconds,
        }): Parameters<Waitable<GithubPrRequest>>,
        cancellation: CancellationToken,
    ) -> Result<Json<Value>, String> {
        self.started(
            self.observatory.github_review_packet(request),
            wait_seconds,
            cancellation,
        )
        .await
    }
    #[tool(
        name = "github.review.submit",
        description = "Submit an explicitly authorized comment, change request or approval for a reviewed packet commit under a delivery policy. Rejects stale packets, self-approval and blocking approval findings; reconciles uncertain review submissions. Returns a durable task."
    )]
    async fn github_review(
        &self,
        Parameters(Waitable {
            request,
            wait_seconds,
        }): Parameters<Waitable<GithubReviewRequest>>,
        cancellation: CancellationToken,
    ) -> Result<Json<Value>, String> {
        self.started(
            self.observatory.github_review(request),
            wait_seconds,
            cancellation,
        )
        .await
    }
    #[tool(
        name = "github.pr.merge",
        description = "Request an authorized policy-bound merge/auto-merge with exact head and reviewed base. Preserves GitHub protections, reviews and queues; never bypasses rules. Returns a durable task; request acceptance is not completion."
    )]
    async fn github_merge(
        &self,
        Parameters(Waitable {
            request,
            wait_seconds,
        }): Parameters<Waitable<GithubMergeRequest>>,
        cancellation: CancellationToken,
    ) -> Result<Json<Value>, String> {
        self.started(
            self.observatory.github_merge(request),
            wait_seconds,
            cancellation,
        )
        .await
    }
    #[tool(
        name = "github.pr.publish",
        description = "Publish a verified owned work chunk or completed integration as a draft PR under an explicit delivery policy. Pushes a named branch without force, checks remote head, and reconciles existing PRs. Returns a durable task."
    )]
    async fn github_publish(
        &self,
        Parameters(Waitable {
            request,
            wait_seconds,
        }): Parameters<Waitable<GithubPublishRequest>>,
        cancellation: CancellationToken,
    ) -> Result<Json<Value>, String> {
        self.started(
            self.observatory.github_publish(request),
            wait_seconds,
            cancellation,
        )
        .await
    }
    #[tool(
        name = "github.pr.ready",
        description = "Mark an explicitly authorized draft PR ready under a publish policy, with pre/post head checks. Returns a durable task; a changed head is reported stale."
    )]
    async fn github_ready(
        &self,
        Parameters(Waitable {
            request,
            wait_seconds,
        }): Parameters<Waitable<GithubReadyRequest>>,
        cancellation: CancellationToken,
    ) -> Result<Json<Value>, String> {
        self.started(
            self.observatory.github_ready(request),
            wait_seconds,
            cancellation,
        )
        .await
    }
    #[tool(
        name = "github.action.get",
        description = "Recover durable GitHub mutation intent and outcome without retrying a remote mutation."
    )]
    async fn github_action_get(
        &self,
        Parameters(request): Parameters<IdRequest>,
    ) -> Result<Json<Value>, String> {
        let observatory = self.observatory.clone();
        Self::offload(move || observatory.github_action_get(&request.id)).await
    }
    #[tool(
        name = "github.action.reconcile",
        description = "Read GitHub to reconcile a known PR action. Only actual merged state proves completion; changed heads invalidate evidence."
    )]
    async fn github_reconcile(
        &self,
        Parameters(Waitable {
            request,
            wait_seconds,
        }): Parameters<Waitable<IdRequest>>,
        cancellation: CancellationToken,
    ) -> Result<Json<Value>, String> {
        self.started(
            self.observatory.github_reconcile(request.id),
            wait_seconds,
            cancellation,
        )
        .await
    }
    #[tool(
        name = "delivery.policy.get",
        description = "Inspect bounded human delivery authority, expiry and consumed mutation budget."
    )]
    async fn delivery_policy_get(
        &self,
        Parameters(request): Parameters<IdRequest>,
    ) -> Result<Json<Value>, String> {
        let observatory = self.observatory.clone();
        Self::offload(move || observatory.delivery_policy_get(&request.id)).await
    }
    #[tool(
        name = "delivery.policy.revoke",
        description = "Revoke future use of a delivery policy explicitly at the user request; retain its audit history."
    )]
    async fn delivery_policy_revoke(
        &self,
        Parameters(request): Parameters<IdRequest>,
    ) -> Result<Json<Value>, String> {
        let observatory = self.observatory.clone();
        Self::offload(move || observatory.delivery_policy_revoke(&request.id)).await
    }
    #[tool(
        name = "github.action.list",
        description = "List durable GitHub delivery intents and outcomes with bounded pagination for recovery after interruption."
    )]
    async fn github_action_list(
        &self,
        Parameters(request): Parameters<SessionListRequest>,
    ) -> Result<Json<Value>, String> {
        let observatory = self.observatory.clone();
        Self::offload(move || {
            observatory.github_action_list(request.limit.unwrap_or(50), request.offset.unwrap_or(0))
        })
        .await
    }
    #[tool(
        name = "project.contract",
        description = "Read the live Cargo workspace contract, members, features, targets, editions and declared MSRV together with current instructions, CI and configuration evidence. Does not refresh the index or infer supported feature combinations."
    )]
    async fn project_contract(&self) -> Result<Json<Value>, String> {
        let observatory = self.observatory.clone();
        Self::offload(move || observatory.project_contract()).await
    }
    #[tool(
        name = "verification.plan",
        description = "Plan explicitly scoped Cargo checks from the live project contract, bound to source, HEAD, environment and selected packages/features/target/toolchain. Default format/check/test/clippy includes doctests. Returns a durable task; inspect the plan before execution."
    )]
    async fn verification_plan(
        &self,
        Parameters(Waitable {
            request,
            wait_seconds,
        }): Parameters<Waitable<VerificationPlanRequest>>,
        cancellation: CancellationToken,
    ) -> Result<Json<Value>, String> {
        self.started(
            self.observatory.verification_plan(request),
            wait_seconds,
            cancellation,
        )
        .await
    }
    #[tool(
        name = "verification.run",
        description = "Execute a prepared verification plan as a durable cancellable task. Rejects stale source/environment, preserves full compiler children/spans/suggestions and output artifacts, and explicitly reports coverage and delivery eligibility. Named Cargo checks may execute project build scripts and tests."
    )]
    async fn verification_run(
        &self,
        Parameters(Waitable {
            request,
            wait_seconds,
        }): Parameters<Waitable<IdRequest>>,
        cancellation: CancellationToken,
    ) -> Result<Json<Value>, String> {
        self.started(
            self.observatory.verification_run(request.id),
            wait_seconds,
            cancellation,
        )
        .await
    }
    #[tool(
        name = "verification.get",
        description = "Recover an immutable verification plan or result by ID, including exact revision/profile, diagnostics and output evidence. Reuse does not make stale results current."
    )]
    async fn verification_get(
        &self,
        Parameters(request): Parameters<IdRequest>,
    ) -> Result<Json<Value>, String> {
        let observatory = self.observatory.clone();
        Self::offload(move || observatory.verification_get(&request.id)).await
    }
    #[tool(
        name = "integration.get",
        description = "Inspect a Git integration preview or isolated resolution, including pinned parent OIDs, worktree, conflicts and durable lifecycle state."
    )]
    async fn integration_get(
        &self,
        Parameters(request): Parameters<IdRequest>,
    ) -> Result<Json<Value>, String> {
        let observatory = self.observatory.clone();
        Self::offload(move || observatory.integration_get(&request.id)).await
    }
    #[tool(
        name = "integration.start",
        description = "Create an isolated integration branch/worktree from a pinned preview and run a no-commit merge. Keeps both original worktrees untouched and retains conflicting work for explicit resolution. Requires an active owning session; returns a durable task."
    )]
    async fn integration_start(
        &self,
        Parameters(Waitable {
            request,
            wait_seconds,
        }): Parameters<Waitable<IntegrationStartRequest>>,
        cancellation: CancellationToken,
    ) -> Result<Json<Value>, String> {
        self.started(
            self.observatory.integration_start(request),
            wait_seconds,
            cancellation,
        )
        .await
    }
    #[tool(
        name = "integration.resolve",
        description = "Record an explicitly staged resolution as an exact-tree merge commit in its isolated worktree. Rejects unresolved/unstaged paths and changed parents. Does not run hooks. Run verification from that worktree afterwards; returns a durable task."
    )]
    async fn integration_resolve(
        &self,
        Parameters(Waitable {
            request,
            wait_seconds,
        }): Parameters<Waitable<IntegrationResolveRequest>>,
        cancellation: CancellationToken,
    ) -> Result<Json<Value>, String> {
        self.started(
            self.observatory.integration_resolve(request),
            wait_seconds,
            cancellation,
        )
        .await
    }
    #[tool(
        name = "integration.complete",
        description = "Confirm a resolved integration using current, complete revision-bound format/check/test/clippy evidence from its worktree. Retains the validated branch for PR delivery without overwriting checked-out main. Returns a durable task."
    )]
    async fn integration_complete(
        &self,
        Parameters(Waitable {
            request,
            wait_seconds,
        }): Parameters<Waitable<IntegrationCompleteRequest>>,
        cancellation: CancellationToken,
    ) -> Result<Json<Value>, String> {
        self.started(
            self.observatory.integration_complete(request),
            wait_seconds,
            cancellation,
        )
        .await
    }
    #[tool(
        name = "commit.execute",
        description = "Execute an immutable owned commit plan using a private index and compare-and-swap HEAD. Preserves unrelated staged work and worktree files; honors commit signing, but does not run Git hooks. Run project verification first. Omit session credentials for a plan made without a session; it is refused once another session is active. Returns a durable task (inline with wait_seconds); retry the same plan to reconcile interrupted execution."
    )]
    async fn commit_execute(
        &self,
        Parameters(Waitable {
            request,
            wait_seconds,
        }): Parameters<Waitable<CommitExecuteRequest>>,
        cancellation: CancellationToken,
    ) -> Result<Json<Value>, String> {
        self.started(
            self.observatory.commit_execute(request),
            wait_seconds,
            cancellation,
        )
        .await
    }

    #[tool(
        name = "commit.get",
        description = "Recover a commit plan and its durable execution state, generated commit OIDs and delivery status."
    )]
    async fn commit_get(
        &self,
        Parameters(request): Parameters<IdRequest>,
    ) -> Result<Json<Value>, String> {
        let observatory = self.observatory.clone();
        Self::offload(move || observatory.commit_get(&request.id)).await
    }

    #[tool(
        name = "chunk.create",
        description = "Record a cohesive delivery chunk from this session's completed commit execution at current HEAD, linking human-owned work and validation references. Does not publish or assume referenced checks are valid."
    )]
    async fn chunk_create(
        &self,
        Parameters(request): Parameters<ChunkCreateRequest>,
    ) -> Result<Json<Value>, String> {
        let observatory = self.observatory.clone();
        Self::offload(move || observatory.chunk_create(request)).await
    }

    #[tool(
        name = "chunk.get",
        description = "Inspect an immutable work chunk and its exact base/head, commits, ownership and validation references."
    )]
    async fn chunk_get(
        &self,
        Parameters(request): Parameters<IdRequest>,
    ) -> Result<Json<Value>, String> {
        let observatory = self.observatory.clone();
        Self::offload(move || observatory.chunk_get(&request.id)).await
    }

    #[tool(
        name = "chunk.list",
        description = "List delivery chunks shared across all linked worktrees, with bounded pagination."
    )]
    async fn chunk_list(
        &self,
        Parameters(request): Parameters<SessionListRequest>,
    ) -> Result<Json<Value>, String> {
        let observatory = self.observatory.clone();
        Self::offload(move || {
            observatory.chunk_list(request.limit.unwrap_or(50), request.offset.unwrap_or(0))
        })
        .await
    }

    #[tool(
        name = "integration.preview",
        description = "Resolve source/base refs and compute Git merge-tree evidence without modifying either index or worktree. Returns clean/conflicted state, immutable OIDs, conflicting paths and explanatory messages as a durable task."
    )]
    async fn integration_preview(
        &self,
        Parameters(Waitable {
            request,
            wait_seconds,
        }): Parameters<Waitable<IntegrationPreviewRequest>>,
        cancellation: CancellationToken,
    ) -> Result<Json<Value>, String> {
        self.started(
            self.observatory.integration_preview(request),
            wait_seconds,
            cancellation,
        )
        .await
    }

    #[tool(
        name = "session.start",
        description = "Register a leased coding session shared across Git worktrees, for work alongside other agents; a single agent needs no session. Optionally create an isolated branch/worktree without copying dirty files. Returns a durable task whose result holds the session and private lease token (inline with wait_seconds)."
    )]
    async fn session_start(
        &self,
        Parameters(Waitable {
            request,
            wait_seconds,
        }): Parameters<Waitable<SessionStartRequest>>,
        cancellation: CancellationToken,
    ) -> Result<Json<Value>, String> {
        self.started(
            self.observatory.session_start(request),
            wait_seconds,
            cancellation,
        )
        .await
    }

    #[tool(
        name = "session.heartbeat",
        description = "Renew an active coding session lease and describe current activity. Requires its private lease token; closed or expired sessions cannot be revived."
    )]
    async fn session_heartbeat(
        &self,
        Parameters(request): Parameters<HeartbeatRequest>,
    ) -> Result<Json<Value>, String> {
        let observatory = self.observatory.clone();
        Self::offload(move || observatory.session_heartbeat(request)).await
    }

    #[tool(
        name = "session.claim",
        description = "Atomically replace this session's file/subtree ownership claims. Overlap reports identify the current owner and preserve existing claims. An empty paths list releases claims; use the registered worktree and lease token."
    )]
    async fn session_claim(
        &self,
        Parameters(request): Parameters<ClaimRequest>,
    ) -> Result<Json<Value>, String> {
        let observatory = self.observatory.clone();
        Self::offload(move || observatory.session_claim(request)).await
    }

    #[tool(
        name = "session.get",
        description = "Inspect one coding session's owner, intent, worktree, branch, lease status, activity and claims without exposing its private token."
    )]
    async fn session_get(
        &self,
        Parameters(request): Parameters<IdRequest>,
    ) -> Result<Json<Value>, String> {
        let observatory = self.observatory.clone();
        Self::offload(move || observatory.session_get(&request.id)).await
    }

    #[tool(
        name = "session.list",
        description = "List active parallel coding sessions across all linked Git worktrees, with owners, intent, activity and claimed paths. Supports explicit pagination and inactive history."
    )]
    async fn session_list(
        &self,
        Parameters(request): Parameters<SessionListRequest>,
    ) -> Result<Json<Value>, String> {
        let observatory = self.observatory.clone();
        Self::offload(move || observatory.session_list(request)).await
    }

    #[tool(
        name = "session.close",
        description = "Close an active coding session and release its ownership claims using its private lease token. Retains branches and worktrees so no uncommitted or committed work is deleted."
    )]
    async fn session_close(
        &self,
        Parameters(request): Parameters<SessionAuth>,
    ) -> Result<Json<Value>, String> {
        let observatory = self.observatory.clone();
        Self::offload(move || observatory.session_close(request)).await
    }

    #[tool(
        name = "commit.plan",
        description = "Prepare cohesive whole-file commit groups from live changes owned by the coding session. Records HEAD and content fingerprints, reports unassigned paths, and never stages another session's files. For single-agent work omit session_id and lease_token: while no other session is active, an implicit session owns exactly the planned paths until commit.execute. Returns a durable task (inline with wait_seconds)."
    )]
    async fn commit_plan(
        &self,
        Parameters(Waitable {
            request,
            wait_seconds,
        }): Parameters<Waitable<CommitPlanRequest>>,
        cancellation: CancellationToken,
    ) -> Result<Json<Value>, String> {
        self.started(
            self.observatory.commit_plan(request),
            wait_seconds,
            cancellation,
        )
        .await
    }

    #[tool(
        name = "repo.consult",
        description = "Mandatory preflight before planning, answering, or acting on any repository-scoped user request; for an edit task change.prepare returns the same guidance and may be the first call instead. Returns global and topical decisions, steering, design and quality constraints, restrictions, governing documentation, workflows, lifecycle risks, runtime contracts, and open known work before planning, answering, or acting. Items that do not fit the budget are compacted before being omitted; context_budget.fetch_more names how to read the rest."
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
        description = "Search Rust source. exact reads the live worktree; broad reads the last published generation without waiting for a refresh and labels freshness."
    )]
    async fn repo_search(
        &self,
        Parameters(request): Parameters<SearchRequest>,
    ) -> Result<Json<Value>, String> {
        Self::value(self.observatory.search(request).await)
    }

    #[tool(
        name = "repo.context",
        description = "Build a bounded evidence context from the last published generation; never waits for a refresh. The freshness envelope reports whether it is stale and why."
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
        description = "Start a durable contextual Rust architecture audit against the live worktree; returns a task id (or the completed task with wait_seconds) and persists the completed report."
    )]
    async fn audit_start(
        &self,
        Parameters(Waitable {
            request,
            wait_seconds,
        }): Parameters<Waitable<ArchitectureRequest>>,
        cancellation: CancellationToken,
    ) -> Result<Json<Value>, String> {
        self.started(
            self.observatory.start_architecture_audit(request),
            wait_seconds,
            cancellation,
        )
        .await
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
        description = "Record a human architectural decision so prepared changes and validation can cite it. It may supersede accepted decisions by ID, which requires recorded_by."
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
        description = "List human architectural decisions: the whole ledger newest first, or those relevant to a symbol, path, or concept, optionally filtered by status."
    )]
    async fn decision_list(
        &self,
        Parameters(request): Parameters<DecisionListRequest>,
    ) -> Result<Json<Value>, String> {
        let observatory = self.observatory.clone();
        Self::offload(move || {
            observatory.decision_list(
                request.scope.as_deref(),
                request.status.as_deref(),
                request.limit,
            )
        })
        .await
    }

    #[tool(
        name = "decision.retire",
        description = "Retire an accepted human decision without replacing it. Requires the retiring human's identity; the record stays in the ledger as history."
    )]
    async fn decision_retire(
        &self,
        Parameters(request): Parameters<RetireDecision>,
    ) -> Result<Json<Value>, String> {
        let observatory = self.observatory.clone();
        Self::offload(move || observatory.decision_retire(request)).await
    }

    #[tool(
        name = "steering.record",
        description = "Record a durable human steering instruction that should shape later changes. It may supersede active steerings by ID, which retires them atomically and requires recorded_by; supersede only on a human's explicit instruction. The response warns when the instruction names repository paths that do not exist."
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
        description = "List durable human steering instructions, by default only active and unexpired ones; status `retired` or `all` returns history. With a scope, steerings scoped to an ancestor or descendant of a named path rank first, then symbol/concept matches, then global steerings. Each steering reports supersession, history, and stale_references to missing paths."
    )]
    async fn steering_list(
        &self,
        Parameters(request): Parameters<SteeringListRequest>,
    ) -> Result<Json<Value>, String> {
        let observatory = self.observatory.clone();
        Self::offload(move || {
            observatory.steering_list(
                request.scope.as_deref(),
                request.status.as_deref(),
                request.limit,
            )
        })
        .await
    }

    #[tool(
        name = "steering.retire",
        description = "Retire an active human steering without replacing it. Requires the retiring human's identity and a reason; the record stays in the ledger as history. Retire only on a human's explicit instruction."
    )]
    async fn steering_retire(
        &self,
        Parameters(request): Parameters<RetireSteering>,
    ) -> Result<Json<Value>, String> {
        let observatory = self.observatory.clone();
        Self::offload(move || observatory.steering_retire(request)).await
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
        description = "Prepare one coherent change before its first edit, against the last published generation without waiting for a refresh. Returns the consultation guidance for the change (decisions, steering, live instructions, engineering route) plus change evidence and an architecture baseline. Reuse its context_id for every validation of the change, however long. Returns a task id, or the completed task with wait_seconds."
    )]
    async fn change_prepare(
        &self,
        Parameters(Waitable {
            request,
            wait_seconds,
        }): Parameters<Waitable<PrepareRequest>>,
        cancellation: CancellationToken,
    ) -> Result<Json<Value>, String> {
        self.started(
            self.observatory.start_prepare_change(request),
            wait_seconds,
            cancellation,
        )
        .await
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
        description = "Validate a change at milestones and at the end. With no diff source it reads pending tracked edits plus untracked, non-ignored files against HEAD, so no diff argument is needed; git_diff, local diff_path, or base_ref with target HEAD/worktree (tracked files only) select another diff. One prepared context_id serves any number of validations; without one, architecture_delta and unmodified_expected_callers are unavailable. Never waits for an index refresh. Returns a task id, or the completed task with wait_seconds."
    )]
    async fn change_validate(
        &self,
        Parameters(Waitable {
            request,
            wait_seconds,
        }): Parameters<Waitable<ValidateRequest>>,
        cancellation: CancellationToken,
    ) -> Result<Json<Value>, String> {
        self.started(
            self.observatory.start_validate_change(request),
            wait_seconds,
            cancellation,
        )
        .await
    }

    #[tool(
        name = "index.status",
        description = "Show the resolved workspace root, live versus indexed freshness (reason, never_published), one index and backend block, the latest refresh task, and the background refresher."
    )]
    async fn index_status(&self) -> Result<Json<Value>, String> {
        Self::value(self.observatory.index_status())
    }

    #[tool(
        name = "index.refresh",
        description = "Start an explicit background index refresh under the single-writer publisher lease; returns a task id, or the completed task with wait_seconds. scope defaults to incremental (changed files and the symbols they affect; a never-indexed or other-indexer-version store is rebuilt in full); workspace or full forces a complete rebuild; git refreshes commit history only and publishes no generation, so freshness stays stale. Unless CRUSTY_AUTO_REFRESH=0, the server also runs the incremental refresh in the background after file changes and commits."
    )]
    async fn index_refresh(
        &self,
        Parameters(Waitable {
            request,
            wait_seconds,
        }): Parameters<Waitable<IndexRefreshRequest>>,
        cancellation: CancellationToken,
    ) -> Result<Json<Value>, String> {
        self.started(
            self.observatory.start_index_refresh(request.scope),
            wait_seconds,
            cancellation,
        )
        .await
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
        description = "Read a durable change, refresh, audit, session, commit, verification, or research task's progress, result, or failure. wait_seconds holds the call until the task settles (at most 120 s) instead of polling; cancelling the request stops only the wait."
    )]
    async fn task_get(
        &self,
        Parameters(Waitable {
            request,
            wait_seconds,
        }): Parameters<Waitable<IdRequest>>,
        cancellation: CancellationToken,
    ) -> Result<Json<Value>, String> {
        if wait_seconds == 0 {
            return Self::value(self.observatory.task_get(&request.id));
        }
        // The caller already holds the task id, so a cancelled wait leaves the
        // task itself running.
        match self
            .observatory
            .wait_for_task(
                &request.id,
                Duration::from_secs(wait_seconds),
                &cancellation,
            )
            .await
            .map_err(|error| format!("{error:#}"))?
        {
            TaskWait::Settled(task) | TaskWait::Pending(task) => Ok(Json(task)),
            TaskWait::Abandoned => Err("request cancelled while waiting".into()),
        }
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
        description = "Recover repository-scoped user prompts from Codex primary and side-session history and Claude Code transcripts, each labelled with its source, and search preserved legacy decisions, steerings, problems, and quality constraints. Read-only results never become work without explicit human creation."
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
    instructions = "Crusty is a Rust repository observatory. Consult before acting on every repository-scoped user prompt: call repo.consult first with the user's complete intent before planning, answering, or acting, including design, review, questions, documentation, configuration, and non-code work. For an edit task, change.prepare may be that first call instead, because it returns the same decisions, steering, live instructions, and engineering route together with change evidence. Apply the human decisions, steering, design and quality constraints, restrictions, governing documents, and workflows either returns. Keep the workflow proportionate: one change.prepare per coherent change, before its first edit, and change.validate at milestones and at the end, reusing the same context_id however long the change runs. Validation needs no diff argument: by default it reads pending tracked edits plus untracked, non-ignored files against HEAD. It also runs without a context_id, minus the architecture delta and expected change surface. Task-starting tools and task.get accept wait_seconds (at most 120): a task that settles in time returns inline, so poll task.get only for long work; task.list and change.get recover interrupted workflows. Use repo.architecture for a bounded live architecture map and audit.start for a durable contextual audit. Change preparation captures an architecture baseline; validation reports advisory new, worsened, and resolved findings without blocking on inferred debt. Use repo.search exact for live call-site work and symbol.relations for callers, references, implementations, and definitions; neither refreshes. The rust-analyzer companion is off until semantic.enable; call it when live semantics or compiler diagnostics would help and semantic.disable when done. Once enabled, semantic.query answers live position queries, semantic.diagnostics returns its captured diagnostics, and change.validate adds advisory diagnostics for changed Rust files. When a broad read is empty, check index.status: never_published distinguishes an unbuilt index from no matches. Index refreshes are durable tasks: the server runs them in the background after edits and commits unless CRUSTY_AUTO_REFRESH=0, and index.refresh starts one explicitly. Reads never wait for a refresh; they serve the last published generation with a freshness envelope whose reason says why it is stale. Research uses local repository evidence plus primary-first web_search by the attached agent. GitHub delivery uses the installed gh CLI only under explicit bounded human delivery policy; never infer remote mutation consent from research or implementation requests. Findings are proposals and only a human may review or promote them into work. Quality proposal never activates a constraint: activation, merging, finding promotion, and work writes require explicit human confirmation. Sessions and path claims are for parallel work only: when other agents work in the same repository, register owner/intent with session.start, prefer an isolated worktree, claim paths before edits, heartbeat before expiry, and close after handoff; claim conflicts require narrowing scope or owner handoff. Use commit.plan and commit.execute for cohesive whole-file commits; a single agent may omit session credentials while no other session is active. Execution preserves unrelated staging and uses exact trees without running hooks. Consult project.contract, obtain engineering guidance, and run revision-bound verification before publishing chunks or completed integrations. Reviews pin head/base evidence; queued merge requests remain pending until actual merge is observed. Source, compiler/runtime behavior, and human ownership remain authoritative."
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
                "chunk.create",
                "chunk.get",
                "chunk.list",
                "cleanup.get",
                "cleanup.plan",
                "commit.execute",
                "commit.get",
                "commit.plan",
                "dashboard.open",
                "decision.list",
                "decision.record",
                "decision.retire",
                "delivery.policy.get",
                "delivery.policy.grant",
                "delivery.policy.revoke",
                "domain.model.get",
                "domain.model.list",
                "domain.model.propose",
                "domain.model.review",
                "engineering.guidance",
                "finding.evidence.add",
                "finding.get",
                "finding.list",
                "finding.promote",
                "finding.review",
                "github.action.get",
                "github.action.list",
                "github.action.reconcile",
                "github.pr.get",
                "github.pr.list",
                "github.pr.merge",
                "github.pr.publish",
                "github.pr.ready",
                "github.review.packet",
                "github.review.submit",
                "github.status",
                "index.refresh",
                "index.status",
                "integration.complete",
                "integration.get",
                "integration.preview",
                "integration.resolve",
                "integration.start",
                "memory.search",
                "performance.contract",
                "performance.get",
                "performance.measure",
                "problem.get",
                "problem.list",
                "problem.record",
                "problem.update",
                "project.contract",
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
                "semantic.diagnostics",
                "semantic.disable",
                "semantic.enable",
                "semantic.query",
                "semantic.status",
                "session.claim",
                "session.close",
                "session.get",
                "session.heartbeat",
                "session.list",
                "session.start",
                "steering.list",
                "steering.record",
                "steering.retire",
                "symbol.relations",
                "task.cancel",
                "task.get",
                "task.list",
                "validation.queue",
                "validation.record",
                "verification.get",
                "verification.plan",
                "verification.run",
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
        for name in ["session.claim", "session.close", "session.heartbeat"] {
            assert!(required(name).contains(&"session_id".to_owned()));
            assert!(required(name).contains(&"lease_token".to_owned()));
        }
        // Single-agent commits may omit the session; the server refuses them
        // while another session is active.
        for name in ["commit.plan", "commit.execute"] {
            assert!(!required(name).contains(&"session_id".to_owned()));
            assert!(!required(name).contains(&"lease_token".to_owned()));
        }
        assert!(required("commit.execute").contains(&"plan_id".to_owned()));
        assert!(required("session.claim").contains(&"paths".to_owned()));
        assert!(required("commit.plan").contains(&"groups".to_owned()));
        // One prepared context serves many validations, and none is required.
        assert!(!required("change.validate").contains(&"context_id".to_owned()));
        assert!(required("change.prepare").contains(&"intent".to_owned()));
        assert!(required("task.get").contains(&"id".to_owned()));
        // Every task-starting tool and task.get can wait inline, and the wait
        // wrapper keeps the inner request strict.
        for name in [
            "change.prepare",
            "change.validate",
            "session.start",
            "index.refresh",
            "audit.start",
            "task.get",
            "commit.plan",
            "commit.execute",
            "verification.run",
        ] {
            let schema = &tools
                .iter()
                .find(|tool| tool.name == name)
                .unwrap()
                .input_schema;
            assert_eq!(
                schema["properties"]["wait_seconds"]["maximum"], 120,
                "{name} needs wait_seconds"
            );
            assert!(!required(name).contains(&"wait_seconds".to_owned()));
            assert_eq!(
                schema.get("additionalProperties"),
                Some(&Value::Bool(false)),
                "{name} must still refuse unknown fields"
            );
        }
        let validation_schema = &tools
            .iter()
            .find(|tool| tool.name == "change.validate")
            .unwrap()
            .input_schema;
        let properties = validation_schema["properties"].as_object().unwrap();
        for name in ["git_diff", "diff_path", "base_ref", "target"] {
            assert!(
                properties.contains_key(name),
                "missing validation input {name}"
            );
            assert!(!required("change.validate").contains(&name.to_owned()));
            assert!(properties[name]["description"].is_string());
        }
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
    fn waitable_requests_peel_wait_seconds_and_keep_the_inner_request_strict() {
        let parsed: Waitable<IdRequest> =
            serde_json::from_value(json!({"id":"task_1","wait_seconds":5})).unwrap();
        assert_eq!(
            (parsed.request.id.as_str(), parsed.wait_seconds),
            ("task_1", 5)
        );
        let defaulted: Waitable<IdRequest> =
            serde_json::from_value(json!({"id":"task_1","wait_seconds":null})).unwrap();
        assert_eq!(defaulted.wait_seconds, 0);
        assert!(
            serde_json::from_value::<Waitable<IdRequest>>(json!({"id":"task_1","wait":5})).is_err()
        );
        assert!(
            serde_json::from_value::<Waitable<IdRequest>>(json!({"id":"t","wait_seconds":-1}))
                .is_err()
        );
    }

    #[tokio::test]
    async fn waiting_returns_a_settled_task_inline_and_cancels_an_abandoned_one() -> Result<()> {
        let directory = tempdir()?;
        fs::write(
            directory.path().join("Cargo.toml"),
            "[package]\nname='fixture'\nversion='0.1.0'\nedition='2024'\n",
        )?;
        fs::create_dir(directory.path().join("src"))?;
        fs::write(directory.path().join("src/lib.rs"), "pub fn waited() {}\n")?;
        let server = CrustyServer::new(Observatory::open(directory.path())?);
        let started = server.observatory.start_index_refresh(None);
        let settled = server
            .started(started, 60, CancellationToken::new())
            .await
            .map_err(anyhow::Error::msg)?
            .0;
        assert_eq!(settled["task"]["status"], "completed", "{settled}");
        assert_eq!(settled["task"]["kind"], "index.refresh");
        assert!(settled["task"]["result"].is_object());

        // Without a wait the start response is returned unchanged.
        let started = server.observatory.start_index_refresh(None);
        let immediate = server
            .started(started, 0, CancellationToken::new())
            .await
            .map_err(anyhow::Error::msg)?
            .0;
        assert_eq!(immediate["poll_with"], "task.get");

        // A client that abandons the request cannot hold the task id, so the
        // started task is cancelled with it.
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        let started = server.observatory.start_index_refresh(None);
        let id = started.as_ref().unwrap()["task_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let Err(abandoned) = server.started(started, 60, cancelled).await else {
            panic!("an abandoned wait must not report success");
        };
        assert!(abandoned.contains("cancellation of task"), "{abandoned}");
        let after = server.observatory.task_get(&id)?;
        // Only a task that settled between the last poll and the cancellation
        // escapes the cancellation request.
        assert!(
            after["task"]["cancel_requested"] == true || after["task"]["status"] == "completed",
            "{after}"
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
        // Edit tasks may start with change.prepare, which carries the same
        // guidance; the workflow stays proportionate for long refactors.
        assert!(instructions.contains("change.prepare may be that first call"));
        assert!(instructions.contains("one change.prepare per coherent change"));
        assert!(instructions.contains("Validation needs no diff argument"));
        assert!(instructions.contains("wait_seconds"));
        assert!(instructions.contains("Sessions and path claims are for parallel work only"));
        // Human authority boundaries stay mandatory.
        assert!(instructions.contains("explicit bounded human delivery policy"));
        assert!(instructions.contains("only a human may review or promote"));
        Ok(())
    }
}

use anyhow::{Context, Result};
use rmcp::{
    Json, ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    tool, tool_handler, tool_router,
};
use rust_repo_intelligence::observatory::{
    ContextRequest, Observatory, PrepareRequest, PromoteFindingRequest, ResearchStartRequest,
    ResearchSubmitRequest, ReviewFindingRequest, SearchRequest, ValidateRequest, WorkCreateRequest,
    WorkUpdateRequest,
};
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
struct IdRequest {
    id: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
struct RunRequest {
    run_id: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
struct IndexRefreshRequest {
    scope: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
struct FindingListRequest {
    status: Option<String>,
    query: Option<String>,
    limit: Option<usize>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
struct WorkListRequest {
    query: Option<String>,
    limit: Option<usize>,
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
}

#[tool_router(router = tool_router)]
impl CrustyServer {
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
        name = "task.get",
        description = "Poll a durable refresh or local-research task for progress, result, or failure."
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
        name = "research.get",
        description = "Get the state and evidence packet for a research run."
    )]
    async fn research_get(
        &self,
        Parameters(request): Parameters<RunRequest>,
    ) -> Result<Json<Value>, String> {
        Self::value(self.observatory.research_packet(&request.run_id))
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
        Self::value(self.observatory.finding_list(
            request.status.as_deref(),
            request.query.as_deref(),
            request.limit.unwrap_or(100),
        ))
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
        Self::value(
            self.observatory
                .work_list(request.query.as_deref(), request.limit.unwrap_or(100)),
        )
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
        description = "Recommend only accepted or active, unblocked, human-owned work from the same store used by work.list."
    )]
    async fn work_recommend(
        &self,
        Parameters(request): Parameters<WorkListRequest>,
    ) -> Result<Json<Value>, String> {
        Self::value(self.observatory.work_recommend(request.query.as_deref()))
    }

    #[tool(
        name = "work.create",
        description = "Create human-owned project work with explicit confirmation. Crusty findings cannot call this autonomously."
    )]
    async fn work_create(
        &self,
        Parameters(request): Parameters<WorkCreateRequest>,
    ) -> Result<Json<Value>, String> {
        Self::value(self.observatory.work_create(request))
    }

    #[tool(
        name = "work.update",
        description = "Update project work with explicit human confirmation."
    )]
    async fn work_update(
        &self,
        Parameters(request): Parameters<WorkUpdateRequest>,
    ) -> Result<Json<Value>, String> {
        Self::value(self.observatory.work_update(request))
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
    version = "0.2.0",
    instructions = "Crusty is a Rust repository observatory. Use repo.search exact for live call-site work; it never refreshes. Use change.prepare before edits and change.validate afterwards, polling both with task.get. Change preparation, validation, refresh, and research are explicit durable tasks. Research uses local repository evidence plus primary-first web_search by the attached agent; no external connectors are available. Findings are proposals and only a human may review or promote them into work. Source, compiler/runtime behavior, and human ownership remain authoritative."
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
        let names = server
            .tool_router
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect::<Vec<_>>();
        assert!(names.contains(&"repo.search".into()));
        assert!(names.contains(&"research.start".into()));
        assert!(names.contains(&"finding.promote".into()));
        assert!(!names.contains(&"repo.refresh".into()));
        assert_eq!(server.get_info().server_info.name, "Crusty");
        Ok(())
    }
}

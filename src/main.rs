use anyhow::{Context, Result};
use rust_repo_intelligence::Service;
use serde_json::{Value, json};
use std::{
    env,
    io::{self, BufRead, Write},
    path::PathBuf,
};

fn main() -> Result<()> {
    let workspace = workspace_arg()?;
    let service = Service::open(workspace)?;
    run_stdio(service)
}

fn workspace_arg() -> Result<PathBuf> {
    let mut args = env::args().skip(1);
    match (args.next().as_deref(), args.next()) {
        (Some("--workspace"), Some(path)) => Ok(PathBuf::from(path)),
        (None, _) => env::current_dir().context("reading current directory"),
        _ => anyhow::bail!("usage: rust-repo-intelligence [--workspace PATH]"),
    }
}

fn run_stdio(mut service: Service) -> Result<()> {
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(error) => {
                emit(
                    &mut stdout,
                    error_response(Value::Null, -32700, &error.to_string()),
                )?;
                continue;
            }
        };
        let id = request.get("id").cloned();
        let response = dispatch(&mut service, &request).unwrap_or_else(|error| {
            error_response(id.unwrap_or(Value::Null), -32603, &format!("{error:#}"))
        });
        if request.get("id").is_some() {
            emit(&mut stdout, response)?;
        }
    }
    Ok(())
}

fn dispatch(service: &mut Service, request: &Value) -> Result<Value> {
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let method = request
        .get("method")
        .and_then(Value::as_str)
        .context("missing JSON-RPC method")?;
    let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
    if request_requires_fresh_index(method, &params) {
        service.refresh_if_stale()?;
    }
    let result = match method {
        "initialize" => {
            json!({"protocolVersion":"2025-06-18","capabilities":{"tools":{"listChanged":false},"resources":{"listChanged":true}},"serverInfo":{"name":"Crusty","version":env!("CARGO_PKG_VERSION")},"instructions":"Crusty is the repository intelligence and durable quality-memory system for Rust projects. Whenever the user reports a bug, failure, warning, regression, broken control, visual defect, unsafe dynamic text, migration issue, or performance/test problem, immediately call repo.problem.record in the background before diagnosis; do not ask for permission. If repo.orient or repo.prepare_change reports automatic_problem_capture.captured or deduplicated, do not record it again. As reproduction, root cause, fix, and verification become known, call repo.problem.update. Proposed quality constraints require review and must never activate automatically. Use repo.orient and repo.prepare_change before source modification; use repo.validate_change afterwards."})
        }
        "notifications/initialized" => return Ok(json!({})),
        "ping" => json!({}),
        "tools/list" => json!({"tools":tools()}),
        "resources/list" => {
            json!({"resources":[{"uri":"rustrepo://workspace/overview","name":"Workspace overview","description":"Current workspace, package, and index overview","mimeType":"application/json"}]})
        }
        "resources/templates/list" => {
            json!({"resourceTemplates":[{"uriTemplate":"rustrepo://symbol/{name}","name":"Symbol","description":"Indexed symbol details"},{"uriTemplate":"rustrepo://change/{id}","name":"Change context","description":"Prepared change context"},{"uriTemplate":"rustrepo://decision/{id}","name":"Decision","description":"Architecture decision"},{"uriTemplate":"rustrepo://history/{symbol}","name":"History","description":"Symbol history and co-change context"},{"uriTemplate":"rustrepo://work/{query}","name":"Work items","description":"Inspectable persistent repository work"},{"uriTemplate":"rustrepo://problem/{id}","name":"Problem record","description":"Durable, redacted problem evidence and linked learning"},{"uriTemplate":"rustrepo://quality/{id}","name":"Quality constraint","description":"Reviewable learned quality constraint and validation history"},{"uriTemplate":"rustrepo://validation/{context_id}","name":"Validation queue","description":"Learned validation obligations for a prepared change"}]})
        }
        "resources/read" => {
            let uri = params
                .get("uri")
                .and_then(Value::as_str)
                .context("resources/read requires uri")?;
            json!({"contents":[{"uri":uri,"mimeType":"application/json","text":serde_json::to_string_pretty(&service.resource(uri)?)?}]})
        }
        "tools/call" => call_tool(service, &params)?,
        _ => return Ok(error_response(id, -32601, "method not found")),
    };
    Ok(json!({"jsonrpc":"2.0","id":id,"result":result}))
}

fn request_requires_fresh_index(method: &str, params: &Value) -> bool {
    match method {
        "tools/call" => !matches!(
            params.get("name").and_then(Value::as_str),
            Some(
                "repo.work.list"
                    | "repo.work.next"
                    | "repo.work.propose"
                    | "repo.work.update"
                    | "repo.steering.record"
                    | "repo.steering.list"
                    | "repo.status"
                    | "repo.refresh"
                    | "repo.checkpoint.create"
                    | "repo.checkpoint.list"
                    | "repo.checkpoint.diff"
                    | "repo.checkpoint.restore_branch"
                    | "repo.problem.record"
                    | "repo.problem.update"
                    | "repo.problem.list"
                    | "repo.quality.propose"
                    | "repo.quality.update"
                    | "repo.quality.merge"
                    | "repo.quality.list"
                    | "repo.validation.queue"
                    | "repo.validation.record"
            )
        ),
        "resources/read" => !params
            .get("uri")
            .and_then(Value::as_str)
            .is_some_and(|uri| {
                uri.starts_with("rustrepo://work/")
                    || uri.starts_with("rustrepo://problem/")
                    || uri.starts_with("rustrepo://quality/")
                    || uri.starts_with("rustrepo://validation/")
            }),
        _ => false,
    }
}

fn call_tool(service: &mut Service, params: &Value) -> Result<Value> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .context("tools/call requires name")?;
    let a = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let value = match name {
        "repo.orient" => service.orient(required(&a, "intent")?)?,
        "repo.locate" => service.locate(
            required(&a, "concept")?,
            item_limit(&a, 12),
            a.get("include_source")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        )?,
        "repo.context_pack" => service.context_pack(
            required(&a, "query")?,
            a.get("budget")
                .and_then(Value::as_u64)
                .map(|value| value as usize)
                .unwrap_or(3_000),
            item_limit(&a, 24),
        )?,
        "repo.explain" => service.explain(required(&a, "target")?, item_limit(&a, 12))?,
        "repo.why" => service.why(required(&a, "target")?)?,
        "repo.constraints" => service.constraints(required(&a, "change")?)?,
        "repo.prepare_change" => {
            let targets: Vec<String> = a
                .get("target")
                .or_else(|| a.get("targets"))
                .and_then(Value::as_array)
                .map(|x| {
                    x.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect::<Vec<String>>()
                })
                .unwrap_or_default();
            service.prepare_change(
                required(&a, "intent")?,
                &targets,
                a.get("depth").and_then(Value::as_u64).unwrap_or(2) as usize,
                a.get("budget").and_then(Value::as_u64).map(|x| x as usize),
            )?
        }
        "repo.expand_context" => service.expand_context(
            required(&a, "context_id")?,
            required(&a, "around")?,
            a.get("relation")
                .and_then(Value::as_str)
                .unwrap_or("references"),
        )?,
        "repo.history" => service.history(required(&a, "symbol")?)?,
        "repo.checkpoint.create" => service.checkpoint_create(
            a.get("label")
                .and_then(Value::as_str)
                .unwrap_or("codex-task"),
            a.get("include_untracked")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        )?,
        "repo.checkpoint.list" => service.checkpoint_list(item_limit(&a, 30))?,
        "repo.checkpoint.diff" => service.checkpoint_diff(
            required(&a, "reference")?,
            a.get("max_bytes")
                .and_then(Value::as_u64)
                .map(|value| value as usize)
                .unwrap_or(40_000)
                .clamp(1_000, 200_000),
        )?,
        "repo.checkpoint.restore_branch" => service.checkpoint_restore_branch(
            required(&a, "reference")?,
            a.get("branch").and_then(Value::as_str),
        )?,
        "repo.record_decision" => service.record_decision(serde_json::from_value(a)?)?,
        "repo.steering.record" => service.record_steering(serde_json::from_value(a)?)?,
        "repo.steering.list" => {
            service.steering_list(a.get("query").and_then(Value::as_str), item_limit(&a, 30))?
        }
        "repo.validate_change" => service.validate_change(
            required(&a, "context_id")?,
            a.get("git_diff").and_then(Value::as_str),
            a.get("run_checks")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        )?,
        "repo.cleanup_candidates" => {
            service.cleanup_candidates(a.get("scope").and_then(Value::as_str))?
        }
        "repo.obsolete_candidates" => service
            .obsolete_candidates(a.get("scope").and_then(Value::as_str), item_limit(&a, 30))?,
        "repo.work.list" => {
            service.work_list(a.get("query").and_then(Value::as_str), item_limit(&a, 30))?
        }
        "repo.work.next" => service.work_next(a.get("query").and_then(Value::as_str))?,
        "repo.work.propose" => service.work_propose(serde_json::from_value(a)?)?,
        "repo.work.update" => {
            let patch = a.get("patch").cloned().unwrap_or_else(|| json!({}));
            service.work_update(required(&a, "id")?, &patch)?
        }
        "repo.problem.record" => service.problem_record(serde_json::from_value(a)?)?,
        "repo.problem.update" => service.problem_update(required(&a, "id")?, &a["patch"])?,
        "repo.problem.list" => {
            service.problem_list(a.get("query").and_then(Value::as_str), item_limit(&a, 30))?
        }
        "repo.quality.propose" => service.quality_constraint_propose(serde_json::from_value(a)?)?,
        "repo.quality.update" => {
            service.quality_constraint_update(required(&a, "id")?, &a["patch"])?
        }
        "repo.quality.merge" => service
            .quality_constraint_merge(required(&a, "source_id")?, required(&a, "target_id")?)?,
        "repo.quality.list" => service
            .quality_constraint_list(a.get("query").and_then(Value::as_str), item_limit(&a, 30))?,
        "repo.quality.explain" => {
            let targets = a
                .get("target")
                .or_else(|| a.get("targets"))
                .and_then(Value::as_array)
                .map(|values| {
                    values
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            service.quality_constraint_explain(
                required(&a, "id")?,
                a.get("change").and_then(Value::as_str).unwrap_or(""),
                &targets,
            )?
        }
        "repo.validation.queue" => service.quality_validation_queue(required(&a, "context_id")?)?,
        "repo.validation.record" => {
            service.quality_validation_record(serde_json::from_value(a)?)?
        }
        "repo.status" => service.status()?,
        "repo.matrix" => service.matrix()?,
        "repo.refresh" => service.refresh(a.get("scope").and_then(Value::as_str))?,
        _ => anyhow::bail!("unknown tool: {name}"),
    };
    let mut content = vec![json!({"type":"text","text":serde_json::to_string_pretty(&value)?})];
    if let Some(id) = value.get("context_id").and_then(Value::as_str) {
        content.push(json!({"type":"resource_link","uri":format!("rustrepo://change/{id}"),"name":format!("Change context {id}"),"mimeType":"application/json"}));
    }
    if let Some(id) = value
        .get("id")
        .and_then(Value::as_str)
        .filter(|_| name == "repo.record_decision")
    {
        content.push(json!({"type":"resource_link","uri":format!("rustrepo://decision/{id}"),"name":format!("Decision {id}"),"mimeType":"application/json"}));
    }
    let quality_link = match name {
        "repo.problem.record" | "repo.problem.update" => {
            value["problem"]["id"].as_str().map(|id| ("problem", id))
        }
        "repo.quality.propose" => value["constraint"]["id"].as_str().map(|id| ("quality", id)),
        "repo.quality.update" => value["constraint"]["id"].as_str().map(|id| ("quality", id)),
        _ => None,
    };
    if let Some((kind, id)) = quality_link {
        content.push(json!({"type":"resource_link","uri":format!("rustrepo://{kind}/{id}"),"name":format!("{} {id}",if kind == "problem" { "Problem" } else { "Quality constraint" }),"mimeType":"application/json"}));
    }
    Ok(json!({"content":content,"structuredContent":value}))
}

fn required<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("missing required argument: {key}"))
}
fn item_limit(value: &Value, default: usize) -> usize {
    value
        .get("limit")
        .or_else(|| value.get("budget"))
        .and_then(Value::as_u64)
        .map(|value| value as usize)
        .unwrap_or(default)
        .clamp(1, 100)
}
fn emit(out: &mut impl Write, value: Value) -> Result<()> {
    serde_json::to_writer(&mut *out, &value)?;
    writeln!(out)?;
    out.flush()?;
    Ok(())
}
fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
}

fn tools() -> Vec<Value> {
    vec![
        tool(
            "repo.orient",
            "Map an unfamiliar task to likely architecture, crates, and symbols before changing code. Explicit defect reports are also captured automatically as durable proposed problem memory.",
            json!({"intent":{"type":"string"}}),
            vec!["intent"],
        ),
        tool(
            "repo.locate",
            "Locate a product or architectural concept with ranked code, documentation, tests, decisions, work, and explicit blind spots.",
            json!({"concept":{"type":"string"},"limit":{"type":"integer","default":12},"include_source":{"type":"boolean","default":false}}),
            vec!["concept"],
        ),
        tool(
            "repo.context_pack",
            "Build a token-budgeted context package using BM25, local symbol embeddings, typed graph expansion, reciprocal-rank fusion, decisions, steerings, work, and documentation.",
            json!({"query":{"type":"string"},"budget":{"type":"integer","minimum":250,"maximum":20000,"default":3000},"limit":{"type":"integer","minimum":1,"maximum":100,"default":24}}),
            vec!["query"],
        ),
        tool(
            "repo.explain",
            "Explain a target using source, lifecycle, decision, and work evidence.",
            json!({"target":{"type":"string"},"limit":{"type":"integer","default":12}}),
            vec!["target"],
        ),
        tool(
            "repo.why",
            "Show why a target exists from decisions, lifecycle evidence, and Git history.",
            json!({"target":{"type":"string"}}),
            vec!["target"],
        ),
        tool(
            "repo.constraints",
            "Return decision and lifecycle constraints for a proposed change, plus explicit blind spots.",
            json!({"change":{"type":"string"}}),
            vec!["change"],
        ),
        tool(
            "repo.prepare_change",
            "Create a provenance-labelled impact briefing and source slices before the first modification. Explicit defect reports are captured automatically if they were not already recorded.",
            json!({"intent":{"type":"string"},"target":{"type":"array","items":{"type":"string"}},"depth":{"type":"integer","minimum":1,"default":2},"budget":{"type":"integer","minimum":250,"default":3000,"description":"Approximate source-context token budget"}}),
            vec!["intent"],
        ),
        tool(
            "repo.expand_context",
            "Expand a prepared context around a symbol and relationship.",
            json!({"context_id":{"type":"string"},"around":{"type":"string"},"relation":{"type":"string","enum":["references","callers","implementations"]}}),
            vec!["context_id", "around"],
        ),
        tool(
            "repo.history",
            "Return Git history and co-change relationships for a symbol.",
            json!({"symbol":{"type":"string"}}),
            vec!["symbol"],
        ),
        tool(
            "repo.checkpoint.create",
            "Capture the current tracked worktree and index in a hidden Git checkpoint ref without changing the branch or working tree. Untracked files require explicit opt-in.",
            json!({"label":{"type":"string","default":"codex-task"},"include_untracked":{"type":"boolean","default":false}}),
            vec![],
        ),
        tool(
            "repo.checkpoint.list",
            "List Git-backed Codex checkpoint refs.",
            json!({"limit":{"type":"integer","default":30}}),
            vec![],
        ),
        tool(
            "repo.checkpoint.diff",
            "Show the binary-capable patch from a checkpoint to the current working tree, with a bounded response.",
            json!({"reference":{"type":"string"},"max_bytes":{"type":"integer","default":40000}}),
            vec!["reference"],
        ),
        tool(
            "repo.checkpoint.restore_branch",
            "Create a codex/ restore branch at a checkpoint without checking it out or modifying the working tree.",
            json!({"reference":{"type":"string"},"branch":{"type":"string"}}),
            vec!["reference"],
        ),
        tool(
            "repo.record_decision",
            "Record an explicit architecture decision, optionally materializing an ADR markdown file.",
            json!({"title":{"type":"string"},"status":{"type":"string","default":"accepted"},"reason":{"type":"string"},"applies_to":{"type":"array","items":{"type":"string"}},"consequences":{"type":"array","items":{"type":"string"}},"supersedes":{"type":"string"},"materialize":{"type":"boolean","default":false}}),
            vec!["title"],
        ),
        tool(
            "repo.steering.record",
            "Record a scoped, prioritized project steering for future change contexts.",
            json!({"title":{"type":"string"},"instruction":{"type":"string"},"scope":{"type":"array","items":{"type":"string"}},"priority":{"type":"string","default":"normal"},"status":{"type":"string","default":"active"},"expires_at":{"type":"string"}}),
            vec!["title", "instruction"],
        ),
        tool(
            "repo.steering.list",
            "List or full-text search persisted project steerings.",
            json!({"query":{"type":"string"},"limit":{"type":"integer","default":30}}),
            vec![],
        ),
        tool(
            "repo.validate_change",
            "Compare a diff against a prepared change context and optionally run Rust quality checks.",
            json!({"context_id":{"type":"string"},"git_diff":{"type":"string"},"run_checks":{"type":"boolean","default":false}}),
            vec!["context_id"],
        ),
        tool(
            "repo.cleanup_candidates",
            "Find private, unreferenced candidates, with conservative confidence labels.",
            json!({"scope":{"type":"string"}}),
            vec![],
        ),
        tool(
            "repo.obsolete_candidates",
            "Find referenced-but-superseded compatibility paths only when source evidence identifies lifecycle context; candidates include a complete removal/verification slice.",
            json!({"scope":{"type":"string"},"limit":{"type":"integer","default":30}}),
            vec![],
        ),
        tool(
            "repo.work.list",
            "List evidence-backed repository work. Automatically discovered items are proposed, not accepted.",
            json!({"query":{"type":"string"},"limit":{"type":"integer","default":30}}),
            vec![],
        ),
        tool(
            "repo.work.next",
            "Return the highest-priority unblocked work candidate.",
            json!({"query":{"type":"string"}}),
            vec![],
        ),
        tool(
            "repo.work.propose",
            "Create or update an explicit, inspectable work item.",
            json!({"title":{"type":"string"},"status":{"type":"string","default":"proposed"},"priority":{"type":"string","default":"normal"},"kind":{"type":"string","default":"cleanup"},"scope":{"type":"array","items":{"type":"string"}},"evidence":{"type":"array","items":{"type":"string"}},"depends_on":{"type":"array","items":{"type":"string"}},"blocked_by":{"type":"array","items":{"type":"string"}},"acceptance_criteria":{"type":"array","items":{"type":"string"}},"verification":{"type":"array","items":{"type":"string"}}}),
            vec!["title"],
        ),
        tool(
            "repo.work.update",
            "Update the inspectable status, dependencies, evidence, or verification for an existing work item.",
            json!({"id":{"type":"string"},"patch":{"type":"object"}}),
            vec!["id"],
        ),
        tool(
            "repo.problem.record",
            "Always call this automatically, without asking permission, when the user reports a bug, failure, warning, regression, broken control, visual defect, unsafe dynamic text, migration issue, or performance/test problem. It redacts and deduplicates the report and creates a reviewable constraint proposal. Do not call again when orient or prepare_change says the same report was automatically captured.",
            problem_input_schema(),
            vec!["report"],
        ),
        tool(
            "repo.problem.update",
            "Call as evidence becomes available to update the lifecycle, reproduction, diagnosis, root cause, fix link, verification, relationships, or scope of a durable problem record.",
            json!({"id":{"type":"string"},"patch":problem_patch_schema()}),
            vec!["id", "patch"],
        ),
        tool(
            "repo.problem.list",
            "List or full-text search historical problem records and their linked constraint proposals.",
            json!({"query":{"type":"string"},"limit":{"type":"integer","default":30}}),
            vec![],
        ),
        tool(
            "repo.quality.propose",
            "Propose a scoped learned quality constraint. Proposals never affect change scope until explicitly approved and activated.",
            quality_constraint_input_schema(),
            vec!["rule", "category", "scope", "recipe"],
        ),
        tool(
            "repo.quality.update",
            "Review, edit, approve, reject, activate, disable, or retire a learned quality constraint.",
            json!({"id":{"type":"string"},"patch":quality_constraint_patch_schema()}),
            vec!["id", "patch"],
        ),
        tool(
            "repo.quality.merge",
            "Merge a duplicate learned constraint into a canonical constraint while preserving problem links and history.",
            json!({"source_id":{"type":"string"},"target_id":{"type":"string"}}),
            vec!["source_id", "target_id"],
        ),
        tool(
            "repo.quality.list",
            "List or full-text search learned quality constraints with status, scope, provenance, and maturity.",
            json!({"query":{"type":"string"},"limit":{"type":"integer","default":30}}),
            vec![],
        ),
        tool(
            "repo.quality.explain",
            "Explain a learned constraint from original problem evidence and show exactly why an optional change matches or does not match.",
            json!({"id":{"type":"string"},"change":{"type":"string"},"target":{"type":"array","items":{"type":"string"}}}),
            vec!["id"],
        ),
        tool(
            "repo.validation.queue",
            "Inspect the repository, learned, and change-specific validation obligations for a prepared change context.",
            json!({"context_id":{"type":"string"}}),
            vec!["context_id"],
        ),
        tool(
            "repo.validation.record",
            "Record a passed, failed, skipped, or unavailable validation outcome with redacted evidence and durable provenance.",
            json!({"obligation_id":{"type":"string"},"status":{"type":"string","enum":["passed","failed","skipped","unavailable"]},"evidence":{"type":"array","items":{"type":"string"}}}),
            vec!["obligation_id", "status"],
        ),
        tool(
            "repo.status",
            "Return exact snapshot, index state, counts, and known blind spots.",
            json!({}),
            vec![],
        ),
        tool(
            "repo.matrix",
            "Return the indexed Cargo package feature and target matrix.",
            json!({}),
            vec![],
        ),
        tool(
            "repo.refresh",
            "Refresh workspace, Cargo, Git, or incrementally stale repository state. Returns the snapshot and degraded areas.",
            json!({"scope":{"type":"string","enum":["workspace","cargo","git","incremental"]}}),
            vec![],
        ),
    ]
}

fn quality_scope_schema() -> Value {
    let strings = || json!({"type":"array","items":{"type":"string"},"default":[]});
    json!({
        "type":"object",
        "properties":{
            "crates":strings(),"files":strings(),"modules":strings(),"symbols":strings(),
            "components":strings(),"dependencies":strings(),"concepts":strings(),
            "widget_types":strings(),"css_classes":strings(),"design_tokens":strings(),
            "diagnostics":strings(),"configurations":strings()
        },
        "additionalProperties":false
    })
}

fn validation_recipe_schema() -> Value {
    json!({
        "type":"object",
        "properties":{
            "kind":{"type":"string","enum":["cargo","focused_test","runtime_log","ui_smoke","structural_source","widget_tree","visual_comparison","manual"]},
            "expected":{"type":"string"},
            "command":{"type":"string"},
            "procedure":{"type":"string"},
            "environment":{"type":"object","additionalProperties":{"type":"string"}}
        },
        "required":["kind","expected"],
        "additionalProperties":false
    })
}

fn problem_input_schema() -> Value {
    json!({
        "report":{"type":"string"},"summary":{"type":"string"},
        "defect_family":{"type":"string","enum":["compiler_warning","clippy_warning","runtime_diagnostic","gtk_diagnostic","action_wiring","visual_consistency","dynamic_text_markup","database_migration","performance_regression","test_regression","uncategorized"]},
        "status":{"type":"string","enum":["reported","reproduced","diagnosed","fixed","verified","obsolete"],"default":"reported"},
        "confidence":{"type":"number","minimum":0,"maximum":1,"default":0.8},
        "scope":quality_scope_schema(),"reproduction":{"type":"string"},
        "diagnostic_signature":{"type":"string"},"root_cause":{"type":"string"},
        "fix_reference":{"type":"string"},"evidence":{"type":"array","items":{"type":"string"}},
        "related":{"type":"array","items":{"type":"string"}},
        "provenance":{"type":"string","default":"HumanReport"}
    })
}

fn problem_patch_schema() -> Value {
    json!({
        "type":"object",
        "description":"Partial problem update.",
        "properties":{
            "summary":{"type":"string"},
            "status":{"type":"string","enum":["reported","reproduced","diagnosed","fixed","verified","obsolete"]},
            "confidence":{"type":"number","minimum":0,"maximum":1},
            "scope":quality_scope_schema(),"reproduction":{"type":"string"},
            "diagnostic_signature":{"type":"string"},"root_cause":{"type":"string"},
            "fix_reference":{"type":"string"},"evidence":{"type":"array","items":{"type":"string"}},
            "related":{"type":"array","items":{"type":"string"}}
        },
        "additionalProperties":false
    })
}

fn quality_constraint_input_schema() -> Value {
    json!({
        "rule":{"type":"string"},
        "category":{"type":"string","enum":["compiler_warning","clippy_warning","runtime_diagnostic","gtk_diagnostic","action_wiring","visual_consistency","dynamic_text_markup","database_migration","performance_regression","test_regression","uncategorized"]},
        "problem_ids":{"type":"array","items":{"type":"string"}},
        "scope":quality_scope_schema(),"exclusions":quality_scope_schema(),
        "activation_criteria":{"type":"array","items":{"type":"string"}},
        "recipe":validation_recipe_schema(),
        "enforcement":{"type":"string","enum":["observe","validate","block"],"default":"observe"},
        "confidence":{"type":"number","minimum":0,"maximum":1,"default":0.8},
        "maturity":{"type":"string","enum":["proposed","approved","established"],"default":"proposed"},
        "provenance":{"type":"array","items":{"type":"string"}},
        "expires_at":{"type":"string"},
        "invalidation_conditions":{"type":"array","items":{"type":"string"}}
    })
}

fn quality_constraint_patch_schema() -> Value {
    json!({
        "type":"object",
        "description":"Partial constraint update. Active constraints need approved or established maturity.",
        "properties":{
            "rule":{"type":"string"},
            "category":{"type":"string","enum":["compiler_warning","clippy_warning","runtime_diagnostic","gtk_diagnostic","action_wiring","visual_consistency","dynamic_text_markup","database_migration","performance_regression","test_regression","uncategorized"]},
            "status":{"type":"string","enum":["proposed","active","rejected","disabled","retired","merged"]},
            "scope":quality_scope_schema(),"exclusions":quality_scope_schema(),
            "activation_criteria":{"type":"array","items":{"type":"string"}},
            "recipe":validation_recipe_schema(),
            "enforcement":{"type":"string","enum":["observe","validate","block"]},
            "confidence":{"type":"number","minimum":0,"maximum":1},
            "maturity":{"type":"string","enum":["proposed","approved","established","deprecated"]},
            "expires_at":{"type":"string"},
            "invalidation_conditions":{"type":"array","items":{"type":"string"}}
        },
        "additionalProperties":false
    })
}

fn tool(name: &str, description: &str, properties: Value, required: Vec<&str>) -> Value {
    json!({"name":name,"description":description,"inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false}})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialization_identifies_crusty_and_instructs_defect_capture() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(
            workspace.path().join("Cargo.toml"),
            "[package]\nname='instruction-test'\nversion='0.1.0'\nedition='2024'\n",
        )
        .unwrap();
        std::fs::create_dir(workspace.path().join("src")).unwrap();
        std::fs::write(workspace.path().join("src/lib.rs"), "pub fn run() {}\n").unwrap();
        let mut service = Service::open(workspace.path()).unwrap();
        let response = dispatch(
            &mut service,
            &json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
        )
        .unwrap();
        assert_eq!(response["result"]["serverInfo"]["name"], "Crusty");
        let instructions = response["result"]["instructions"].as_str().unwrap();
        assert!(instructions.starts_with("Crusty is the repository intelligence"));
        assert!(instructions.contains("repo.problem.record"));
        assert!(instructions.contains("repo.problem.update"));
        assert!(instructions.contains("must never activate automatically"));
    }

    #[test]
    fn durable_memory_requests_do_not_refresh_the_repository_index() {
        assert_eq!(tools().len(), 36);
        assert!(
            tools()
                .iter()
                .any(|tool| tool["name"] == "repo.context_pack")
        );
        for name in [
            "repo.work.list",
            "repo.work.next",
            "repo.work.propose",
            "repo.work.update",
            "repo.steering.record",
            "repo.steering.list",
            "repo.status",
            "repo.refresh",
            "repo.checkpoint.create",
            "repo.checkpoint.list",
            "repo.checkpoint.diff",
            "repo.checkpoint.restore_branch",
            "repo.problem.record",
            "repo.problem.update",
            "repo.problem.list",
            "repo.quality.propose",
            "repo.quality.update",
            "repo.quality.merge",
            "repo.quality.list",
            "repo.validation.queue",
            "repo.validation.record",
        ] {
            assert!(!request_requires_fresh_index(
                "tools/call",
                &json!({"name": name})
            ));
        }
        assert!(!request_requires_fresh_index(
            "resources/read",
            &json!({"uri": "rustrepo://work/queued"})
        ));
        assert!(!request_requires_fresh_index(
            "resources/read",
            &json!({"uri": "rustrepo://quality/QC-0001"})
        ));
        assert!(request_requires_fresh_index(
            "tools/call",
            &json!({"name": "repo.orient"})
        ));
        assert!(request_requires_fresh_index(
            "tools/call",
            &json!({"name": "repo.quality.explain"})
        ));
        assert!(request_requires_fresh_index(
            "resources/read",
            &json!({"uri": "rustrepo://symbol/Store"})
        ));

        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(
            workspace.path().join("Cargo.toml"),
            "[package]\nname='queue-test'\nversion='0.1.0'\nedition='2024'\n",
        )
        .unwrap();
        std::fs::create_dir(workspace.path().join("src")).unwrap();
        std::fs::write(workspace.path().join("src/lib.rs"), "pub fn queued() {}\n").unwrap();
        let mut service = Service::open(workspace.path()).unwrap();
        dispatch(
            &mut service,
            &json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {"name": "repo.work.list", "arguments": {}}
            }),
        )
        .unwrap();
        let status = service.status().unwrap();
        assert_eq!(status["counts"]["nodes"], 0);
        assert_eq!(status["snapshot"]["index"]["rust_analyzer_running"], false);
    }

    #[test]
    fn existing_mcp_tool_contract_remains_available() {
        let names = tools()
            .into_iter()
            .filter_map(|tool| tool["name"].as_str().map(str::to_owned))
            .collect::<std::collections::BTreeSet<_>>();
        for name in [
            "repo.orient",
            "repo.locate",
            "repo.context_pack",
            "repo.explain",
            "repo.why",
            "repo.constraints",
            "repo.prepare_change",
            "repo.expand_context",
            "repo.history",
            "repo.checkpoint.create",
            "repo.checkpoint.list",
            "repo.checkpoint.diff",
            "repo.checkpoint.restore_branch",
            "repo.record_decision",
            "repo.steering.record",
            "repo.steering.list",
            "repo.validate_change",
            "repo.cleanup_candidates",
            "repo.obsolete_candidates",
            "repo.work.list",
            "repo.work.next",
            "repo.work.propose",
            "repo.work.update",
            "repo.status",
            "repo.matrix",
            "repo.refresh",
        ] {
            assert!(names.contains(name), "missing existing MCP tool {name}");
        }
    }
}

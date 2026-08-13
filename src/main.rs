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
    if matches!(method, "tools/call" | "resources/read") {
        service.refresh_if_stale()?;
    }
    let result = match method {
        "initialize" => {
            json!({"protocolVersion":"2025-06-18","capabilities":{"tools":{"listChanged":false},"resources":{"listChanged":true}},"serverInfo":{"name":"rust-repo-intelligence","version":env!("CARGO_PKG_VERSION")},"instructions":"Use repo.orient and repo.prepare_change before source modification; use repo.validate_change afterwards."})
        }
        "notifications/initialized" => return Ok(json!({})),
        "ping" => json!({}),
        "tools/list" => json!({"tools":tools()}),
        "resources/list" => {
            json!({"resources":[{"uri":"rustrepo://workspace/overview","name":"Workspace overview","description":"Current workspace, package, and index overview","mimeType":"application/json"}]})
        }
        "resources/templates/list" => {
            json!({"resourceTemplates":[{"uriTemplate":"rustrepo://symbol/{name}","name":"Symbol","description":"Indexed symbol details"},{"uriTemplate":"rustrepo://change/{id}","name":"Change context","description":"Prepared change context"},{"uriTemplate":"rustrepo://decision/{id}","name":"Decision","description":"Architecture decision"},{"uriTemplate":"rustrepo://history/{symbol}","name":"History","description":"Symbol history and co-change context"},{"uriTemplate":"rustrepo://work/{query}","name":"Work items","description":"Inspectable persistent repository work"}]})
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
        "repo.record_decision" => service.record_decision(serde_json::from_value(a)?)?,
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
            "Map an unfamiliar task to likely architecture, crates, and symbols before changing code.",
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
            "Create a provenance-labelled impact briefing and source slices before the first modification.",
            json!({"intent":{"type":"string"},"target":{"type":"array","items":{"type":"string"}},"depth":{"type":"integer","minimum":1,"default":2},"budget":{"type":"integer","minimum":1000,"default":12000}}),
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
            "repo.record_decision",
            "Record an explicit architecture decision, optionally materializing an ADR markdown file.",
            json!({"title":{"type":"string"},"status":{"type":"string","default":"accepted"},"reason":{"type":"string"},"applies_to":{"type":"array","items":{"type":"string"}},"consequences":{"type":"array","items":{"type":"string"}},"supersedes":{"type":"string"},"materialize":{"type":"boolean","default":false}}),
            vec!["title"],
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
fn tool(name: &str, description: &str, properties: Value, required: Vec<&str>) -> Value {
    json!({"name":name,"description":description,"inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false}})
}

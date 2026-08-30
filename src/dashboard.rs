//! Secure loopback dashboard for reviewing research findings.

use crate::observatory::{Observatory, PromoteFindingRequest, ReviewFindingRequest};
use anyhow::Result;
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use rand::random;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tokio::net::TcpListener;

#[derive(Clone)]
struct DashboardState {
    observatory: Observatory,
    bootstrap_token: Arc<Mutex<Option<String>>>,
    session_token: String,
    csrf_token: String,
    csp_nonce: String,
}

/// A running dashboard listener that can mint a fresh one-time URL on demand.
///
/// The bootstrap token is deliberately single-use, so caching the first URL
/// meant every later `dashboard.open` handed the human a spent link that could
/// only answer 401. The handle keeps one listener and re-issues the token
/// instead.
#[derive(Clone)]
pub struct DashboardHandle {
    address: String,
    bootstrap_token: Arc<Mutex<Option<String>>>,
}

impl DashboardHandle {
    /// Replaces any outstanding bootstrap token and returns a usable URL.
    pub fn issue_url(&self) -> String {
        let bootstrap_token = token();
        *self
            .bootstrap_token
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(bootstrap_token.clone());
        format!("http://{}/?token={bootstrap_token}", self.address)
    }
}

pub async fn start(observatory: Observatory) -> Result<DashboardHandle> {
    let bootstrap_token = Arc::new(Mutex::new(None));
    let state = DashboardState {
        observatory,
        bootstrap_token: bootstrap_token.clone(),
        session_token: token(),
        csrf_token: token(),
        csp_nonce: token(),
    };
    let app = Router::new()
        .route("/", get(index))
        .route("/api/status", get(api_status))
        .route("/api/findings", get(api_findings))
        .route("/api/research", get(api_research))
        .route("/api/work", get(api_work))
        .route("/api/findings/{id}/review", post(api_review))
        .route("/api/findings/{id}/promote", post(api_promote))
        .with_state(state);
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, app).await {
            eprintln!("Crusty dashboard stopped: {error}");
        }
    });
    Ok(DashboardHandle {
        address: address.to_string(),
        bootstrap_token,
    })
}

async fn index(
    State(state): State<DashboardState>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let already_authenticated =
        session_from(&headers).is_some_and(|value| value == state.session_token);
    let mut establish_session = false;
    if !already_authenticated {
        let supplied = query.get("token");
        let mut bootstrap = state
            .bootstrap_token
            .lock()
            .expect("bootstrap mutex poisoned");
        if supplied.is_some() && supplied == bootstrap.as_ref() {
            bootstrap.take();
            establish_session = true;
        } else {
            return secure_response(
                (StatusCode::UNAUTHORIZED, "Dashboard session required").into_response(),
                &state,
            );
        }
    }
    let html = DASHBOARD_HTML
        .replace("{{CSRF}}", &state.csrf_token)
        .replace("{{NONCE}}", &state.csp_nonce);
    let mut response = Html(html).into_response();
    if establish_session {
        let cookie = format!(
            "crusty_session={}; HttpOnly; SameSite=Strict; Path=/",
            state.session_token
        );
        if let Ok(value) = HeaderValue::from_str(&cookie) {
            response.headers_mut().insert(header::SET_COOKIE, value);
        }
    }
    secure_response(response, &state)
}

#[derive(Default, Deserialize)]
struct ListQuery {
    status: Option<String>,
    query: Option<String>,
    limit: Option<usize>,
}

async fn api_status(State(state): State<DashboardState>, headers: HeaderMap) -> Response {
    read_api(&state, &headers, || state.observatory.index_status())
}

async fn api_findings(
    State(state): State<DashboardState>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> Response {
    read_api(&state, &headers, || {
        state.observatory.finding_list(
            query.status.as_deref(),
            query.query.as_deref(),
            query.limit.unwrap_or(100),
        )
    })
}

async fn api_research(State(state): State<DashboardState>, headers: HeaderMap) -> Response {
    read_api(&state, &headers, || {
        state.observatory.research_list(None, None, 100)
    })
}

async fn api_work(
    State(state): State<DashboardState>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> Response {
    read_api(&state, &headers, || {
        state
            .observatory
            .work_list(query.query.as_deref(), query.limit.unwrap_or(100))
    })
}

#[derive(Deserialize)]
struct ReviewBody {
    decision: String,
    reviewed_by: String,
    note: Option<String>,
}

async fn api_review(
    State(state): State<DashboardState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<ReviewBody>,
) -> Response {
    write_api(&state, &headers, || {
        state.observatory.finding_review(ReviewFindingRequest {
            finding_id: id,
            decision: body.decision,
            reviewed_by: body.reviewed_by,
            note: body.note.unwrap_or_default(),
        })
    })
}

#[derive(Deserialize)]
struct PromoteBody {
    reviewed_by: String,
    priority: Option<String>,
    kind: Option<String>,
    acceptance_criteria: Option<Vec<String>>,
    verification: Option<Vec<String>>,
}

async fn api_promote(
    State(state): State<DashboardState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<PromoteBody>,
) -> Response {
    write_api(&state, &headers, || {
        state.observatory.finding_promote(PromoteFindingRequest {
            finding_id: id,
            reviewed_by: body.reviewed_by,
            confirm_human: true,
            priority: body.priority.unwrap_or_else(|| "normal".into()),
            kind: body.kind.unwrap_or_else(|| "improvement".into()),
            acceptance_criteria: body.acceptance_criteria.unwrap_or_default(),
            verification: body.verification.unwrap_or_default(),
        })
    })
}

fn read_api(
    state: &DashboardState,
    headers: &HeaderMap,
    operation: impl FnOnce() -> Result<Value>,
) -> Response {
    if !authorized(state, headers) {
        return unauthorized(state);
    }
    json_result(state, operation())
}

fn write_api(
    state: &DashboardState,
    headers: &HeaderMap,
    operation: impl FnOnce() -> Result<Value>,
) -> Response {
    if !authorized(state, headers) {
        return unauthorized(state);
    }
    if headers
        .get("x-crusty-csrf")
        .and_then(|value| value.to_str().ok())
        != Some(state.csrf_token.as_str())
    {
        return secure_response(
            (
                StatusCode::FORBIDDEN,
                Json(json!({"error":"CSRF token required"})),
            )
                .into_response(),
            state,
        );
    }
    json_result(state, operation())
}

fn json_result(state: &DashboardState, result: Result<Value>) -> Response {
    match result {
        Ok(value) => secure_response(Json(value).into_response(), state),
        Err(error) => secure_response(
            (
                StatusCode::BAD_REQUEST,
                Json(json!({"error":format!("{error:#}")})),
            )
                .into_response(),
            state,
        ),
    }
}

fn unauthorized(state: &DashboardState) -> Response {
    secure_response(
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error":"Dashboard session required"})),
        )
            .into_response(),
        state,
    )
}

fn authorized(state: &DashboardState, headers: &HeaderMap) -> bool {
    session_from(headers).is_some_and(|value| value == state.session_token)
}

fn session_from(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .map(str::trim)
        .find_map(|cookie| cookie.strip_prefix("crusty_session="))
}

fn secure_response(mut response: Response, state: &DashboardState) -> Response {
    let csp = format!(
        "default-src 'none'; script-src 'nonce-{}'; style-src 'nonce-{}'; connect-src 'self'; img-src 'self' data:; base-uri 'none'; form-action 'self'; frame-ancestors 'none'",
        state.csp_nonce, state.csp_nonce
    );
    if let Ok(value) = HeaderValue::from_str(&csp) {
        response
            .headers_mut()
            .insert(header::CONTENT_SECURITY_POLICY, value);
    }
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response.headers_mut().insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn token() -> String {
    random::<[u8; 32]>()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

const DASHBOARD_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width,initial-scale=1">
  <title>Crusty Observatory</title>
  <style nonce="{{NONCE}}">
    :root{color-scheme:dark;--ink:#f5f0e7;--muted:#aaa69f;--panel:#181a1c;--line:#303438;--ember:#f27649;--mint:#75d5b2;--warn:#f2c14e;--bg:#0d0f10}*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--ink);font:15px/1.45 ui-sans-serif,system-ui,sans-serif}button,input,select{font:inherit}header{display:flex;align-items:center;justify-content:space-between;padding:22px 28px;border-bottom:1px solid var(--line);position:sticky;top:0;background:rgba(13,15,16,.94);backdrop-filter:blur(12px);z-index:2}.brand{display:flex;gap:13px;align-items:center}.mark{width:36px;height:36px;border-radius:50% 44% 48% 42%;background:var(--ember);box-shadow:inset -7px -4px 0 #bf4d2d}.brand h1{font:700 20px/1.1 ui-monospace,monospace;margin:0}.brand small{color:var(--muted)}.fresh{font:12px ui-monospace,monospace;color:var(--mint);padding:7px 10px;border:1px solid #285848;border-radius:999px}main{max-width:1440px;margin:auto;padding:28px}.hero{display:grid;grid-template-columns:minmax(0,1.3fr) minmax(260px,.7fr);gap:22px;margin-bottom:24px}.hero>section,.stats{background:linear-gradient(145deg,#1a1d1f,#121416);border:1px solid var(--line);border-radius:18px;padding:24px}.eyebrow{color:var(--ember);font:700 12px ui-monospace,monospace;text-transform:uppercase;letter-spacing:.14em}.hero h2{font-size:clamp(28px,4vw,52px);line-height:1.02;letter-spacing:-.04em;max-width:780px;margin:10px 0}.hero p{max-width:720px;color:var(--muted);font-size:16px}.stats{display:grid;grid-template-columns:1fr 1fr;gap:12px}.stat{border:1px solid var(--line);padding:14px;border-radius:12px}.stat strong{display:block;font:700 26px ui-monospace,monospace}.stat span{color:var(--muted);font-size:12px}.toolbar{display:flex;gap:10px;flex-wrap:wrap;margin:18px 0}.toolbar input,.toolbar select{background:var(--panel);color:var(--ink);border:1px solid var(--line);border-radius:9px;padding:10px 12px}.toolbar input{min-width:280px;flex:1}.grid{display:grid;grid-template-columns:repeat(auto-fit,minmax(310px,1fr));gap:14px}.card{background:var(--panel);border:1px solid var(--line);border-radius:14px;padding:18px;display:flex;flex-direction:column;gap:12px}.card:hover{border-color:#555c61}.meta{display:flex;gap:7px;flex-wrap:wrap}.tag{font:11px ui-monospace,monospace;border:1px solid var(--line);padding:4px 7px;border-radius:999px;color:var(--muted)}.tag.high,.tag.critical{color:var(--warn);border-color:#715f28}.tag.product{color:#9fc7ff}.tag.design{color:#d8a8ff}.card h3{font-size:18px;margin:0}.card p{margin:0;color:var(--muted)}.scope{font:12px ui-monospace,monospace;color:#8e979d}.actions{display:flex;gap:8px;margin-top:auto}.actions button{border:1px solid var(--line);border-radius:8px;padding:8px 10px;background:#222629;color:var(--ink);cursor:pointer}.actions button.primary{background:var(--ember);border-color:var(--ember);color:#190b06;font-weight:700}.actions button:hover{filter:brightness(1.12)}.empty{border:1px dashed var(--line);border-radius:14px;padding:50px;text-align:center;color:var(--muted)}.notice{position:fixed;right:24px;bottom:24px;background:#222629;border:1px solid var(--line);padding:12px 16px;border-radius:9px;display:none}@media(max-width:760px){header{padding:16px}main{padding:16px}.hero{grid-template-columns:1fr}.hero h2{font-size:34px}.stats{grid-template-columns:repeat(4,1fr)}.stat{padding:9px}.stat strong{font-size:20px}.stat span{font-size:10px}}
  </style>
</head>
<body>
<header><div class="brand"><div class="mark" aria-hidden="true"></div><div><h1>Crusty</h1><small>Repository observatory</small></div></div><div class="fresh" id="fresh">checking snapshot…</div></header>
<main>
  <div class="hero"><section><div class="eyebrow">Finding inbox</div><h2>Evidence worth a human decision.</h2><p>Crusty explores technical, product, and design improvement potential. Findings stay proposals until you review them; accepted findings still require an explicit promotion before they become project work.</p></section><div class="stats"><div class="stat"><strong id="proposed">–</strong><span>to review</span></div><div class="stat"><strong id="accepted">–</strong><span>accepted</span></div><div class="stat"><strong id="research">–</strong><span>research runs</span></div><div class="stat"><strong id="work">–</strong><span>human work</span></div></div></div>
  <div class="toolbar"><input id="query" type="search" placeholder="Search findings, evidence, or scope" aria-label="Search findings"><select id="status" aria-label="Finding status"><option value="">All states</option><option value="proposed" selected>Needs review</option><option value="accepted">Accepted</option><option value="needs_evidence">Needs evidence</option><option value="dismissed">Dismissed</option></select></div>
  <section class="grid" id="findings" aria-live="polite"></section>
</main><div class="notice" id="notice" role="status"></div>
<script nonce="{{NONCE}}">
history.replaceState({},document.title,'/');
const csrf='{{CSRF}}', grid=document.querySelector('#findings'), notice=document.querySelector('#notice');
const esc=v=>String(v??'');
async function api(url,options={}){options.headers={...(options.headers||{}),'content-type':'application/json','x-crusty-csrf':csrf};const response=await fetch(url,options);const body=await response.json();if(!response.ok)throw new Error(body.error||response.statusText);return body}
function flash(message){notice.textContent=message;notice.style.display='block';setTimeout(()=>notice.style.display='none',2600)}
function tag(value){const node=document.createElement('span');node.className='tag '+esc(value);node.textContent=esc(value);return node}
function card(f){const node=document.createElement('article');node.className='card';const meta=document.createElement('div');meta.className='meta';[f.category,f.severity,f.status,Math.round(f.confidence*100)+'% confidence'].forEach(v=>meta.append(tag(v)));const title=document.createElement('h3');title.textContent=f.title;const summary=document.createElement('p');summary.textContent=f.summary;const scope=document.createElement('div');scope.className='scope';scope.textContent=(f.scope||[]).join(' · ')||'repository-wide';const actions=document.createElement('div');actions.className='actions';if(f.status==='proposed'||f.status==='needs_evidence'){const accept=document.createElement('button');accept.className='primary';accept.textContent='Accept finding';accept.onclick=()=>review(f.id,'accepted');const evidence=document.createElement('button');evidence.textContent='Needs evidence';evidence.onclick=()=>review(f.id,'needs_evidence');const dismiss=document.createElement('button');dismiss.textContent='Dismiss';dismiss.onclick=()=>review(f.id,'dismissed');actions.append(accept,evidence,dismiss)}if(f.status==='accepted'){const promote=document.createElement('button');promote.className='primary';promote.textContent='Promote to work';promote.onclick=()=>promoteFinding(f.id);actions.append(promote)}node.append(meta,title,summary,scope,actions);return node}
async function load(){const status=document.querySelector('#status').value,query=document.querySelector('#query').value;const [findings,runs,work,statusData]=await Promise.all([api('/api/findings?status='+encodeURIComponent(status)+'&query='+encodeURIComponent(query)),api('/api/research'),api('/api/work'),api('/api/status')]);grid.replaceChildren();findings.findings.forEach(f=>grid.append(card(f)));if(!findings.findings.length){const empty=document.createElement('div');empty.className='empty';empty.textContent='No findings in this view.';grid.append(empty)}document.querySelector('#proposed').textContent=findings.findings.filter(f=>f.status==='proposed').length;document.querySelector('#accepted').textContent=findings.findings.filter(f=>f.status==='accepted').length;document.querySelector('#research').textContent=runs.runs.length;document.querySelector('#work').textContent=work.items.filter(w=>w.human_owned).length;const stale=statusData.freshness.stale;document.querySelector('#fresh').textContent=stale?'snapshot stale · live search available':'snapshot current'}
async function review(id,decision){const reviewer=prompt('Reviewer name');if(!reviewer)return;await api('/api/findings/'+encodeURIComponent(id)+'/review',{method:'POST',body:JSON.stringify({decision,reviewed_by:reviewer})});flash('Finding '+decision);load()}
async function promoteFinding(id){const reviewer=prompt('Confirm your name to create human-owned work');if(!reviewer)return;await api('/api/findings/'+encodeURIComponent(id)+'/promote',{method:'POST',body:JSON.stringify({reviewed_by:reviewer})});flash('Promoted to project work');load()}
document.querySelector('#status').onchange=load;let timer;document.querySelector('#query').oninput=()=>{clearTimeout(timer);timer=setTimeout(load,180)};load().catch(error=>flash(error.message));
</script>
</body></html>"#;

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn dashboard_has_no_external_asset_or_script_urls() {
        assert!(!DASHBOARD_HTML.contains("http://"));
        assert!(!DASHBOARD_HTML.contains("https://"));
        assert!(DASHBOARD_HTML.contains("Finding inbox"));
    }

    #[tokio::test]
    async fn loopback_dashboard_requires_session_and_csrf() -> Result<()> {
        let directory = tempdir()?;
        fs::write(
            directory.path().join("Cargo.toml"),
            "[package]\nname='fixture'\nversion='0.1.0'\nedition='2024'\n",
        )?;
        fs::create_dir(directory.path().join("src"))?;
        fs::write(directory.path().join("src/lib.rs"), "")?;
        let handle = start(Observatory::open(directory.path())?).await?;
        let url = handle.issue_url();
        let without_scheme = url.strip_prefix("http://").expect("loopback URL");
        let (address, target) = without_scheme.split_once('/').expect("URL path");

        let authenticated = request(
            address,
            &format!("GET /{target} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"),
        )
        .await?;
        assert!(authenticated.starts_with("HTTP/1.1 200"));
        assert!(
            authenticated
                .to_lowercase()
                .contains("set-cookie: crusty_session=")
        );
        assert!(
            authenticated
                .to_lowercase()
                .contains("content-security-policy:")
        );
        let cookie = authenticated
            .lines()
            .find(|line| line.to_lowercase().starts_with("set-cookie:"))
            .and_then(|line| line.split_once(':'))
            .and_then(|(_, value)| value.trim().split(';').next())
            .expect("session cookie");

        let body = r#"{"decision":"accepted","reviewed_by":"test","note":null}"#;
        let forbidden = request(
            address,
            &format!(
                "POST /api/findings/unknown/review HTTP/1.1\r\nHost: {address}\r\nCookie: {cookie}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            ),
        )
        .await?;
        assert!(forbidden.starts_with("HTTP/1.1 403"));
        Ok(())
    }

    async fn request(address: &str, request: &str) -> Result<String> {
        let mut stream = tokio::net::TcpStream::connect(address).await?;
        stream.write_all(request.as_bytes()).await?;
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await?;
        Ok(String::from_utf8(response)?)
    }
}

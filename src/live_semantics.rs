//! Position-based live LSP intelligence, independent of the published index.

use crate::{
    Service,
    coordination::{Coordinator, normalize_path},
    rust_analyzer::{self, Launch, Process, Readiness},
    verification::BuildProfile,
};
use anyhow::{Context, Result, ensure};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeSet, fs, path::Path, sync::MutexGuard, time::Duration};

/// Upper bound of one analyzer request once the workspace is loaded.
const QUERY_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SemanticQuery {
    Hover,
    Definition,
    TypeDefinition,
    References,
    Implementations,
    IncomingCalls,
    OutgoingCalls,
    ExpandMacro,
    Rename,
    Assists,
    SignatureHelp,
    DependencyList,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SemanticRequest {
    pub query: SemanticQuery,
    pub file: String,
    /// One-based source line.
    pub line: usize,
    /// Zero-based UTF-16 code units, as specified by LSP. Not a byte offset.
    pub character: usize,
    /// Required for rename; proposed edits are never applied automatically.
    pub new_name: Option<String>,
    #[serde(default)]
    pub profile: BuildProfile,
    pub limit: Option<usize>,
}

pub(crate) fn file_uri(path: &Path) -> String {
    let encoded = path
        .as_os_str()
        .as_encoded_bytes()
        .iter()
        .map(|&b| {
            if b.is_ascii_alphanumeric() || b"/:-_.~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect::<String>();
    format!("file://{encoded}")
}

/// Diagnostic severities, most severe first.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Information,
    #[default]
    Hint,
}

impl DiagnosticSeverity {
    fn label(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Information => "information",
            Self::Hint => "hint",
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticsRequest {
    /// Repository-relative files. They are opened in the analyzer so it
    /// publishes their diagnostics; empty lists every captured file.
    #[serde(default)]
    pub paths: Vec<String>,
    /// Minimum severity to include (default: hint, i.e. everything).
    #[serde(default)]
    pub severity: DiagnosticSeverity,
    /// Maximum diagnostics returned across all files (default 100, at most 500).
    pub limit: Option<usize>,
}

/// How long index-time reference queries wait for a warming analyzer before
/// falling back to static evidence.
const INDEX_READY_WAIT: Duration = Duration::from_secs(5);
/// Bounds of the advisory diagnostics that validation reports.
const VALIDATION_READY_WAIT: Duration = Duration::from_secs(15);
const VALIDATION_PUBLISH_WAIT: Duration = Duration::from_secs(10);
const VALIDATION_MAX_FILES: usize = 32;
const VALIDATION_MAX_DIAGNOSTICS: usize = 50;

impl Service {
    pub(crate) fn semantic_status(&self) -> Result<Value> {
        let mut status = self.ra.status(self.ra_enabled, &self.ra_program);
        status["authority"] =
            json!("Live companion state; no index refresh or implicit analyzer startup.");
        Ok(status)
    }

    /// One line for consultation when the companion is off or unhealthy.
    pub(crate) fn semantic_hint(&self) -> Option<String> {
        if !self.ra_enabled {
            return Some(
                "Live rust-analyzer semantics and diagnostics are off; call semantic.enable to start them for this server (semantic.status)."
                    .into(),
            );
        }
        match self.ra.state(true) {
            state @ ("failed" | "restarting" | "exited") => Some(format!(
                "The rust-analyzer companion is {state}; semantic.status has the error."
            )),
            _ => None,
        }
    }

    /// The analyzer for index-time reference queries: enabled, running the
    /// default profile, and able to answer after a short wait.
    pub(crate) fn analyzer_for_index(&self) -> Option<MutexGuard<'_, Process>> {
        if !self.ra_enabled {
            return None;
        }
        let mut process = self.ra.acquire(&self.execution).ok()?;
        self.ra
            .prepare(
                &mut process,
                &self.root,
                &self.ra_program,
                Some(&Launch::workspace_default()),
            )
            .ok()?;
        self.ra
            .wait_ready(INDEX_READY_WAIT, &self.execution)
            .ok()
            .filter(|readiness| readiness.answers())?;
        Some(process)
    }

    /// Captured analyzer diagnostics. Named files are opened first (without
    /// waiting for a busy analyzer), so their diagnostics are published.
    pub(crate) fn semantic_diagnostics(&self, request: DiagnosticsRequest) -> Result<Value> {
        let limit = request.limit.unwrap_or(100).clamp(1, 500);
        if !self.ra_enabled {
            return Ok(json!({"state":"disabled","complete":false,"files":[],
                "hint":rust_analyzer::ENABLE_HINT}));
        }
        let mut paths = Vec::new();
        for path in &request.paths {
            paths.push(self.root.join(normalize_path(&self.root, path)?));
        }
        let mut opened = Vec::new();
        let mut busy = false;
        if !paths.is_empty() {
            match self.ra.try_process() {
                Some(mut process) => {
                    if self
                        .ra
                        .prepare(&mut process, &self.root, &self.ra_program, None)
                        .is_ok()
                    {
                        opened = self.ra.open_documents(&mut process, &paths);
                    }
                }
                None => busy = true,
            }
        }
        if !opened.is_empty() && self.ra.readiness().answers() {
            self.ra
                .wait_diagnostics(&opened, Duration::from_secs(2), &self.execution)?;
        }
        let mut report =
            self.ra
                .diagnostics_report(&self.root, &paths, request.severity.label(), limit, true);
        report["next"] = json!(match report["state"].as_str() {
            Some("starting" | "warming") =>
                "The analyzer is loading the workspace; call again shortly for complete diagnostics.",
            Some("ready" | "unconfirmed") if busy =>
                "The analyzer was busy with another query, so the named files were not reopened; call again for fresh diagnostics.",
            Some("ready" | "unconfirmed") =>
                "Files labelled pending or unconfirmed settle within seconds; call again to confirm.",
            _ => "Check semantic.status for the analyzer state and last error.",
        });
        Ok(report)
    }

    /// Advisory analyzer diagnostics for the Rust files a validated diff
    /// changed. Never blocks validation; reports why when unavailable.
    pub(crate) fn validation_diagnostics(&self, changed_files: &BTreeSet<String>) -> Result<Value> {
        let advisory = |mut report: Value| {
            report["advisory"] = json!(true);
            report["blocking"] = json!(false);
            report
        };
        if !self.ra_enabled {
            return Ok(advisory(
                json!({"state":"disabled","hint":rust_analyzer::ENABLE_HINT}),
            ));
        }
        let paths = changed_files
            .iter()
            .filter(|path| path.ends_with(".rs"))
            .map(|path| self.root.join(path))
            .filter(|path| path.is_file())
            .take(VALIDATION_MAX_FILES)
            .collect::<Vec<_>>();
        if paths.is_empty() {
            return Ok(advisory(
                json!({"state":"not_applicable","reason":"no existing Rust source in the diff"}),
            ));
        }
        let opened = {
            let mut process = self.ra.acquire(&self.execution)?;
            if let Err(unavailable) =
                self.ra
                    .prepare(&mut process, &self.root, &self.ra_program, None)
            {
                return Ok(advisory(unavailable.report(&self.ra_program)));
            }
            self.ra.open_documents(&mut process, &paths)
        };
        let readiness = self.ra.wait_ready(
            VALIDATION_READY_WAIT.min(rust_analyzer::configured_ready_timeout()),
            &self.execution,
        )?;
        if readiness.answers() {
            self.ra
                .wait_diagnostics(&opened, VALIDATION_PUBLISH_WAIT, &self.execution)?;
        }
        let mut report = self.ra.diagnostics_report(
            &self.root,
            &paths,
            "hint",
            VALIDATION_MAX_DIAGNOSTICS,
            true,
        );
        if !readiness.answers() {
            report["note"] = json!(
                "The analyzer is still loading the workspace; diagnostics may be missing. semantic.diagnostics with these paths returns them once it is ready."
            );
        }
        Ok(advisory(report))
    }

    pub(crate) fn semantic_query(&self, request: SemanticRequest) -> Result<Value> {
        if !self.ra_enabled {
            return Ok(
                json!({"state":"disabled","complete":false,"next":rust_analyzer::ENABLE_HINT}),
            );
        }
        ensure!(
            request.profile.packages.is_empty(),
            "the semantic companion analyzes a workspace; package filtering is unsupported"
        );
        for value in request
            .profile
            .features
            .iter()
            .chain(request.profile.target.iter())
            .chain(request.profile.toolchain.iter())
        {
            ensure!(
                !value.is_empty()
                    && value.len() <= 200
                    && !value.starts_with('-')
                    && value
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"._-/".contains(&b)),
                "invalid semantic profile value"
            );
        }
        ensure!(
            !request.profile.all_features
                || (request.profile.features.is_empty() && !request.profile.no_default_features),
            "incompatible semantic feature selections"
        );
        let path = self.root.join(normalize_path(&self.root, &request.file)?);
        ensure!(
            path.is_file(),
            "semantic query needs an existing source file"
        );
        ensure!(
            fs::metadata(&path)?.len() <= rust_analyzer::MAX_SOURCE_BYTES,
            "source file exceeds semantic capture budget"
        );
        let source = fs::read_to_string(&path)?;
        let line = request.line.checked_sub(1).context("line is one-based")?;
        let text = source
            .split('\n')
            .nth(line)
            .context("line outside source file")?;
        validate_utf16_position(text, request.character)?;
        let coordinator = Coordinator::open(&self.root, self.execution.clone())?;
        let before = coordinator.workspace_fingerprint()?;
        // One canonical profile: source edits are synchronised into the
        // running analyzer; only version, options, toolchain and Cargo
        // configuration changes restart it.
        let launch = Launch::for_profile(&request.profile);
        let prepare = |process: &mut Process| {
            self.ra
                .prepare(process, &self.root, &self.ra_program, Some(&launch))
        };
        // Wait for a warming analyzer without holding the process, so other
        // callers are not queued behind this query.
        if let Err(unavailable) = prepare(&mut *self.ra.acquire(&self.execution)?) {
            return Ok(unavailable.report(&self.ra_program));
        }
        let ready_timeout = rust_analyzer::configured_ready_timeout();
        self.ra.wait_ready(ready_timeout, &self.execution)?;
        let mut process = self.ra.acquire(&self.execution)?;
        let profile_digest = match prepare(&mut process) {
            Ok(digest) => digest,
            Err(unavailable) => return Ok(unavailable.report(&self.ra_program)),
        };
        let version = rust_analyzer::rust_analyzer_version(&self.ra_program);
        let client = process.client()?;
        let uri = file_uri(&path);
        // Synchronization errors are visible rather than silently retaining text.
        client.sync_document(&path, &source)?;
        let readiness = self.ra.readiness();
        match readiness {
            Readiness::Starting | Readiness::Warming => {
                return Ok(
                    json!({"state":"warming","complete":false,"query":request.query,
                    "waited_seconds":ready_timeout.as_secs(),"server_status":self.ra.server_status(),
                    "profile_digest":profile_digest,"version":version,
                    "next":format!("rust-analyzer is still loading the workspace (cache priming). Retry shortly, or raise {} for longer waits. Exact source search remains available.", rust_analyzer::READY_TIMEOUT_ENV)}),
                );
            }
            Readiness::Exited => {
                return Ok(
                    json!({"state":"failed","complete":false,"error":"rust-analyzer exited while loading the workspace; the next query restarts it",
                    "profile_digest":profile_digest,"version":version}),
                );
            }
            Readiness::Ready | Readiness::Unconfirmed => {}
        }
        let position = json!({"textDocument":{"uri":uri},"position":{"line":line,"character":request.character}});
        let (method, capability) = match request.query {
            SemanticQuery::Hover => ("textDocument/hover", Some("hoverProvider")),
            SemanticQuery::Definition => ("textDocument/definition", Some("definitionProvider")),
            SemanticQuery::TypeDefinition => (
                "textDocument/typeDefinition",
                Some("typeDefinitionProvider"),
            ),
            SemanticQuery::References => ("textDocument/references", Some("referencesProvider")),
            SemanticQuery::Implementations => (
                "textDocument/implementation",
                Some("implementationProvider"),
            ),
            SemanticQuery::IncomingCalls | SemanticQuery::OutgoingCalls => (
                "textDocument/prepareCallHierarchy",
                Some("callHierarchyProvider"),
            ),
            SemanticQuery::ExpandMacro => ("rust-analyzer/expandMacro", None),
            SemanticQuery::Rename => ("textDocument/rename", Some("renameProvider")),
            SemanticQuery::Assists => ("textDocument/codeAction", Some("codeActionProvider")),
            SemanticQuery::SignatureHelp => {
                ("textDocument/signatureHelp", Some("signatureHelpProvider"))
            }
            SemanticQuery::DependencyList => ("rust-analyzer/fetchDependencyList", None),
        };
        if capability.is_some_and(|key| {
            client
                .capabilities
                .get(key)
                .is_none_or(|v| v == &Value::Null || v == &json!(false))
        }) {
            return Ok(
                json!({"state":"unsupported","complete":false,"query":request.query,"capabilities":client.capabilities,"version":version}),
            );
        }
        let mut params = position.clone();
        match request.query {
            SemanticQuery::References => {
                params["context"] = json!({"includeDeclaration":true});
            }
            SemanticQuery::Rename => {
                let name = request
                    .new_name
                    .as_deref()
                    .context("rename requires new_name")?;
                ensure!(
                    !name.is_empty() && name.len() <= 200 && !name.contains('\0'),
                    "invalid rename name"
                );
                params["newName"] = json!(name);
            }
            SemanticQuery::Assists => {
                let diagnostics = self.ra.lsp_diagnostics_at(&path, line);
                params = json!({"textDocument":{"uri":uri},"range":{"start":position["position"],"end":position["position"]},"context":{"diagnostics":diagnostics}});
            }
            SemanticQuery::DependencyList => params = json!({}),
            _ => {}
        }
        let mut result = client.request(method, params, QUERY_TIMEOUT, &self.execution)?;
        let mut hierarchy_truncated = false;
        if matches!(
            request.query,
            SemanticQuery::IncomingCalls | SemanticQuery::OutgoingCalls
        ) && result.get("protocol_error").is_none()
        {
            let method = if matches!(request.query, SemanticQuery::IncomingCalls) {
                "callHierarchy/incomingCalls"
            } else {
                "callHierarchy/outgoingCalls"
            };
            let mut calls = Vec::new();
            hierarchy_truncated = result.as_array().is_some_and(|items| items.len() > 20);
            let mut failure = None;
            for item in result.as_array().into_iter().flatten().take(20) {
                let response =
                    client.request(method, json!({"item":item}), QUERY_TIMEOUT, &self.execution)?;
                if let Some(error) = response.get("protocol_error") {
                    failure = Some(error.clone());
                    break;
                }
                if let Some(items) = response.as_array() {
                    calls.extend(items.iter().cloned());
                }
            }
            result = match failure {
                Some(error) => json!({"protocol_error":error,"partial":calls}),
                None => json!(calls),
            };
        }
        let unchanged = before == coordinator.workspace_fingerprint()?;
        let server_status = self.ra.server_status();
        let readiness = readiness == Readiness::Ready && server_status["health"] == "ok";
        let limit = request.limit.unwrap_or(100).clamp(1, 500);
        let total = result.as_array().map(Vec::len);
        let truncated = hierarchy_truncated || total.is_some_and(|count| count > limit);
        if let Some(array) = result.as_array_mut() {
            array.truncate(limit);
        }
        let protocol_error = result.get("protocol_error");
        let state = if !unchanged {
            "stale"
        } else if let Some(error) = protocol_error {
            if error["code"] == -32601 {
                "unsupported"
            } else {
                "failed"
            }
        } else if !readiness {
            "loading_or_unconfirmed"
        } else if result.is_null() || result.as_array().is_some_and(Vec::is_empty) {
            "empty"
        } else {
            "ready"
        };
        Ok(
            json!({"state":state,"query":request.query,"result":result,"total":total,"truncated":truncated,
            "complete":unchanged && readiness && !truncated && protocol_error.is_none(),"workspace_digest":before,"source_digest":rust_analyzer::content_hash(source.as_bytes()),
            "profile":request.profile,"profile_digest":profile_digest,"version":version,"server_status":server_status,
            "position_encoding":"utf-16","authority":"Live rust-analyzer LSP response; locations may refer to external dependency source. Compiler/runtime evidence remains authoritative.",
            "edits_applied":false,"edit_contract":"Rename and assist edits are proposals. Claim all affected paths, prepare the change, check document versions/source fingerprints, apply the complete edit, and validate it. Commands are not executed."}),
        )
    }
}

fn validate_utf16_position(line: &str, position: usize) -> Result<()> {
    let mut offset = 0;
    if position == 0 {
        return Ok(());
    }
    for character in line.trim_end_matches('\r').chars() {
        offset += character.len_utf16();
        if offset == position {
            return Ok(());
        }
        ensure!(offset < position, "UTF-16 position splits a surrogate pair");
    }
    anyhow::bail!("character outside source line")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unicode_positions_and_file_uris_follow_lsp_encoding() {
        assert!(validate_utf16_position("a🦀b", 1).is_ok());
        assert!(validate_utf16_position("a🦀b", 2).is_err());
        assert!(validate_utf16_position("a🦀b", 3).is_ok());
        assert!(validate_utf16_position("a🦀b", 5).is_err());
        assert_eq!(
            file_uri(Path::new("/tmp/Rust 🦀/a.rs")),
            "file:///tmp/Rust%20%F0%9F%A6%80/a.rs"
        );
    }

    #[test]
    fn real_analyzer_answers_before_publication_and_reflects_edits_without_restart() {
        let program =
            rust_analyzer::rust_analyzer_program(Path::new(env!("CARGO_MANIFEST_DIR")), None);
        if rust_analyzer::rust_analyzer_version(&program).is_none() {
            eprintln!("real rust-analyzer unavailable; live integration fixture skipped");
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("Rust 🦀 fixture");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname='live_fixture'\nversion='0.1.0'\nedition='2024'\n",
        )
        .unwrap();
        let source = "pub struct Thing { pub value: u32 }\nimpl Thing { pub fn make() -> Self { Self { value: 1 } } }\npub fn caller() -> Thing { Thing::make() }\nmacro_rules! answer { () => { 42u32 } }\npub fn macro_user() -> u32 { answer!() }\npub mod other;\npub fn use_helper() { let _ = other::helper(); }\n";
        let helper = "pub fn helper() -> u32 { 1 }\n";
        fs::write(root.join("src/lib.rs"), source).unwrap();
        fs::write(root.join("src/other.rs"), helper).unwrap();
        let mut service = Service::open(&root).unwrap();
        service.ra_enabled = true;
        service.ra_program = program;
        let published = |service: &Service| {
            service
                .db
                .query_row(
                    "SELECT COUNT(*) FROM index_generations WHERE status='published'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap()
        };
        assert_eq!(published(&service), 0);
        let query = |kind, line, character| SemanticRequest {
            query: kind,
            file: "src/lib.rs".into(),
            line,
            character,
            new_name: None,
            profile: BuildProfile::default(),
            limit: None,
        };
        let make = source.lines().nth(2).unwrap().find("make").unwrap();
        let definition = service
            .semantic_query(query(SemanticQuery::Definition, 3, make))
            .unwrap();
        assert_eq!(definition["state"], "ready", "{definition}");
        assert_eq!(definition["complete"], true, "{definition}");
        assert!(!definition["result"].as_array().unwrap().is_empty());
        let mut rename = query(SemanticQuery::Rename, 3, make);
        rename.new_name = Some("build".into());
        let rename = service.semantic_query(rename).unwrap();
        assert_eq!(rename["edits_applied"], false);
        assert!(rename["result"].to_string().contains("build"));
        assert_eq!(fs::read_to_string(root.join("src/lib.rs")).unwrap(), source);
        let expand = service
            .semantic_query(query(
                SemanticQuery::ExpandMacro,
                5,
                source.lines().nth(4).unwrap().find("answer!").unwrap(),
            ))
            .unwrap();
        assert_eq!(expand["state"], "ready", "{expand}");
        assert!(expand["result"].to_string().contains("42u32"));
        let helper_call = source
            .lines()
            .nth(6)
            .unwrap()
            .find("other::helper")
            .unwrap()
            + 7;
        let before = service
            .semantic_query(query(SemanticQuery::Hover, 7, helper_call))
            .unwrap();
        assert!(before["result"].to_string().contains("u32"), "{before}");
        // Edits to the queried file and to a file that was never opened reach
        // the running analyzer without restarting it.
        let edited = source.replace("value: u32", "value: u64");
        fs::write(root.join("src/lib.rs"), &edited).unwrap();
        fs::write(root.join("src/other.rs"), helper.replace("u32", "u64")).unwrap();
        let hover = service
            .semantic_query(query(
                SemanticQuery::Hover,
                1,
                source.lines().next().unwrap().find("value").unwrap(),
            ))
            .unwrap();
        assert_eq!(hover["state"], "ready", "{hover}");
        assert!(hover["result"].to_string().contains("u64"));
        let after = service
            .semantic_query(query(SemanticQuery::Hover, 7, helper_call))
            .unwrap();
        assert!(after["result"].to_string().contains("u64"), "{after}");
        assert_eq!(definition["profile_digest"], hover["profile_digest"]);
        assert_eq!(definition["profile_digest"], after["profile_digest"]);
        let status = service.semantic_status().unwrap();
        assert_eq!(status["starts"], 1, "{status}");
        assert_eq!(status["state"], "ready", "{status}");
        // Diagnostics are captured, versioned, and labelled fresh.
        fs::write(
            root.join("src/lib.rs"),
            format!("{edited}pub fn broken() {{ let = 1; }}\n"),
        )
        .unwrap();
        let request = DiagnosticsRequest {
            paths: vec!["src/lib.rs".into()],
            severity: DiagnosticSeverity::Error,
            limit: None,
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        let diagnostics = loop {
            let report = service.semantic_diagnostics(request.clone()).unwrap();
            let file = &report["files"][0];
            if file["freshness"] == "fresh" && file["count"].as_u64().unwrap_or(0) > 0 {
                break report;
            }
            assert!(std::time::Instant::now() < deadline, "{report}");
            std::thread::sleep(Duration::from_millis(200));
        };
        let first = &diagnostics["files"][0]["diagnostics"][0];
        assert_eq!(first["severity"], "error", "{diagnostics}");
        assert_eq!(first["source"], "rust-analyzer", "{diagnostics}");
        assert_eq!(first["line"], 8, "{diagnostics}");
        assert_eq!(service.semantic_status().unwrap()["starts"], 1);
        assert_eq!(published(&service), 0);
    }
}

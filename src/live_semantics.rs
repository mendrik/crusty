//! Position-based live LSP intelligence, independent of the published index.

use crate::{
    RustAnalyzerClient, Service, answer_lsp_request_configured,
    coordination::{Coordinator, normalize_path},
    execution::ExecutionControl,
    verification::BuildProfile,
    write_lsp,
};
use anyhow::{Context, Result, ensure};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    process::Command,
    sync::TryLockError,
    time::{Duration, Instant},
};

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

impl RustAnalyzerClient {
    fn synchronize_live(&mut self, uri: &str, source: &str) -> Result<()> {
        let hash = blake3::hash(source.as_bytes());
        if let Some((version, previous)) = self.opened_versions.get(uri) {
            if *previous == hash {
                return Ok(());
            }
            let version = version + 1;
            write_lsp(
                &mut self.stdin,
                &json!({"jsonrpc":"2.0","method":"textDocument/didChange","params":{"textDocument":{"uri":uri,"version":version},"contentChanges":[{"text":source}]}}),
            )?;
            self.opened_versions.insert(uri.into(), (version, hash));
        } else {
            write_lsp(
                &mut self.stdin,
                &json!({"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":uri,"languageId":"rust","version":1,"text":source}}}),
            )?;
            self.opened_versions.insert(uri.into(), (1, hash));
        }
        Ok(())
    }
    fn request_live(
        &mut self,
        method: &str,
        params: Value,
        control: &ExecutionControl,
    ) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        write_lsp(
            &mut self.stdin,
            &json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}),
        )?;
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Err(error) = control.check() {
                let _ = write_lsp(
                    &mut self.stdin,
                    &json!({"jsonrpc":"2.0","method":"$/cancelRequest","params":{"id":id}}),
                );
                return Err(error.into());
            }
            if Instant::now() >= deadline {
                let _ = write_lsp(
                    &mut self.stdin,
                    &json!({"jsonrpc":"2.0","method":"$/cancelRequest","params":{"id":id}}),
                );
                anyhow::bail!("rust-analyzer query timed out; empty results were not inferred");
            }
            let message = match self.responses.recv_timeout(Duration::from_millis(100)) {
                Ok(message) => message,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(error) => return Err(error).context("rust-analyzer disconnected"),
            };
            if message["method"] == "experimental/serverStatus" {
                self.server_status = message["params"].clone();
                continue;
            }
            if answer_lsp_request_configured(&mut self.stdin, &message, &self.configuration)? {
                continue;
            }
            if message["id"].as_u64() != Some(id) {
                continue;
            }
            if let Some(error) = message.get("error") {
                return Ok(json!({"protocol_error":error}));
            }
            return Ok(message["result"].clone());
        }
    }

    fn wait_ready(&mut self, control: &ExecutionControl) -> Result<()> {
        // Extensions vary by installed version. A server without this extension
        // can answer queries, but cannot establish complete workspace readiness.
        let deadline = Instant::now() + Duration::from_secs(20);
        while self.server_status["quiescent"] != true && Instant::now() < deadline {
            control.check()?;
            match self.responses.recv_timeout(Duration::from_millis(100)) {
                Ok(message) => {
                    if message["method"] == "experimental/serverStatus" {
                        self.server_status = message["params"].clone();
                    } else {
                        answer_lsp_request_configured(
                            &mut self.stdin,
                            &message,
                            &self.configuration,
                        )?;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(error) => {
                    return Err(error).context("rust-analyzer disconnected during workspace load");
                }
            }
        }
        Ok(())
    }
}

impl Service {
    pub(crate) fn semantic_status(&self) -> Result<Value> {
        let backend = self
            .ra
            .lock()
            .map_err(|_| anyhow::anyhow!("semantic backend lock poisoned"))?;
        Ok(
            json!({"enabled":self.ra_enabled,"program":self.ra_program,"running":backend.client.is_some(),
            "start_error":backend.start_error,"profile_digest":backend.live_profile,
            "capabilities":backend.client.as_ref().map(|c|&c.capabilities),"server_status":backend.client.as_ref().map(|c|&c.server_status),
            "authority":"Live companion state; no index refresh or implicit analyzer startup."}),
        )
    }

    pub(crate) fn semantic_query(&self, request: SemanticRequest) -> Result<Value> {
        if !self.ra_enabled {
            return Ok(
                json!({"state":"disabled","complete":false,"next":"Enable RUST_REPO_INTELLIGENCE_ENABLE_RUST_ANALYZER=1 for the semantic companion. Exact source search remains available."}),
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
            fs::metadata(&path)?.len() <= 4_000_000,
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
        let mut options = crate::rust_analyzer_initialization_options();
        options["cargo"]["features"] = if request.profile.all_features {
            json!("all")
        } else {
            json!(request.profile.features)
        };
        options["cargo"]["noDefaultFeatures"] = json!(request.profile.no_default_features);
        options["cargo"]["target"] = json!(request.profile.target);
        let program_version = self
            .execution
            .output(Command::new(&self.ra_program).arg("--version"))?;
        ensure!(
            program_version.status.success(),
            "unable to identify rust-analyzer version"
        );
        let version = String::from_utf8(program_version.stdout)?.trim().to_owned();
        // Source/config changes restart the companion, avoiding stale module
        // graphs, changed manifests and dependency configurations across queries.
        let profile_digest = format!(
            "b3:{}",
            blake3::hash(
                json!([before, options, request.profile.toolchain, version])
                    .to_string()
                    .as_bytes()
            )
            .to_hex()
        );
        let mut backend = loop {
            self.execution.check()?;
            match self.ra.try_lock() {
                Ok(lock) => break lock,
                Err(TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(10)),
                Err(TryLockError::Poisoned(_)) => anyhow::bail!("semantic backend lock poisoned"),
            }
        };
        let running = match backend.client.as_mut() {
            Some(client) => client._child.is_running()?,
            None => false,
        };
        if backend.live_profile.as_deref() != Some(&profile_digest) || !running {
            backend.client = None;
            backend.start_attempted = true;
            backend.live_profile = Some(profile_digest.clone());
            match RustAnalyzerClient::start_configured(
                &self.root,
                &self.ra_program,
                options,
                request.profile.toolchain.as_deref(),
            ) {
                Ok(client) => {
                    backend.client = Some(client);
                    backend.start_error = None;
                }
                Err(error) => {
                    backend.start_error = Some(format!("{error:#}"));
                    return Ok(
                        json!({"state":"failed","complete":false,"error":backend.start_error,"program":self.ra_program}),
                    );
                }
            }
        }
        let client = backend
            .client
            .as_mut()
            .context("semantic companion missing")?;
        let uri = file_uri(&path);
        // Synchronization errors are visible rather than silently retaining text.
        client.synchronize_live(&uri, &source)?;
        client.wait_ready(&self.execution)?;
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
                params = json!({"textDocument":{"uri":uri},"range":{"start":position["position"],"end":position["position"]},"context":{"diagnostics":[]}});
            }
            SemanticQuery::DependencyList => params = json!({}),
            _ => {}
        }
        let mut result = client.request_live(method, params, &self.execution)?;
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
                    client.request_live(method, json!({"item":item}), &self.execution)?;
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
        let readiness =
            client.server_status["quiescent"] == true && client.server_status["health"] == "ok";
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
            "complete":unchanged && readiness && !truncated && protocol_error.is_none(),"workspace_digest":before,"source_digest":format!("b3:{}",blake3::hash(source.as_bytes()).to_hex()),
            "profile":request.profile,"profile_digest":profile_digest,"version":version,"server_status":client.server_status,
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
    fn real_analyzer_answers_before_publication_and_restarts_after_source_edits() {
        let program = crate::rust_analyzer_program(None);
        if !Command::new(&program)
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success())
        {
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
        let source = "pub struct Thing { pub value: u32 }\nimpl Thing { pub fn make() -> Self { Self { value: 1 } } }\npub fn caller() -> Thing { Thing::make() }\nmacro_rules! answer { () => { 42u32 } }\npub fn macro_user() -> u32 { answer!() }\n";
        fs::write(root.join("src/lib.rs"), source).unwrap();
        let mut service = Service::open(&root).unwrap();
        service.ra_enabled = true;
        service.ra_program = program;
        assert_eq!(
            service
                .db
                .query_row(
                    "SELECT COUNT(*) FROM index_generations WHERE status='published'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
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
        fs::write(
            root.join("src/lib.rs"),
            source.replace("value: u32", "value: u64"),
        )
        .unwrap();
        let hover = service
            .semantic_query(query(
                SemanticQuery::Hover,
                1,
                source.lines().next().unwrap().find("value").unwrap(),
            ))
            .unwrap();
        assert_eq!(hover["state"], "ready", "{hover}");
        assert!(hover["result"].to_string().contains("u64"));
        assert_ne!(definition["profile_digest"], hover["profile_digest"]);
        assert_eq!(
            service
                .db
                .query_row(
                    "SELECT COUNT(*) FROM index_generations WHERE status='published'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
    }
}

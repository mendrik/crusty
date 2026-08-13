//! Repository indexing and change-impact services used by the MCP binary.
//! Semantic facts returned by a static scan are explicitly marked as such.

use anyhow::{Context, Result, bail};
use chrono::Utc;
use regex::Regex;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        Mutex,
        mpsc::{self, Receiver},
    },
    thread,
    time::{Duration, Instant},
};
use walkdir::WalkDir;

const INDEX_DIRECTORY: &str = ".rust-repo-intelligence";
const MAX_SLICE_BYTES: usize = 12_000;
const SCHEMA_VERSION: &str = "4";
const RUST_ANALYZER_THREADS: u64 = 1;

#[derive(Debug, Clone, Serialize)]
pub struct Revision {
    pub head: Option<String>,
    pub dirty: bool,
    pub workspace_digest: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceSlice {
    pub symbol: String,
    pub file: String,
    pub range: [usize; 2],
    pub content_hash: String,
    pub reason: String,
    pub semantic_relationship: String,
    pub provenance: String,
    pub confidence: f64,
    pub source: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Node {
    pub id: i64,
    pub kind: String,
    pub canonical_name: String,
    pub crate_name: Option<String>,
    pub file: String,
    pub start_line: usize,
    pub end_line: usize,
    pub visibility: String,
    pub content_hash: String,
}

type LifecycleRecord = (
    i64,
    Option<i64>,
    String,
    String,
    String,
    f64,
    Node,
    Option<String>,
);

#[derive(Debug, Deserialize)]
pub struct RecordDecision {
    pub title: String,
    #[serde(default = "accepted")]
    pub status: String,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub applies_to: Vec<String>,
    #[serde(default)]
    pub consequences: Vec<String>,
    pub supersedes: Option<String>,
    #[serde(default)]
    pub materialize: bool,
}

/// Editable, evidence-backed repository work. Automatically discovered work is
/// always `proposed`; callers must explicitly accept it before it becomes a plan.
#[derive(Debug, Deserialize)]
pub struct WorkItemInput {
    pub title: String,
    #[serde(default = "proposed")]
    pub status: String,
    #[serde(default = "normal")]
    pub priority: String,
    #[serde(default = "cleanup")]
    pub kind: String,
    #[serde(default)]
    pub scope: Vec<String>,
    #[serde(default)]
    pub evidence: Vec<String>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub blocked_by: Vec<String>,
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,
    #[serde(default)]
    pub verification: Vec<String>,
}

fn proposed() -> String {
    "proposed".into()
}
fn normal() -> String {
    "normal".into()
}
fn cleanup() -> String {
    "cleanup".into()
}

fn accepted() -> String {
    "accepted".into()
}

pub struct RustAnalyzerClient {
    _child: Child,
    stdin: ChildStdin,
    responses: Receiver<Value>,
    next_id: u64,
}

impl RustAnalyzerClient {
    fn start(workspace: &Path) -> Result<Self> {
        let mut child = Command::new("rust-analyzer")
            .current_dir(workspace)
            // Keep the analyzer's Cargo subprocesses from multiplying the
            // worker limit below.  This is intentionally a conservative
            // default for a background MCP service.
            .env("CARGO_BUILD_JOBS", RUST_ANALYZER_THREADS.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("starting rust-analyzer")?;
        let stdout = child
            .stdout
            .take()
            .context("opening rust-analyzer stdout")?;
        let (sender, responses) = mpsc::channel();
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            while let Some(message) = read_lsp(&mut reader) {
                if sender.send(message).is_err() {
                    break;
                }
            }
        });
        let mut stdin = child.stdin.take().context("opening rust-analyzer stdin")?;
        let request = json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"processId": null, "rootUri": format!("file://{}", workspace.display()),
                "capabilities": {},
                "initializationOptions": rust_analyzer_initialization_options(),
                "workspaceFolders": [{"uri": format!("file://{}", workspace.display()), "name": "workspace"}]}
        });
        write_lsp(&mut stdin, &request)?;
        write_lsp(
            &mut stdin,
            &json!({"jsonrpc":"2.0","method":"initialized","params":{}}),
        )?;
        Ok(Self {
            _child: child,
            stdin,
            responses,
            next_id: 2,
        })
    }

    fn did_change(&mut self, uri: &str, text: &str) {
        let _ = write_lsp(
            &mut self.stdin,
            &json!({"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":uri,"languageId":"rust","version":1,"text":text}}}),
        );
    }

    fn locations(
        &mut self,
        method: &str,
        uri: &str,
        line: usize,
        character: usize,
    ) -> Option<Vec<Value>> {
        let id = self.next_id;
        self.next_id += 1;
        write_lsp(&mut self.stdin, &json!({
            "jsonrpc":"2.0", "id":id, "method":method,
            "params":{"textDocument":{"uri":uri},"position":{"line":line,"character":character},"context":{"includeDeclaration":true}}
        })).ok()?;
        let deadline = Instant::now() + Duration::from_secs(3);
        while let Some(wait) = deadline.checked_duration_since(Instant::now()) {
            let response = self.responses.recv_timeout(wait).ok()?;
            if response.get("id").and_then(Value::as_u64) == Some(id) {
                return response.get("result").and_then(Value::as_array).cloned();
            }
        }
        None
    }

    fn references(&mut self, uri: &str, line: usize, character: usize) -> Option<Vec<Value>> {
        self.locations("textDocument/references", uri, line, character)
    }

    fn implementations(&mut self, uri: &str, line: usize, character: usize) -> Option<Vec<Value>> {
        self.locations("textDocument/implementation", uri, line, character)
    }
}

fn rust_analyzer_initialization_options() -> Value {
    json!({
        // This process is not an editor and does not need diagnostics after
        // every didOpen notification.  Semantic reference requests still work.
        "checkOnSave": false,
        "cachePriming": {
            "enable": false,
            "numThreads": RUST_ANALYZER_THREADS,
        },
        "numThreads": RUST_ANALYZER_THREADS,
        "cargo": {
            "extraEnv": {
                "CARGO_BUILD_JOBS": RUST_ANALYZER_THREADS.to_string(),
            },
        },
        "check": {
            "extraEnv": {
                "CARGO_BUILD_JOBS": RUST_ANALYZER_THREADS.to_string(),
            },
        },
    })
}

impl Drop for RustAnalyzerClient {
    fn drop(&mut self) {
        let _ = self._child.kill();
        let _ = self._child.wait();
    }
}

fn write_lsp(out: &mut ChildStdin, value: &Value) -> Result<()> {
    let body = serde_json::to_vec(value)?;
    write!(out, "Content-Length: {}\r\n\r\n", body.len())?;
    out.write_all(&body)?;
    out.flush()?;
    Ok(())
}

fn read_lsp(reader: &mut impl BufRead) -> Option<Value> {
    let mut content_length = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        if let Some(value) = trimmed.strip_prefix("Content-Length:") {
            content_length = value.trim().parse::<usize>().ok();
        }
    }
    let mut body = vec![0; content_length?];
    reader.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}

pub struct Service {
    root: PathBuf,
    db: Connection,
    ra: Option<Mutex<RustAnalyzerClient>>,
}

impl Service {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = fs::canonicalize(root.into()).context("resolving workspace path")?;
        let index_dir = root.join(INDEX_DIRECTORY);
        fs::create_dir_all(&index_dir)?;
        let db = Connection::open(index_dir.join("index.sqlite3"))?;
        initialize_schema(&db)?;
        let ra = RustAnalyzerClient::start(&root).ok().map(Mutex::new);
        // MCP clients impose a short initialization deadline.  Indexing a large
        // workspace here makes the server appear unavailable, so cache creation
        // is deliberately lazy: the first tool/resource query refreshes it.
        Ok(Self { root, db, ra })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn revision(&self) -> Revision {
        let head = command_text(&self.root, &["rev-parse", "HEAD"]);
        let dirty = Command::new("git")
            .args(["diff", "--quiet"])
            .current_dir(&self.root)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| !s.success())
            .unwrap_or(false);
        let mut hasher = blake3::Hasher::new();
        for (path, (_, hash)) in index_inputs(&self.root) {
            hasher.update(path.as_bytes());
            hasher.update(hash.as_bytes());
        }
        Revision {
            head,
            dirty,
            workspace_digest: format!("b3:{}", hasher.finalize().to_hex()),
        }
    }

    pub fn reindex(&mut self) -> Result<()> {
        let revision = self.revision();
        self.db
            .execute("DELETE FROM edges WHERE provenance = 'StaticIndex'", [])?;
        self.db
            .execute("DELETE FROM edges WHERE provenance = 'RustAnalyzer'", [])?;
        self.db.execute("DELETE FROM nodes", [])?;
        self.db.execute("DELETE FROM packages", [])?;
        self.db.execute("DELETE FROM package_dependencies", [])?;
        self.db.execute("DELETE FROM package_targets", [])?;
        self.db.execute("DELETE FROM package_features", [])?;
        self.db.execute("DELETE FROM commits", [])?;
        self.db.execute("DELETE FROM commit_files", [])?;
        self.db.execute("DELETE FROM co_changes", [])?;
        self.db.execute("DELETE FROM documents", [])?;
        self.db.execute("DELETE FROM use_case_nodes", [])?;
        self.db.execute("DELETE FROM use_cases", [])?;
        let packages = cargo_packages(&self.root);
        for package in &packages {
            self.db.execute("INSERT OR REPLACE INTO packages(name, manifest_path, metadata, revision) VALUES (?1, ?2, ?3, ?4)", params![package.0, package.1, package.2, revision.workspace_digest])?;
        }
        for (source, target, context) in cargo_dependency_edges(&self.root) {
            self.db.execute(
                "INSERT OR REPLACE INTO package_dependencies(source, target, context_json, revision) VALUES (?1, ?2, ?3, ?4)",
                params![source, target, context, revision.workspace_digest],
            )?;
        }
        self.index_cargo_matrix(&revision.workspace_digest)?;
        let nodes = self.index_source_files(&rust_files(&self.root), &packages)?;
        self.index_static_references(&nodes, &revision.workspace_digest)?;
        self.index_semantic_edges(&nodes, &revision.workspace_digest);
        self.index_lifecycle_evidence(&nodes, &revision.workspace_digest)?;
        self.relink_decision_targets()?;
        self.index_documents_and_use_cases(&nodes, &revision.workspace_digest)?;
        self.index_proposed_work(&revision.workspace_digest)?;
        self.index_git(&revision.workspace_digest)?;
        self.record_index_state(&revision, &index_inputs(&self.root))?;
        Ok(())
    }

    /// Refresh only the portions invalidated by changed inputs. Cargo and toolchain
    /// inputs deliberately trigger a complete semantic refresh; documentation alone
    /// never invalidates source symbols or graph edges.
    pub fn refresh_if_stale(&mut self) -> Result<()> {
        let inputs = index_inputs(&self.root);
        let previous: BTreeMap<String, (String, String)> = self
            .db
            .prepare("SELECT path, kind, content_hash FROM input_state")?
            .query_map([], |row| Ok((row.get(0)?, (row.get(1)?, row.get(2)?))))?
            .collect::<rusqlite::Result<_>>()?;
        if previous.is_empty() {
            return self.reindex();
        }
        let revision = self.revision();
        let indexed_head: Option<String> = self
            .db
            .query_row(
                "SELECT value FROM metadata WHERE key='indexed_head'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .filter(|head| !head.is_empty());
        let head_changed = indexed_head != revision.head;
        let changed: Vec<(String, String)> = inputs
            .iter()
            .filter(|(path, (kind, hash))| {
                previous.get(*path) != Some(&(kind.clone(), hash.clone()))
            })
            .map(|(path, (kind, _))| (path.clone(), kind.clone()))
            .chain(
                previous
                    .iter()
                    .filter(|(path, _)| !inputs.contains_key(*path))
                    .map(|(path, (kind, _))| (path.clone(), kind.clone())),
            )
            .collect();
        if changed.is_empty() {
            if head_changed {
                self.refresh_git(&revision.workspace_digest)?;
                self.record_index_state(&revision, &inputs)?;
            }
            return Ok(());
        }
        if changed.iter().any(|(_, kind)| kind == "cargo") {
            return self.reindex();
        }
        let source_paths: Vec<PathBuf> = changed
            .iter()
            .filter(|(_, kind)| kind == "source")
            .map(|(path, _)| self.root.join(path))
            .collect();
        if !source_paths.is_empty() {
            for path in &source_paths {
                self.db.execute(
                    "DELETE FROM nodes WHERE file = ?1",
                    [relative(&self.root, path)],
                )?;
            }
            let packages = cargo_packages(&self.root);
            let existing: Vec<PathBuf> = source_paths
                .into_iter()
                .filter(|path| path.exists())
                .collect();
            self.index_source_files(&existing, &packages)?;
            self.db
                .execute("DELETE FROM edges WHERE provenance = 'StaticIndex'", [])?;
            self.db
                .execute("DELETE FROM edges WHERE provenance = 'RustAnalyzer'", [])?;
            let nodes = self.all_nodes()?;
            self.index_static_references(&nodes, &revision.workspace_digest)?;
            self.index_semantic_edges(&nodes, &revision.workspace_digest);
            self.index_lifecycle_evidence(&nodes, &revision.workspace_digest)?;
            self.relink_decision_targets()?;
            self.refresh_documents_and_use_cases(&nodes, &revision.workspace_digest)?;
            self.index_proposed_work(&revision.workspace_digest)?;
        } else {
            let nodes = self.all_nodes()?;
            self.refresh_documents_and_use_cases(&nodes, &revision.workspace_digest)?;
            self.index_proposed_work(&revision.workspace_digest)?;
        }
        if head_changed {
            self.refresh_git(&revision.workspace_digest)?;
        }
        self.record_index_state(&revision, &inputs)
    }

    fn record_index_state(
        &self,
        revision: &Revision,
        inputs: &BTreeMap<String, (String, String)>,
    ) -> Result<()> {
        self.db.execute(
            "INSERT OR REPLACE INTO metadata(key, value) VALUES ('revision', ?1)",
            [serde_json::to_string(revision)?],
        )?;
        self.db.execute(
            "INSERT OR REPLACE INTO metadata(key, value) VALUES ('indexed_head', ?1)",
            [revision.head.clone().unwrap_or_default()],
        )?;
        self.db.execute(
            "INSERT OR REPLACE INTO metadata(key, value) VALUES ('indexed_at', ?1)",
            [Utc::now().to_rfc3339()],
        )?;
        self.db.execute("DELETE FROM input_state", [])?;
        for (path, (kind, hash)) in inputs {
            self.db.execute(
                "INSERT INTO input_state(path, kind, content_hash, revision) VALUES (?1, ?2, ?3, ?4)",
                params![path, kind, hash, revision.workspace_digest],
            )?;
        }
        Ok(())
    }

    fn all_nodes(&self) -> Result<Vec<Node>> {
        let mut statement = self.db.prepare(
            "SELECT id,kind,canonical_name,crate_name,file,start_line,end_line,visibility,content_hash FROM nodes ORDER BY id",
        )?;
        Ok(statement
            .query_map([], node_from_row)?
            .filter_map(Result::ok)
            .collect())
    }

    fn refresh_documents_and_use_cases(&self, nodes: &[Node], revision: &str) -> Result<()> {
        self.db.execute("DELETE FROM documents", [])?;
        self.db.execute("DELETE FROM use_case_nodes", [])?;
        self.db.execute("DELETE FROM use_cases", [])?;
        self.index_documents_and_use_cases(nodes, revision)
    }

    fn refresh_git(&self, revision: &str) -> Result<()> {
        self.db.execute("DELETE FROM commits", [])?;
        self.db.execute("DELETE FROM commit_files", [])?;
        self.db.execute("DELETE FROM co_changes", [])?;
        self.index_git(revision)
    }

    fn relink_decision_targets(&self) -> Result<()> {
        let targets: Vec<(String, String)> = self
            .db
            .prepare("SELECT decision_id, target_ref FROM decision_targets")?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for (decision_id, target_ref) in targets {
            let node_id = self
                .search_nodes(&terms(&target_ref), 1)?
                .into_iter()
                .next()
                .map(|node| node.id);
            self.db.execute(
                "UPDATE decision_targets SET node_id=?1 WHERE decision_id=?2 AND target_ref=?3",
                params![node_id, decision_id, target_ref],
            )?;
        }
        Ok(())
    }

    fn index_source_files(
        &self,
        paths: &[PathBuf],
        packages: &[(String, String, String)],
    ) -> Result<Vec<Node>> {
        let symbol_re = Regex::new(
            r"^\s*(pub(?:\([^)]*\))?\s+)?(?:(async)\s+)?(fn|struct|enum|trait|type|const|static|mod|macro_rules!)\s+([A-Za-z_][A-Za-z0-9_]*)",
        )?;
        let mut nodes = Vec::new();
        for path in paths {
            let relative = relative(&self.root, path);
            let text = fs::read_to_string(path).unwrap_or_default();
            if let Some(ra) = &self.ra
                && let Ok(mut ra) = ra.lock()
            {
                ra.did_change(&format!("file://{}", path.display()), &text);
            }
            let lines: Vec<_> = text.lines().collect();
            let crate_name = crate_for_file(packages, path);
            for (offset, line) in lines.iter().enumerate() {
                let Some(cap) = symbol_re.captures(line) else {
                    continue;
                };
                let name = cap.get(4).unwrap().as_str().to_owned();
                let kind = cap.get(3).unwrap().as_str().to_owned();
                let visibility = if cap.get(1).is_some() {
                    "public"
                } else {
                    "private"
                }
                .to_string();
                let end = item_end(&lines, offset);
                let canonical_name = format!(
                    "{}::{}",
                    relative.trim_end_matches(".rs").replace('/', "::"),
                    name
                );
                let hash = format!(
                    "b3:{}",
                    blake3::hash(lines[offset..end].join("\n").as_bytes()).to_hex()
                );
                self.db.execute("INSERT INTO nodes(kind, canonical_name, crate_name, file, start_line, end_line, visibility, content_hash, metadata) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, '{}')", params![kind, canonical_name, crate_name, relative, offset + 1, end, visibility, hash])?;
                let id = self.db.last_insert_rowid();
                nodes.push(Node {
                    id,
                    kind,
                    canonical_name,
                    crate_name: crate_name.clone(),
                    file: relative.clone(),
                    start_line: offset + 1,
                    end_line: end,
                    visibility,
                    content_hash: hash,
                });
            }
        }
        Ok(nodes)
    }

    fn index_static_references(&self, nodes: &[Node], revision: &str) -> Result<()> {
        let word = Regex::new(r"[A-Za-z_][A-Za-z0-9_]*")?;
        for file in rust_files(&self.root) {
            let text = fs::read_to_string(&file).unwrap_or_default();
            let rel = relative(&self.root, &file);
            let source = nodes.iter().find(|n| n.file == rel).map(|n| n.id);
            let words: BTreeSet<_> = word.find_iter(&text).map(|m| m.as_str()).collect();
            for target in nodes
                .iter()
                .filter(|n| words.contains(short_name(&n.canonical_name)))
            {
                if Some(target.id) == source {
                    continue;
                }
                self.db.execute("INSERT INTO edges(src, dst, kind, context_json, confidence, provenance, revision, metadata) VALUES (?1, ?2, 'REFERENCES', ?3, 0.55, 'StaticIndex', ?4, ?5)", params![source, target.id, "{}", revision, json!({"file":rel}).to_string()])?;
            }
        }
        Ok(())
    }

    fn index_semantic_edges(&self, nodes: &[Node], revision: &str) {
        let Some(client) = &self.ra else {
            return;
        };
        let Ok(mut client) = client.lock() else {
            return;
        };
        let by_location = |path: &Path, line: usize| {
            let file = relative(&self.root, path);
            nodes
                .iter()
                .find(|node| node.file == file && node.start_line <= line && line <= node.end_line)
        };
        for target in nodes.iter().take(32) {
            let path = self.root.join(&target.file);
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            let Some(line_text) = text.lines().nth(target.start_line.saturating_sub(1)) else {
                continue;
            };
            let Some(byte) = line_text.find(short_name(&target.canonical_name)) else {
                continue;
            };
            let character = line_text[..byte].encode_utf16().count();
            let uri = format!("file://{}", path.display());
            for (kind, locations) in [
                (
                    "REFERENCES",
                    client.references(&uri, target.start_line - 1, character),
                ),
                (
                    "IMPLEMENTS",
                    client.implementations(&uri, target.start_line - 1, character),
                ),
            ] {
                let Some(locations) = locations else { continue };
                for location in locations.into_iter().take(30) {
                    let Some(location_uri) = location.get("uri").and_then(Value::as_str) else {
                        continue;
                    };
                    let Some(location_path) =
                        location_uri.strip_prefix("file://").map(PathBuf::from)
                    else {
                        continue;
                    };
                    if !location_path.starts_with(&self.root) {
                        continue;
                    }
                    let Some(line) = location
                        .get("range")
                        .and_then(|range| range.get("start"))
                        .and_then(|start| start.get("line"))
                        .and_then(Value::as_u64)
                        .map(|line| line as usize + 1)
                    else {
                        continue;
                    };
                    let Some(source) = by_location(&location_path, line) else {
                        continue;
                    };
                    if source.id == target.id {
                        continue;
                    }
                    let _ = self.db.execute(
                        "INSERT INTO edges(src,dst,kind,context_json,confidence,provenance,revision,metadata) VALUES (?1,?2,?3,?4,1.0,'RustAnalyzer',?5,?6) ON CONFLICT DO NOTHING",
                        params![source.id, target.id, kind, "{}", revision, json!({"file": relative(&self.root, &location_path), "line": line}).to_string()],
                    );
                }
            }
        }
    }

    fn index_cargo_matrix(&self, revision: &str) -> Result<()> {
        let output = Command::new("cargo")
            .args(["metadata", "--format-version=1", "--no-deps"])
            .current_dir(&self.root)
            .output()
            .context("reading Cargo metadata for feature/target matrix")?;
        let metadata: Value = serde_json::from_slice(&output.stdout)
            .context("parsing Cargo metadata for feature/target matrix")?;
        let Some(packages) = metadata.get("packages").and_then(Value::as_array) else {
            return Ok(());
        };
        for package in packages {
            let Some(name) = package.get("name").and_then(Value::as_str) else {
                continue;
            };
            let edition = package.get("edition").and_then(Value::as_str).unwrap_or("");
            if let Some(targets) = package.get("targets").and_then(Value::as_array) {
                for target in targets {
                    self.db.execute(
                        "INSERT OR REPLACE INTO package_targets(package,target_name,kind,crate_types,required_features,edition,doc,doctest,test,bench,revision) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
                        params![name,
                            target.get("name").and_then(Value::as_str).unwrap_or(""),
                            target.get("kind").cloned().unwrap_or_else(|| json!([])).to_string(),
                            target.get("crate_types").cloned().unwrap_or_else(|| json!([])).to_string(),
                            target.get("required-features").cloned().unwrap_or_else(|| json!([])).to_string(),
                            edition,
                            target.get("doc").and_then(Value::as_bool).unwrap_or(false),
                            target.get("doctest").and_then(Value::as_bool).unwrap_or(false),
                            target.get("test").and_then(Value::as_bool).unwrap_or(false),
                            target.get("bench").and_then(Value::as_bool).unwrap_or(false),
                            revision],
                    )?;
                }
            }
            if let Some(features) = package.get("features").and_then(Value::as_object) {
                for (feature, dependencies) in features {
                    self.db.execute(
                        "INSERT OR REPLACE INTO package_features(package,feature,dependencies,revision) VALUES (?1,?2,?3,?4)",
                        params![name, feature, dependencies.to_string(), revision],
                    )?;
                }
            }
        }
        Ok(())
    }

    /// Index only explicit lifecycle statements. Naming conventions create a
    /// low-confidence *candidate*, never a claim that deletion is safe.
    fn index_lifecycle_evidence(&self, nodes: &[Node], revision: &str) -> Result<()> {
        self.db.execute("DELETE FROM lifecycle_edges", [])?;
        let explicit = Regex::new(
            r"(?i)(?:deprecated|legacy|compat(?:ibility)?|fallback|superseded|replaced)\D{0,80}(?:use|with|by|replace(?:d)?\s+(?:with\s+)?)\s*`?([A-Za-z_][A-Za-z0-9_]*)`?",
        )?;
        let lifecycle_name = Regex::new(r"(?i)(legacy|deprecated|compat|fallback|adapter|v1|old)")?;
        for node in nodes {
            let text = fs::read_to_string(self.root.join(&node.file)).unwrap_or_default();
            let start = node.start_line.saturating_sub(5);
            let excerpt = text
                .lines()
                .skip(start)
                .take(28)
                .collect::<Vec<_>>()
                .join("\n");
            let replacement = explicit
                .captures(&excerpt)
                .and_then(|capture| capture.get(1))
                .map(|capture| capture.as_str())
                .and_then(|name| {
                    nodes
                        .iter()
                        .find(|other| short_name(&other.canonical_name) == name)
                });
            let inferred_lifecycle = lifecycle_name.is_match(short_name(&node.canonical_name))
                || lifecycle_name.is_match(&excerpt);
            if replacement.is_none() && !inferred_lifecycle {
                continue;
            }
            let (canonical_node, kind, confidence, provenance) = match replacement {
                Some(target) => (Some(target.id), "supersedes", 0.95, "SourceDoc"),
                None => (None, "suspected_legacy", 0.45, "Heuristic"),
            };
            self.db.execute(
                "INSERT INTO lifecycle_edges(legacy_node, canonical_node, kind, evidence, provenance, confidence, revision) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![node.id, canonical_node, kind, excerpt, provenance, confidence, revision],
            )?;
        }
        Ok(())
    }

    fn index_proposed_work(&self, revision: &str) -> Result<()> {
        let marker = Regex::new(r"(?i)\b(TODO|FIXME|XXX)\b\s*[:\-]?\s*(.+)")?;
        let indexed_at = Utc::now().to_rfc3339();
        // Preserve the item for architectural history, but make disappearing
        // automatic evidence visibly stale instead of silently trusting it.
        self.db.execute(
            "UPDATE work_items SET evidence_json=?1, confidence=0.20, last_validated_snapshot=?2, updated_at=?3 \
             WHERE provenance='SourceDoc' AND discovered_from='TODO/FIXME scanner' AND status='proposed'",
            params![json!(["Source marker absent at the current snapshot; review or remove this proposed item."]).to_string(), revision, indexed_at],
        )?;
        for path in all_text_files(&self.root) {
            let relative_path = relative(&self.root, &path);
            let text = fs::read_to_string(&path).unwrap_or_default();
            for (offset, line) in text.lines().enumerate() {
                let Some(capture) = marker.captures(line) else {
                    continue;
                };
                let title = capture
                    .get(2)
                    .map(|value| value.as_str().trim())
                    .unwrap_or("Repository follow-up");
                let id = format!(
                    "work_{}",
                    &blake3::hash(format!("{relative_path}:{offset}:{title}").as_bytes()).to_hex()
                        [..12]
                );
                self.db.execute(
                    "INSERT INTO work_items(id,title,status,priority,kind,scope_json,evidence_json,depends_json,blocked_json,acceptance_json,verification_json,discovered_from,provenance,confidence,last_validated_snapshot,created_at,updated_at) \
                     VALUES (?1,?2,'proposed','normal','documentation',?3,?4,'[]','[]','[]','[]',?5,'SourceDoc',0.70,?6,?7,?7) \
                     ON CONFLICT(id) DO UPDATE SET evidence_json=excluded.evidence_json,confidence=excluded.confidence,last_validated_snapshot=excluded.last_validated_snapshot,updated_at=excluded.updated_at",
                    params![id, title, json!([relative_path]).to_string(), json!([format!("{relative_path}:{}: {}", offset + 1, line.trim())]).to_string(), "TODO/FIXME scanner", revision, indexed_at],
                )?;
            }
        }
        Ok(())
    }

    fn index_git(&self, revision: &str) -> Result<()> {
        let Some(log) = command_text(
            &self.root,
            &[
                "log",
                "--format=%H%x1f%aI%x1f%an%x1f%s",
                "--name-only",
                "-n",
                "250",
            ],
        ) else {
            return Ok(());
        };
        let mut files_by_commit: Vec<Vec<String>> = Vec::new();
        for section in log.split("\n\n") {
            let mut lines = section.lines();
            let Some(header) = lines.next() else { continue };
            let bits: Vec<_> = header.split('\x1f').collect();
            if bits.len() != 4 {
                continue;
            }
            self.db.execute("INSERT OR REPLACE INTO commits(hash, timestamp, author, subject, revision) VALUES (?1, ?2, ?3, ?4, ?5)", params![bits[0], bits[1], bits[2], bits[3], revision])?;
            let files: Vec<_> = lines.filter(|x| !x.is_empty()).map(str::to_owned).collect();
            for file in &files {
                self.db.execute(
                    "INSERT OR REPLACE INTO commit_files(commit_hash, file) VALUES (?1, ?2)",
                    params![bits[0], file],
                )?;
            }
            files_by_commit.push(files);
        }
        let mut pairs: HashMap<(String, String), i64> = HashMap::new();
        for files in files_by_commit {
            for (i, a) in files.iter().enumerate() {
                for b in files.iter().skip(i + 1) {
                    let key = if a < b {
                        (a.clone(), b.clone())
                    } else {
                        (b.clone(), a.clone())
                    };
                    *pairs.entry(key).or_default() += 1;
                }
            }
        }
        for ((a, b), count) in pairs {
            self.db.execute("INSERT OR REPLACE INTO co_changes(node_a, node_b, count, score) VALUES (?1, ?2, ?3, ?4)", params![a, b, count, (count as f64 / 10.0).min(1.0)])?;
        }
        Ok(())
    }

    fn index_documents_and_use_cases(&self, nodes: &[Node], revision: &str) -> Result<()> {
        for entry in WalkDir::new(&self.root)
            .into_iter()
            .filter_entry(|entry| {
                let name = entry.file_name().to_string_lossy();
                !matches!(name.as_ref(), "target" | ".git" | INDEX_DIRECTORY)
            })
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_file())
        {
            let path = entry.path();
            let is_document = path
                .extension()
                .is_some_and(|ext| ext == "md" || ext == "txt");
            if is_document {
                let text = fs::read_to_string(path).unwrap_or_default();
                self.db.execute(
                    "INSERT INTO documents(kind, path, text, revision) VALUES (?1, ?2, ?3, ?4)",
                    params!["source_doc", relative(&self.root, path), text, revision],
                )?;
            }
        }
        for node in nodes.iter().filter(|node| {
            node.file.contains("test") || short_name(&node.canonical_name).starts_with("test_")
        }) {
            let name = short_name(&node.canonical_name).replace('_', " ");
            let id = format!("usecase:{}", slug(&format!("{}-{}", node.file, name)));
            self.db.execute(
                "INSERT OR REPLACE INTO use_cases(id, name, description, provenance, confidence, revision) VALUES (?1, ?2, ?3, 'AgentInference', 0.60, ?4)",
                params![id, name, format!("Behavior exercised by {}", node.canonical_name), revision],
            )?;
            self.db.execute(
                "INSERT OR REPLACE INTO use_case_nodes(use_case_id, node_id, relationship, provenance, confidence, revision) VALUES (?1, ?2, 'TESTS', 'AgentInference', 0.60, ?3)",
                params![id, node.id, revision],
            )?;
        }
        Ok(())
    }

    pub fn orient(&self, intent: &str) -> Result<Value> {
        let terms = terms(intent);
        let nodes = self.search_nodes(&terms, 12)?;
        let crates: Vec<String> = self
            .db
            .prepare("SELECT name FROM packages ORDER BY name")?
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let dependency_count: i64 =
            self.db
                .query_row("SELECT COUNT(*) FROM package_dependencies", [], |row| {
                    row.get(0)
                })?;
        Ok(
            json!({"intent":intent,"revision":self.revision(),"architecture":{"crates":crates,"resolved_cargo_dependency_edges":dependency_count,"likely_symbols":nodes,"index_status":self.index_status()},"next":"Call repo.prepare_change before modifying source."}),
        )
    }

    pub fn prepare_change(
        &self,
        intent: &str,
        targets: &[String],
        depth: usize,
        budget: Option<usize>,
    ) -> Result<Value> {
        let context_id = format!(
            "ctx_{}",
            &blake3::hash(format!("{}:{:?}:{}", intent, targets, Utc::now()).as_bytes()).to_hex()
                [..12]
        );
        let mut target_nodes = Vec::new();
        for target in targets {
            target_nodes.extend(self.search_nodes(&terms(target), 8)?);
        }
        if target_nodes.is_empty() {
            target_nodes = self.search_nodes(&terms(intent), 8)?;
        }
        dedupe_nodes(&mut target_nodes);
        let ids: Vec<i64> = target_nodes.iter().map(|n| n.id).collect();
        let references = self.references_for(&ids, depth)?;
        let semantic_references = self.semantic_references(&target_nodes);
        let tests = self.test_nodes(&target_nodes)?;
        let use_cases = self.use_cases_for(&tests)?;
        let decisions = self.decisions_for(&targets.join(" "))?;
        let lifecycle = self.lifecycle_for(&target_nodes)?;
        let obsolete = self.obsolete_candidates(Some(&targets.join(" ")), 12)?;
        let work = self.work_list(Some(&targets.join(" ")), 12)?;
        let likely_surface = files_for_nodes(&target_nodes, &references);
        let max = budget.unwrap_or(12_000).min(40_000);
        let mut slices = Vec::new();
        let mut used = 0;
        for (node, reason, relation) in target_nodes
            .iter()
            .map(|n| (n, "target", "DEFINES"))
            .chain(references.iter().map(|n| (n, "reference", "REFERENCES")))
            .chain(tests.iter().map(|n| (n, "behavioral test", "TESTS")))
        {
            let slice = self.source_slice(node, reason, relation)?;
            let size = slice.source.len();
            if used + size > max {
                break;
            }
            used += size;
            slices.push(slice);
        }
        let payload = json!({"context_id":context_id,"intent":intent,"revision":self.revision(),"snapshot":self.snapshot(),"primary_symbols":target_nodes,"references":references,"reference_provenance":{"semantic":"RustAnalyzer (confidence 1.0) when persisted","fallback":"StaticIndex (confidence 0.55)"},"semantic_references":semantic_references,"tests":tests,"use_cases":use_cases,"decisions":decisions,"lifecycle":lifecycle,"obsolete_candidates":obsolete.get("candidates"),"work_items":work.get("items"),"likely_change_surface":likely_surface,"source_slices":slices,"risk":risk(&target_nodes, &references),"unresolved_edges":["Runtime registration, generated code, dynamic dispatch, feature/target-specific paths, external consumers, and deployment state may require manual confirmation."],"semantic_note":"rust-analyzer resolved locations, when present, are semantic facts. StaticIndex relationships are candidate references, not resolved facts."});
        self.db.execute("INSERT INTO change_contexts(id, intent, payload, revision, created_at) VALUES (?1, ?2, ?3, ?4, ?5)", params![context_id, intent, payload.to_string(), self.revision().workspace_digest, Utc::now().to_rfc3339()])?;
        Ok(payload)
    }

    pub fn expand_context(&self, id: &str, around: &str, relation: &str) -> Result<Value> {
        let context: String = self
            .db
            .query_row(
                "SELECT payload FROM change_contexts WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .optional()?
            .context("unknown change context")?;
        let nodes = self.search_nodes(&terms(around), 20)?;
        let related = match relation {
            "callers" | "references" => {
                self.references_for(&nodes.iter().map(|n| n.id).collect::<Vec<_>>(), 2)?
            }
            _ => nodes.clone(),
        };
        let slices: Result<Vec<_>> = related
            .iter()
            .take(12)
            .map(|n| self.source_slice(n, "expanded context", relation))
            .collect();
        Ok(
            json!({"context_id":id,"around":around,"relation":relation,"parent_context":serde_json::from_str::<Value>(&context)?,"source_slices":slices?,"revision":self.revision()}),
        )
    }

    pub fn history(&self, symbol: &str) -> Result<Value> {
        let nodes = self.search_nodes(&terms(symbol), 8)?;
        let mut commits = Vec::new();
        for node in &nodes {
            if let Some(output) = command_text(
                &self.root,
                &["log", "--format=%H%x1f%s", "-n", "12", "--", &node.file],
            ) {
                commits.extend(output.lines().filter_map(|l| {
                    let p: Vec<_> = l.split('\x1f').collect();
                    (p.len() == 2).then(|| json!({"hash":p[0],"subject":p[1],"file":node.file}))
                }));
            }
        }
        let mut co_changes = Vec::new();
        for node in &nodes {
            let mut statement = self.db.prepare(
                "SELECT node_a, node_b, count, score FROM co_changes \
                 WHERE node_a=?1 OR node_b=?1 ORDER BY count DESC LIMIT 5",
            )?;
            let matches = statement.query_map([&node.file], |row| {
                let first: String = row.get(0)?;
                let second: String = row.get(1)?;
                Ok(json!({
                    "file": if first == node.file { second } else { first },
                    "count": row.get::<_, i64>(2)?,
                    "score": row.get::<_, f64>(3)?,
                }))
            })?;
            co_changes.extend(matches.filter_map(Result::ok));
        }
        Ok(
            json!({"symbol":symbol,"nodes":nodes,"commits":commits,"frequent_co_change":co_changes,"revision":self.revision()}),
        )
    }

    pub fn record_decision(&self, input: RecordDecision) -> Result<Value> {
        let next: i64 = self.db.query_row(
            "SELECT COALESCE(MAX(sequence), 0) + 1 FROM decisions",
            [],
            |r| r.get(0),
        )?;
        let id = format!("DEC-{:04}", next);
        self.db.execute("INSERT INTO decisions(id, sequence, status, title, rationale, applies_to, consequences, supersedes, revision, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)", params![id, next, input.status, input.title, input.reason, serde_json::to_string(&input.applies_to)?, serde_json::to_string(&input.consequences)?, input.supersedes, self.revision().workspace_digest, Utc::now().to_rfc3339()])?;
        for target in &input.applies_to {
            let node_id = self
                .search_nodes(&terms(target), 1)?
                .into_iter()
                .next()
                .map(|node| node.id);
            self.db.execute(
                "INSERT OR REPLACE INTO decision_targets(decision_id, node_id, target_ref) VALUES (?1, ?2, ?3)",
                params![id, node_id, target],
            )?;
        }
        self.db.execute(
            "INSERT OR REPLACE INTO metadata(key, value) VALUES ('revision', ?1)",
            [serde_json::to_string(&self.revision())?],
        )?;
        let path = if input.materialize {
            let dir = self.root.join("docs/decisions");
            fs::create_dir_all(&dir)?;
            let path = dir.join(format!("{:04}-{}.md", next, slug(&input.title)));
            fs::write(&path, decision_markdown(&id, &input))?;
            Some(relative(&self.root, &path))
        } else {
            None
        };
        Ok(
            json!({"id":id,"status":input.status,"resource_uri":format!("rustrepo://decision/{id}"),"materialized_path":path,"revision":self.revision()}),
        )
    }

    pub fn validate_change(
        &self,
        context_id: &str,
        diff: Option<&str>,
        run_checks: bool,
    ) -> Result<Value> {
        let payload: String = self
            .db
            .query_row(
                "SELECT payload FROM change_contexts WHERE id=?1",
                [context_id],
                |r| r.get(0),
            )
            .optional()?
            .context("unknown change context")?;
        let context: Value = serde_json::from_str(&payload)?;
        let diff = diff
            .map(str::to_string)
            .or_else(|| command_text(&self.root, &["diff", "--"]))
            .unwrap_or_default();
        let changed_files: BTreeSet<String> = Regex::new(r"(?m)^\+\+\+ b/(.+)$")?
            .captures_iter(&diff)
            .filter_map(|c| c.get(1).map(|m| m.as_str().to_string()))
            .collect();
        let expected: BTreeSet<String> = context
            .get("likely_change_surface")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
        let unmodified: Vec<_> = expected.difference(&changed_files).cloned().collect();
        let checks = if run_checks {
            vec![
                check(&self.root, &["fmt", "--check"]),
                check(&self.root, &["check", "--all-targets"]),
                check(
                    &self.root,
                    &["clippy", "--all-targets", "--", "-D", "warnings"],
                ),
            ]
        } else {
            Vec::new()
        };
        let lifecycle = context
            .get("lifecycle")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let legacy_left = lifecycle
            .into_iter()
            .filter_map(|entry| {
                entry
                    .get("symbol")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .filter(|symbol| diff.contains(symbol))
            .collect::<Vec<_>>();
        Ok(
            json!({"context_id":context_id,"revision_before":context.get("revision"),"revision_after":self.revision(),"snapshot":self.snapshot(),"changed_files":changed_files,"expected_affected_files":expected,"unmodified_expected_callers":unmodified,"new_references":"Re-run repo.prepare_change after signature changes to refresh static candidates.","architectural_violations":self.decision_conflicts(&diff)?,"legacy_paths_touched":legacy_left,"unresolved_edges":context.get("unresolved_edges"),"recommended_tests":context.get("tests"),"recommended_commands":["cargo fmt --check","cargo check --all-targets","cargo clippy --all-targets -- -D warnings","cargo test"],"checks":checks,"uncertainty":"This validates indexed evidence and diff scope, not unindexed runtime registration, generated code, external consumers, or deployment state."}),
        )
    }

    pub fn cleanup_candidates(&self, scope: Option<&str>) -> Result<Value> {
        let scope = scope.unwrap_or("");
        let nodes = self.search_nodes(&terms(scope), 500)?;
        let mut candidates = Vec::new();
        for node in nodes {
            if node.visibility == "private" {
                let inbound: i64 = self.db.query_row(
                    "SELECT COUNT(*) FROM edges WHERE dst=?1",
                    [node.id],
                    |r| r.get(0),
                )?;
                if inbound == 0 {
                    candidates.push(json!({"symbol":node.canonical_name,"file":node.file,"kind":node.kind,"cleanup_score":0.45,"reason":"private symbol has no indexed inbound references","confidence":0.55,"provenance":"StaticIndex"}));
                }
            }
        }
        Ok(
            json!({"scope":scope,"candidates":candidates,"note":"Candidates must be confirmed with rust-analyzer/compiler before deletion.","revision":self.revision()}),
        )
    }

    pub fn locate(&self, concept: &str, limit: usize, include_source: bool) -> Result<Value> {
        let mut nodes = Vec::new();
        for term in terms(concept) {
            nodes.extend(self.search_nodes(&[term], limit)?);
        }
        dedupe_nodes(&mut nodes);
        nodes.truncate(limit);
        let docs: Vec<Value> = self
            .db
            .prepare("SELECT path, text FROM documents WHERE lower(text) LIKE ?1 OR lower(path) LIKE ?1 LIMIT ?2")?
            .query_map(params![format!("%{}%", concept.to_lowercase()), limit as i64], |row| {
                let path: String = row.get(0)?;
                let text: String = row.get(1)?;
                Ok(json!({"path":path,"evidence":trim_text(&text, 500),"provenance":"SourceDoc","confidence":0.65}))
            })?
            .filter_map(Result::ok)
            .collect();
        let mut implementations = Vec::new();
        let mut tests = Vec::new();
        let mut canonical = Vec::new();
        for node in &nodes {
            let item = json!({"id":node.id,"symbol":node.canonical_name,"file":node.file,"kind":node.kind,"visibility":node.visibility,"provenance":"StaticIndex","confidence":0.65});
            if node.file.contains("test") || short_name(&node.canonical_name).starts_with("test_") {
                tests.push(item);
            } else if node.visibility == "public" {
                canonical.push(item);
            } else {
                implementations.push(item);
            }
        }
        let source_slices = if include_source {
            nodes
                .iter()
                .take(limit.min(8))
                .map(|node| self.source_slice(node, "concept candidate", "LOCATES"))
                .collect::<Result<Vec<_>>>()?
        } else {
            Vec::new()
        };
        Ok(
            json!({"concept":concept,"snapshot":self.snapshot(),"canonical_candidates":canonical,"implementation_candidates":implementations,"tests":tests,"documentation":docs,"governing_decisions":self.decisions_for(concept)?,"known_work":self.work_list(Some(concept), limit)?,"source_slices":source_slices,"blind_spots":["Runtime registration, generated code, external consumers, and configuration-selected implementations require confirmation unless indexed explicitly."],"context_budget":{"items":limit,"source_included":include_source}}),
        )
    }

    pub fn explain(&self, target: &str, budget: usize) -> Result<Value> {
        let nodes = self.search_nodes(&terms(target), budget.min(20))?;
        let lifecycle = self.lifecycle_for(&nodes)?;
        Ok(
            json!({"target":target,"snapshot":self.snapshot(),"nodes":nodes,"lifecycle":lifecycle,"decisions":self.decisions_for(target)?,"work":self.work_list(Some(target), budget.min(10))?,"source_slices":nodes.iter().take(4).map(|node|self.source_slice(node,"explanation","DEFINES")).collect::<Result<Vec<_>>>()?,"confidence_note":"Resolved rust-analyzer locations are compiler-backed; lifecycle and text-derived results carry their own provenance and confidence."}),
        )
    }

    pub fn why(&self, target: &str) -> Result<Value> {
        let nodes = self.search_nodes(&terms(target), 12)?;
        Ok(
            json!({"target":target,"snapshot":self.snapshot(),"decisions":self.decisions_for(target)?,"lifecycle":self.lifecycle_for(&nodes)?,"history":self.history(target)?,"evidence_policy":"Only human-authored decisions and source/Git evidence are confirmed. Heuristics remain labeled inferences."}),
        )
    }

    pub fn constraints(&self, change: &str) -> Result<Value> {
        Ok(
            json!({"change":change,"snapshot":self.snapshot(),"decisions":self.decisions_for(change)?,"lifecycle_risks":self.obsolete_candidates(Some(change), 20)?,"unresolved_edges":["Dynamic dispatch, macros, runtime registration, generated bindings, external API consumers, and deployment state may not be resolved by the current index."],"required_verification":["cargo check --all-targets","cargo test","Review relevant feature/target combinations and persisted/external contracts."]}),
        )
    }

    pub fn obsolete_candidates(&self, scope: Option<&str>, limit: usize) -> Result<Value> {
        let scope = scope.unwrap_or("");
        let mut statement = self.db.prepare("SELECT l.legacy_node,l.canonical_node,l.kind,l.evidence,l.provenance,l.confidence,n.kind,n.canonical_name,n.file,n.visibility,c.canonical_name FROM lifecycle_edges l JOIN nodes n ON n.id=l.legacy_node LEFT JOIN nodes c ON c.id=l.canonical_node WHERE lower(n.canonical_name) LIKE ?1 OR lower(n.file) LIKE ?1 OR ?1='%%' ORDER BY l.confidence DESC LIMIT ?2")?;
        let needle = if scope.is_empty() {
            "%%".to_owned()
        } else {
            format!("%{}%", scope.to_lowercase())
        };
        let records: Vec<LifecycleRecord> = statement
            .query_map(params![needle, limit as i64], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    Node {
                        id: row.get(0)?,
                        kind: row.get(6)?,
                        canonical_name: row.get(7)?,
                        crate_name: None,
                        file: row.get(8)?,
                        start_line: 0,
                        end_line: 0,
                        visibility: row.get(9)?,
                        content_hash: String::new(),
                    },
                    row.get(10)?,
                ))
            })?
            .filter_map(Result::ok)
            .collect();
        let mut candidates = Vec::new();
        for (
            legacy_id,
            canonical_id,
            kind,
            evidence,
            provenance,
            confidence,
            node,
            canonical_name,
        ) in records
        {
            let callers = self.references_for(&[legacy_id], 1)?;
            let classification = if canonical_id.is_some() {
                "architecturally-superseded"
            } else {
                "suspected-stale-fallback"
            };
            candidates.push(json!({"classification":classification,"symbol":node.canonical_name,"file":node.file,"replacement":canonical_name,"lifecycle_kind":kind,"evidence":trim_text(&evidence,900),"provenance":provenance,"confidence":confidence,"current_callers":callers,"complete_removal_slice":{"remove":[node.canonical_name],"update_callers":callers.iter().map(|caller|caller.canonical_name.clone()).collect::<Vec<_>>(),"expected_affected_systems":[node.file],"verification_boundary":["cargo check --all-targets","cargo test","Confirm no external/public consumer or configuration path requires the compatibility boundary."]},"unresolved_questions":["Are there external consumers, runtime-selected paths, generated code, or persisted historical formats not represented by the repository index?"]}));
        }
        Ok(
            json!({"scope":scope,"snapshot":self.snapshot(),"candidates":candidates,"safety_note":"Referenced code is not presumed required or removable. Only explicit replacement evidence produces an architecturally-superseded candidate; removal remains blocked on the stated verification boundary."}),
        )
    }

    pub fn work_list(&self, query: Option<&str>, limit: usize) -> Result<Value> {
        let query = query.unwrap_or("").to_lowercase();
        let mut statement = self.db.prepare("SELECT id,title,status,priority,kind,scope_json,evidence_json,depends_json,blocked_json,acceptance_json,verification_json,discovered_from,provenance,confidence,last_validated_snapshot,updated_at FROM work_items WHERE lower(title) LIKE ?1 OR lower(scope_json) LIKE ?1 OR ?1='%%' ORDER BY CASE priority WHEN 'critical' THEN 0 WHEN 'high' THEN 1 WHEN 'normal' THEN 2 ELSE 3 END, updated_at DESC LIMIT ?2")?;
        let items = statement
            .query_map(
                params![
                    if query.is_empty() {
                        "%%".to_owned()
                    } else {
                        format!("%{query}%")
                    },
                    limit as i64
                ],
                work_row,
            )?
            .filter_map(Result::ok)
            .collect::<Vec<_>>();
        Ok(json!({"query":query,"snapshot":self.snapshot(),"items":items}))
    }

    pub fn work_next(&self, query: Option<&str>) -> Result<Value> {
        let items = self.work_list(query, 50)?["items"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let next = items.into_iter().find(|item| {
            item["status"] == "accepted"
                || item["status"] == "in_progress"
                || item["status"] == "proposed"
        });
        Ok(
            json!({"snapshot":self.snapshot(),"next":next,"selection_note":"Blocked, completed, and obsolete work is excluded. Proposed work still requires confirmation before becoming an accepted plan."}),
        )
    }

    pub fn work_propose(&self, input: WorkItemInput) -> Result<Value> {
        let revision = self.revision();
        let identity = format!("{}:{:?}", input.title, input.scope);
        let id = format!("work_{}", &blake3::hash(identity.as_bytes()).to_hex()[..12]);
        self.db.execute("INSERT INTO work_items(id,title,status,priority,kind,scope_json,evidence_json,depends_json,blocked_json,acceptance_json,verification_json,discovered_from,provenance,confidence,last_validated_snapshot,created_at,updated_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,'explicit MCP proposal','HumanDecision',1.0,?12,?13,?13) ON CONFLICT(id) DO UPDATE SET title=excluded.title,evidence_json=excluded.evidence_json,updated_at=excluded.updated_at", params![id,input.title,input.status,input.priority,input.kind,serde_json::to_string(&input.scope)?,serde_json::to_string(&input.evidence)?,serde_json::to_string(&input.depends_on)?,serde_json::to_string(&input.blocked_by)?,serde_json::to_string(&input.acceptance_criteria)?,serde_json::to_string(&input.verification)?,revision.workspace_digest,Utc::now().to_rfc3339()])?;
        Ok(
            json!({"id":id,"snapshot":self.snapshot(),"status":input.status,"provenance":"HumanDecision"}),
        )
    }

    pub fn work_update(&self, id: &str, patch: &Value) -> Result<Value> {
        let current = self.db.query_row("SELECT status,priority,kind,scope_json,evidence_json,depends_json,blocked_json,acceptance_json,verification_json FROM work_items WHERE id=?1",[id],|row|Ok(json!({"status":row.get::<_,String>(0)?,"priority":row.get::<_,String>(1)?,"kind":row.get::<_,String>(2)?,"scope":serde_json::from_str::<Value>(&row.get::<_,String>(3)?).unwrap_or(json!([])),"evidence":serde_json::from_str::<Value>(&row.get::<_,String>(4)?).unwrap_or(json!([])),"depends_on":serde_json::from_str::<Value>(&row.get::<_,String>(5)?).unwrap_or(json!([])),"blocked_by":serde_json::from_str::<Value>(&row.get::<_,String>(6)?).unwrap_or(json!([])),"acceptance_criteria":serde_json::from_str::<Value>(&row.get::<_,String>(7)?).unwrap_or(json!([])),"verification":serde_json::from_str::<Value>(&row.get::<_,String>(8)?).unwrap_or(json!([]))})))?;
        let field = |name: &str| {
            patch
                .get(name)
                .cloned()
                .unwrap_or_else(|| current[name].clone())
        };
        self.db.execute("UPDATE work_items SET status=?1,priority=?2,kind=?3,scope_json=?4,evidence_json=?5,depends_json=?6,blocked_json=?7,acceptance_json=?8,verification_json=?9,last_validated_snapshot=?10,updated_at=?11 WHERE id=?12",params![field("status").as_str(),field("priority").as_str(),field("kind").as_str(),field("scope").to_string(),field("evidence").to_string(),field("depends_on").to_string(),field("blocked_by").to_string(),field("acceptance_criteria").to_string(),field("verification").to_string(),self.revision().workspace_digest,Utc::now().to_rfc3339(),id])?;
        Ok(json!({"id":id,"snapshot":self.snapshot(),"updated":true}))
    }

    pub fn status(&self) -> Result<Value> {
        let nodes: i64 = self
            .db
            .query_row("SELECT COUNT(*) FROM nodes", [], |row| row.get(0))?;
        let edges: i64 = self
            .db
            .query_row("SELECT COUNT(*) FROM edges", [], |row| row.get(0))?;
        let lifecycle: i64 =
            self.db
                .query_row("SELECT COUNT(*) FROM lifecycle_edges", [], |row| row.get(0))?;
        let work: i64 = self
            .db
            .query_row("SELECT COUNT(*) FROM work_items", [], |row| row.get(0))?;
        let package_targets: i64 =
            self.db
                .query_row("SELECT COUNT(*) FROM package_targets", [], |row| row.get(0))?;
        let package_features: i64 =
            self.db
                .query_row("SELECT COUNT(*) FROM package_features", [], |row| {
                    row.get(0)
                })?;
        Ok(
            json!({"snapshot":self.snapshot(),"index":self.index_status(),"counts":{"nodes":nodes,"edges":edges,"lifecycle_evidence":lifecycle,"work_items":work,"package_targets":package_targets,"package_features":package_features},"degraded_areas":["Static source edges do not prove runtime, macro-generated, dynamically-dispatched, external, or deployment relationships."],"cache":"rebuildable SQLite cache; no repository files are changed by indexing."}),
        )
    }

    pub fn matrix(&self) -> Result<Value> {
        let targets = self
            .db
            .prepare("SELECT package,target_name,kind,crate_types,required_features,edition,doc,doctest,test,bench FROM package_targets ORDER BY package,target_name")?
            .query_map([], |row| {
                Ok(json!({
                    "package": row.get::<_, String>(0)?,
                    "target": row.get::<_, String>(1)?,
                    "kind": serde_json::from_str::<Value>(&row.get::<_, String>(2)?).unwrap_or_else(|_| json!([])),
                    "crate_types": serde_json::from_str::<Value>(&row.get::<_, String>(3)?).unwrap_or_else(|_| json!([])),
                    "required_features": serde_json::from_str::<Value>(&row.get::<_, String>(4)?).unwrap_or_else(|_| json!([])),
                    "edition": row.get::<_, String>(5)?,
                    "doc": row.get::<_, bool>(6)?,
                    "doctest": row.get::<_, bool>(7)?,
                    "test": row.get::<_, bool>(8)?,
                    "bench": row.get::<_, bool>(9)?,
                }))
            })?
            .filter_map(Result::ok)
            .collect::<Vec<_>>();
        let features = self
            .db
            .prepare("SELECT package,feature,dependencies FROM package_features ORDER BY package,feature")?
            .query_map([], |row| {
                Ok(json!({
                    "package": row.get::<_, String>(0)?,
                    "feature": row.get::<_, String>(1)?,
                    "dependencies": serde_json::from_str::<Value>(&row.get::<_, String>(2)?).unwrap_or_else(|_| json!([])),
                }))
            })?
            .filter_map(Result::ok)
            .collect::<Vec<_>>();
        Ok(json!({"targets":targets,"features":features,"revision":self.revision()}))
    }

    pub fn refresh(&mut self, scope: Option<&str>) -> Result<Value> {
        match scope.unwrap_or("workspace") {
            "workspace" | "cargo" | "git" => self.reindex()?,
            "incremental" => self.refresh_if_stale()?,
            other => bail!(
                "unsupported refresh scope `{other}`; use workspace, cargo, git, or incremental"
            ),
        }
        self.status()
    }

    pub fn resource(&self, uri: &str) -> Result<Value> {
        let path = uri
            .strip_prefix("rustrepo://")
            .context("unsupported resource URI")?;
        let value = match path {
            "workspace/overview" => self.orient("workspace overview")?,
            _ if path.starts_with("symbol/") => {
                json!({"symbol":&path[7..],"nodes":self.search_nodes(&terms(&path[7..]),20)?,"revision":self.revision()})
            }
            _ if path.starts_with("change/") => {
                let id = &path[7..];
                let s: String = self.db.query_row(
                    "SELECT payload FROM change_contexts WHERE id=?1",
                    [id],
                    |r| r.get(0),
                )?;
                serde_json::from_str(&s)?
            }
            _ if path.starts_with("decision/") => self.decision_resource(&path[9..])?,
            _ if path.starts_with("history/") => self.history(&path[8..])?,
            _ if path.starts_with("work/") => self.work_list(Some(&path[5..]), 50)?,
            _ => bail!("unknown resource"),
        };
        Ok(value)
    }

    pub fn index_status(&self) -> Value {
        json!({"semantic_engine":"rust-analyzer LSP companion", "rust_analyzer_running":self.ra.is_some(),"fallback":"StaticIndex (provenance/confidence labelled)","store":"SQLite"})
    }

    fn snapshot(&self) -> Value {
        json!({"repository":self.root.to_string_lossy(),"branch":command_text(&self.root,&["branch","--show-current"]),"revision":self.revision(),"indexing_timestamp":self.db.query_row("SELECT value FROM metadata WHERE key='indexed_at'",[],|row|row.get::<_,String>(0)).optional().ok().flatten(),"cargo_assumptions":{"features":"cargo metadata default resolution","target_triple":Value::Null,"package_targets":self.db.query_row("SELECT COUNT(*) FROM package_targets",[],|row|row.get::<_,i64>(0)).unwrap_or(0),"package_features":self.db.query_row("SELECT COUNT(*) FROM package_features",[],|row|row.get::<_,i64>(0)).unwrap_or(0)},"index":self.index_status()})
    }

    fn lifecycle_for(&self, nodes: &[Node]) -> Result<Vec<Value>> {
        let mut output = Vec::new();
        for node in nodes {
            let mut statement = self.db.prepare("SELECT l.kind,l.evidence,l.provenance,l.confidence,c.canonical_name FROM lifecycle_edges l LEFT JOIN nodes c ON c.id=l.canonical_node WHERE l.legacy_node=?1 OR l.canonical_node=?1")?;
            let rows = statement.query_map([node.id], |row| Ok(json!({"symbol":node.canonical_name,"kind":row.get::<_,String>(0)?,"evidence":trim_text(&row.get::<_,String>(1)?,800),"provenance":row.get::<_,String>(2)?,"confidence":row.get::<_,f64>(3)?,"related_symbol":row.get::<_,Option<String>>(4)?})))?;
            output.extend(rows.filter_map(Result::ok));
        }
        Ok(output)
    }

    fn search_nodes(&self, query: &[String], limit: usize) -> Result<Vec<Node>> {
        let mut statement=self.db.prepare("SELECT id,kind,canonical_name,crate_name,file,start_line,end_line,visibility,content_hash FROM nodes WHERE lower(canonical_name) LIKE ?1 OR lower(file) LIKE ?1 ORDER BY visibility DESC, canonical_name LIMIT ?2")?;
        let pat = format!(
            "%{}%",
            query
                .first()
                .map(String::as_str)
                .unwrap_or("")
                .to_lowercase()
        );
        Ok(statement
            .query_map(params![pat, limit as i64], node_from_row)?
            .filter_map(Result::ok)
            .collect())
    }
    fn references_for(&self, ids: &[i64], depth: usize) -> Result<Vec<Node>> {
        if ids.is_empty() {
            return Ok(vec![]);
        }
        let mut seen = BTreeSet::new();
        let mut current = ids.to_vec();
        for _ in 0..depth.max(1) {
            let q = std::iter::repeat_n("?", current.len())
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                "SELECT DISTINCT n.id,n.kind,n.canonical_name,n.crate_name,n.file,n.start_line,n.end_line,n.visibility,n.content_hash FROM edges e JOIN nodes n ON e.src=n.id WHERE e.dst IN ({q}) LIMIT 80"
            );
            let mut stmt = self.db.prepare(&sql)?;
            let found: Vec<Node> = stmt
                .query_map(rusqlite::params_from_iter(current.iter()), node_from_row)?
                .filter_map(Result::ok)
                .collect();
            current = found
                .iter()
                .map(|n| n.id)
                .filter(|id| seen.insert(*id))
                .collect();
        }
        let mut out = Vec::new();
        for id in seen {
            out.push(self.node(id)?);
        }
        Ok(out)
    }
    fn node(&self, id: i64) -> Result<Node> {
        Ok(self.db.query_row("SELECT id,kind,canonical_name,crate_name,file,start_line,end_line,visibility,content_hash FROM nodes WHERE id=?1",[id],node_from_row)?)
    }
    fn test_nodes(&self, nodes: &[Node]) -> Result<Vec<Node>> {
        let names: Vec<String> = nodes
            .iter()
            .map(|n| short_name(&n.canonical_name).to_lowercase().to_string())
            .collect();
        let all = self.search_nodes(&["".into()], 500)?;
        Ok(all
            .into_iter()
            .filter(|n| {
                n.file.contains("test")
                    || names
                        .iter()
                        .any(|s| n.canonical_name.to_lowercase().contains(s))
            })
            .take(20)
            .collect())
    }
    fn semantic_references(&self, targets: &[Node]) -> Vec<SourceSlice> {
        let Some(client) = &self.ra else {
            return Vec::new();
        };
        let Ok(mut client) = client.lock() else {
            return Vec::new();
        };
        let mut slices = Vec::new();
        for target in targets.iter().take(4) {
            let path = self.root.join(&target.file);
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            let Some(line) = text.lines().nth(target.start_line.saturating_sub(1)) else {
                continue;
            };
            let Some(byte) = line.find(short_name(&target.canonical_name)) else {
                continue;
            };
            let character = line[..byte].encode_utf16().count();
            let uri = format!("file://{}", path.display());
            let Some(locations) = client.references(&uri, target.start_line - 1, character) else {
                continue;
            };
            for location in locations.into_iter().take(30) {
                let Some(uri) = location.get("uri").and_then(Value::as_str) else {
                    continue;
                };
                let Some(path) = uri.strip_prefix("file://").map(PathBuf::from) else {
                    continue;
                };
                if !path.starts_with(&self.root) {
                    continue;
                }
                let Some(start) = location
                    .get("range")
                    .and_then(|range| range.get("start"))
                    .and_then(|start| start.get("line"))
                    .and_then(Value::as_u64)
                else {
                    continue;
                };
                if let Ok(slice) =
                    self.location_slice(&path, start as usize, &target.canonical_name)
                {
                    slices.push(slice);
                }
            }
        }
        slices
    }
    fn location_slice(&self, path: &Path, line: usize, symbol: &str) -> Result<SourceSlice> {
        let text = fs::read_to_string(path)?;
        let lines: Vec<_> = text.lines().collect();
        let start = line.min(lines.len());
        let end = (start + 16).min(lines.len());
        let mut source = lines[start..end].join("\n");
        if source.len() > MAX_SLICE_BYTES {
            source.truncate(MAX_SLICE_BYTES);
        }
        Ok(SourceSlice {
            symbol: symbol.to_owned(),
            file: relative(&self.root, path),
            range: [start + 1, end],
            content_hash: format!("b3:{}", blake3::hash(source.as_bytes()).to_hex()),
            reason: "rust-analyzer resolved reference".into(),
            semantic_relationship: "REFERENCES".into(),
            provenance: "RustAnalyzer".into(),
            confidence: 1.0,
            source,
        })
    }
    fn use_cases_for(&self, tests: &[Node]) -> Result<Vec<Value>> {
        let mut output = Vec::new();
        for test in tests {
            let mut statement = self.db.prepare(
                "SELECT u.id, u.name, u.description FROM use_cases u \
                 JOIN use_case_nodes links ON links.use_case_id=u.id WHERE links.node_id=?1",
            )?;
            let rows = statement.query_map([test.id], |row| {
                Ok(json!({"id":row.get::<_, String>(0)?,"name":row.get::<_, String>(1)?,"description":row.get::<_, String>(2)?,"test_symbol":test.canonical_name}))
            })?;
            output.extend(rows.filter_map(Result::ok));
        }
        Ok(output)
    }
    fn source_slice(&self, node: &Node, reason: &str, relation: &str) -> Result<SourceSlice> {
        let path = self.root.join(&node.file);
        let text = fs::read_to_string(&path)?;
        let lines: Vec<_> = text.lines().collect();
        let end = node.end_line.min(lines.len());
        let start = node.start_line.saturating_sub(1);
        let mut source = lines[start..end].join("\n");
        if source.len() > MAX_SLICE_BYTES {
            source.truncate(MAX_SLICE_BYTES)
        }
        Ok(SourceSlice {
            symbol: node.canonical_name.clone(),
            file: node.file.clone(),
            range: [node.start_line, end],
            content_hash: node.content_hash.clone(),
            reason: reason.into(),
            semantic_relationship: relation.into(),
            provenance: "StaticIndex".into(),
            confidence: 0.9,
            source,
        })
    }
    fn decisions_for(&self, text: &str) -> Result<Vec<Value>> {
        let q = format!(
            "%{}%",
            text.split_whitespace().next().unwrap_or("").to_lowercase()
        );
        let mut s=self.db.prepare("SELECT id,status,title,rationale,applies_to,consequences FROM decisions WHERE lower(title) LIKE ?1 OR lower(applies_to) LIKE ?1 ORDER BY sequence DESC LIMIT 20")?;
        Ok(s.query_map([q],|r|Ok(json!({"id":r.get::<_,String>(0)?,"status":r.get::<_,String>(1)?,"title":r.get::<_,String>(2)?,"rationale":r.get::<_,String>(3)?,"applies_to":serde_json::from_str::<Value>(&r.get::<_,String>(4)?).unwrap_or(Value::Null),"consequences":serde_json::from_str::<Value>(&r.get::<_,String>(5)?).unwrap_or(Value::Null)})))?.filter_map(Result::ok).collect())
    }
    fn decision_resource(&self, id: &str) -> Result<Value> {
        Ok(self.db.query_row("SELECT id,status,title,rationale,applies_to,consequences,supersedes,revision,created_at FROM decisions WHERE id=?1",[id],|r|Ok(json!({"id":r.get::<_,String>(0)?,"status":r.get::<_,String>(1)?,"title":r.get::<_,String>(2)?,"rationale":r.get::<_,String>(3)?,"applies_to":serde_json::from_str::<Value>(&r.get::<_,String>(4)?).unwrap_or(Value::Null),"consequences":serde_json::from_str::<Value>(&r.get::<_,String>(5)?).unwrap_or(Value::Null),"supersedes":r.get::<_,Option<String>>(6)?,"revision":r.get::<_,String>(7)?,"created_at":r.get::<_,String>(8)?})))?)
    }
    fn decision_conflicts(&self, diff: &str) -> Result<Vec<Value>> {
        let mut s = self
            .db
            .prepare("SELECT id,title,consequences FROM decisions WHERE status='accepted'")?;
        Ok(s.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?
        .filter_map(Result::ok)
        .filter_map(|(id, title, cs)| {
            let cs: Vec<String> = serde_json::from_str(&cs).unwrap_or_default();
            cs.iter()
                .any(|c| diff.to_lowercase().contains(&c.to_lowercase()))
                .then(|| json!({"decision":id,"title":title,"matched_consequence":cs}))
        })
        .collect())
    }
}

const SCHEMA: &str = "
PRAGMA journal_mode=WAL;
PRAGMA foreign_keys=ON;
CREATE TABLE IF NOT EXISTS metadata(key TEXT PRIMARY KEY,value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS packages(name TEXT PRIMARY KEY,manifest_path TEXT,metadata TEXT,revision TEXT);
CREATE TABLE IF NOT EXISTS package_dependencies(source TEXT,target TEXT,context_json TEXT,revision TEXT,PRIMARY KEY(source,target,context_json));
CREATE TABLE IF NOT EXISTS package_targets(package TEXT NOT NULL,target_name TEXT NOT NULL,kind TEXT NOT NULL,crate_types TEXT NOT NULL,required_features TEXT NOT NULL,edition TEXT NOT NULL,doc INTEGER NOT NULL,doctest INTEGER NOT NULL,test INTEGER NOT NULL,bench INTEGER NOT NULL,revision TEXT NOT NULL,PRIMARY KEY(package,target_name));
CREATE TABLE IF NOT EXISTS package_features(package TEXT NOT NULL,feature TEXT NOT NULL,dependencies TEXT NOT NULL,revision TEXT NOT NULL,PRIMARY KEY(package,feature));
CREATE TABLE IF NOT EXISTS nodes(id INTEGER PRIMARY KEY,kind TEXT NOT NULL,canonical_name TEXT NOT NULL,crate_name TEXT,file TEXT NOT NULL,start_line INTEGER,end_line INTEGER,visibility TEXT,content_hash TEXT,metadata TEXT);
CREATE INDEX IF NOT EXISTS nodes_name ON nodes(canonical_name);
CREATE INDEX IF NOT EXISTS nodes_file ON nodes(file);
CREATE TABLE IF NOT EXISTS edges(id INTEGER PRIMARY KEY,src INTEGER REFERENCES nodes(id) ON DELETE CASCADE,dst INTEGER REFERENCES nodes(id) ON DELETE CASCADE,kind TEXT,context_json TEXT,confidence REAL,provenance TEXT,revision TEXT,metadata TEXT);
CREATE INDEX IF NOT EXISTS edges_src ON edges(src);
CREATE INDEX IF NOT EXISTS edges_dst ON edges(dst);
CREATE TABLE IF NOT EXISTS decisions(id TEXT PRIMARY KEY,sequence INTEGER,status TEXT,title TEXT,rationale TEXT,applies_to TEXT,consequences TEXT,supersedes TEXT,revision TEXT,created_at TEXT);
CREATE TABLE IF NOT EXISTS decision_targets(decision_id TEXT NOT NULL REFERENCES decisions(id) ON DELETE CASCADE,node_id INTEGER REFERENCES nodes(id) ON DELETE SET NULL,target_ref TEXT NOT NULL,PRIMARY KEY(decision_id,target_ref));
CREATE TABLE IF NOT EXISTS documents(id INTEGER PRIMARY KEY,kind TEXT,path TEXT,text TEXT,revision TEXT);
CREATE TABLE IF NOT EXISTS use_cases(id TEXT PRIMARY KEY,name TEXT,description TEXT,provenance TEXT NOT NULL,confidence REAL NOT NULL,revision TEXT);
CREATE TABLE IF NOT EXISTS use_case_nodes(use_case_id TEXT REFERENCES use_cases(id) ON DELETE CASCADE,node_id INTEGER REFERENCES nodes(id) ON DELETE CASCADE,relationship TEXT,provenance TEXT NOT NULL,confidence REAL NOT NULL,revision TEXT,PRIMARY KEY(use_case_id,node_id));
CREATE TABLE IF NOT EXISTS commits(hash TEXT PRIMARY KEY,timestamp TEXT,author TEXT,subject TEXT,revision TEXT);
CREATE TABLE IF NOT EXISTS commit_files(commit_hash TEXT NOT NULL REFERENCES commits(hash) ON DELETE CASCADE,file TEXT NOT NULL,PRIMARY KEY(commit_hash,file));
CREATE INDEX IF NOT EXISTS commit_files_file ON commit_files(file);
CREATE TABLE IF NOT EXISTS co_changes(node_a TEXT,node_b TEXT,count INTEGER,score REAL,PRIMARY KEY(node_a,node_b));
CREATE INDEX IF NOT EXISTS co_changes_pair ON co_changes(node_a,node_b);
CREATE TABLE IF NOT EXISTS lifecycle_edges(id INTEGER PRIMARY KEY,legacy_node INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,canonical_node INTEGER REFERENCES nodes(id) ON DELETE SET NULL,kind TEXT NOT NULL,evidence TEXT NOT NULL,provenance TEXT NOT NULL,confidence REAL NOT NULL,revision TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS lifecycle_legacy ON lifecycle_edges(legacy_node);
CREATE INDEX IF NOT EXISTS lifecycle_canonical ON lifecycle_edges(canonical_node);
CREATE TABLE IF NOT EXISTS work_items(id TEXT PRIMARY KEY,title TEXT NOT NULL,status TEXT NOT NULL,priority TEXT NOT NULL,kind TEXT NOT NULL,scope_json TEXT NOT NULL,evidence_json TEXT NOT NULL,depends_json TEXT NOT NULL,blocked_json TEXT NOT NULL,acceptance_json TEXT NOT NULL,verification_json TEXT NOT NULL,discovered_from TEXT NOT NULL,provenance TEXT NOT NULL,confidence REAL NOT NULL,last_validated_snapshot TEXT NOT NULL,created_at TEXT NOT NULL,updated_at TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS work_status_priority ON work_items(status,priority);
CREATE TABLE IF NOT EXISTS change_contexts(id TEXT PRIMARY KEY,intent TEXT,payload TEXT,revision TEXT,created_at TEXT);
CREATE TABLE IF NOT EXISTS input_state(path TEXT PRIMARY KEY,kind TEXT NOT NULL,content_hash TEXT NOT NULL,revision TEXT NOT NULL);
";

fn initialize_schema(db: &Connection) -> Result<()> {
    db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
    let metadata_exists: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='metadata')",
        [],
        |row| row.get(0),
    )?;
    if metadata_exists {
        let version: Option<String> = db
            .query_row(
                "SELECT value FROM metadata WHERE key='schema_version'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        if version.as_deref() != Some(SCHEMA_VERSION) {
            db.execute_batch("PRAGMA foreign_keys=OFF;
                DROP TABLE IF EXISTS input_state; DROP TABLE IF EXISTS change_contexts;
                DROP TABLE IF EXISTS co_changes; DROP TABLE IF EXISTS commit_files; DROP TABLE IF EXISTS commits;
                DROP TABLE IF EXISTS use_case_nodes; DROP TABLE IF EXISTS use_cases; DROP TABLE IF EXISTS documents;
                DROP TABLE IF EXISTS work_items; DROP TABLE IF EXISTS lifecycle_edges;
                DROP TABLE IF EXISTS decision_targets; DROP TABLE IF EXISTS decisions; DROP TABLE IF EXISTS edges;
                DROP TABLE IF EXISTS nodes; DROP TABLE IF EXISTS package_features; DROP TABLE IF EXISTS package_targets; DROP TABLE IF EXISTS package_dependencies; DROP TABLE IF EXISTS packages;
                DROP TABLE IF EXISTS metadata; PRAGMA foreign_keys=ON;")?;
        }
    }
    db.execute_batch(SCHEMA)?;
    db.execute(
        "INSERT OR REPLACE INTO metadata(key, value) VALUES ('schema_version', ?1)",
        [SCHEMA_VERSION],
    )?;
    Ok(())
}

fn node_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Node> {
    Ok(Node {
        id: row.get(0)?,
        kind: row.get(1)?,
        canonical_name: row.get(2)?,
        crate_name: row.get(3)?,
        file: row.get(4)?,
        start_line: row.get(5)?,
        end_line: row.get(6)?,
        visibility: row.get(7)?,
        content_hash: row.get(8)?,
    })
}
fn work_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    let array = |index| -> rusqlite::Result<Value> {
        Ok(serde_json::from_str::<Value>(&row.get::<_, String>(index)?)
            .unwrap_or_else(|_| json!([])))
    };
    Ok(
        json!({"id":row.get::<_,String>(0)?,"title":row.get::<_,String>(1)?,"status":row.get::<_,String>(2)?,"priority":row.get::<_,String>(3)?,"kind":row.get::<_,String>(4)?,"scope":array(5)?,"evidence":array(6)?,"depends_on":array(7)?,"blocked_by":array(8)?,"acceptance_criteria":array(9)?,"verification":array(10)?,"discovered_from":row.get::<_,String>(11)?,"provenance":row.get::<_,String>(12)?,"confidence":row.get::<_,f64>(13)?,"last_validated_snapshot":row.get::<_,String>(14)?,"updated_at":row.get::<_,String>(15)?}),
    )
}
fn trim_text(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}
fn rust_files(root: &Path) -> Vec<PathBuf> {
    WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| {
            let n = e.file_name().to_string_lossy();
            !matches!(n.as_ref(), "target" | ".git" | INDEX_DIRECTORY)
        })
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file() && e.path().extension().is_some_and(|x| x == "rs"))
        .map(|e| e.into_path())
        .collect()
}
fn all_text_files(root: &Path) -> Vec<PathBuf> {
    WalkDir::new(root)
        .into_iter()
        .filter_entry(|entry| {
            let name = entry.file_name().to_string_lossy();
            !matches!(name.as_ref(), "target" | ".git" | INDEX_DIRECTORY)
        })
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .filter(|entry| {
            entry.path().extension().is_some_and(|extension| {
                matches!(
                    extension.to_string_lossy().as_ref(),
                    "rs" | "md" | "txt" | "toml" | "yaml" | "yml" | "json"
                )
            })
        })
        .map(|entry| entry.into_path())
        .collect()
}
fn index_inputs(root: &Path) -> BTreeMap<String, (String, String)> {
    WalkDir::new(root)
        .into_iter()
        .filter_entry(|entry| {
            let name = entry.file_name().to_string_lossy();
            !matches!(name.as_ref(), "target" | ".git" | INDEX_DIRECTORY)
        })
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .filter_map(|entry| {
            let path = entry.into_path();
            let name = path.file_name()?.to_str()?;
            let config_input = matches!(
                name,
                "Cargo.toml"
                    | "Cargo.lock"
                    | "rust-toolchain"
                    | "rust-toolchain.toml"
                    | "config"
                    | "config.toml"
                    | "build.rs"
            );
            let kind = if config_input {
                "cargo"
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                "source"
            } else if path
                .extension()
                .is_some_and(|extension| extension == "md" || extension == "txt")
            {
                "document"
            } else {
                return None;
            };
            let bytes = fs::read(&path).ok()?;
            Some((
                relative(root, &path),
                (
                    kind.to_owned(),
                    format!("b3:{}", blake3::hash(&bytes).to_hex()),
                ),
            ))
        })
        .collect()
}
fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}
fn command_text(root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}
fn cargo_packages(root: &Path) -> Vec<(String, String, String)> {
    let output = Command::new("cargo")
        .args(["metadata", "--format-version=1", "--no-deps"])
        .current_dir(root)
        .output()
        .ok();
    output
        .and_then(|o| serde_json::from_slice::<Value>(&o.stdout).ok())
        .and_then(|v| v.get("packages").and_then(Value::as_array).cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|p| {
            Some((
                p.get("name")?.as_str()?.to_owned(),
                p.get("manifest_path")?.as_str()?.to_owned(),
                p.to_string(),
            ))
        })
        .collect()
}
fn cargo_dependency_edges(root: &Path) -> Vec<(String, String, String)> {
    let output = Command::new("cargo")
        .args(["metadata", "--format-version=1"])
        .current_dir(root)
        .stderr(Stdio::null())
        .output()
        .ok();
    let Some(metadata) =
        output.and_then(|output| serde_json::from_slice::<Value>(&output.stdout).ok())
    else {
        return Vec::new();
    };
    let names: HashMap<String, String> = metadata
        .get("packages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|package| {
            Some((
                package.get("id")?.as_str()?.to_owned(),
                package.get("name")?.as_str()?.to_owned(),
            ))
        })
        .collect();
    let workspace_members: BTreeSet<String> = metadata
        .get("workspace_members")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    metadata
        .get("resolve")
        .and_then(|resolve| resolve.get("nodes"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|node| {
            node.get("id")
                .and_then(Value::as_str)
                .is_some_and(|id| workspace_members.contains(id))
        })
        .flat_map(|node| {
            let Some(source) = node
                .get("id")
                .and_then(Value::as_str)
                .and_then(|id| names.get(id))
                .cloned()
            else {
                return Vec::new();
            };
            let names = names.clone();
            let features = node.get("features").cloned().unwrap_or_else(|| json!([]));
            node.get("deps")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(move |dependency| {
                    let target = names.get(dependency.get("pkg")?.as_str()?)?.clone();
                    let context = json!({
                        "package": source,
                        "feature_set": features,
                        "cfg_context": dependency.get("dep_kinds").cloned().unwrap_or_else(|| json!([])),
                        "target_triple": null,
                        "dependency": dependency,
                    });
                    Some((source.clone(), target, context.to_string()))
                })
                .collect::<Vec<_>>()
        })
        .collect()
}
fn crate_for_file(packages: &[(String, String, String)], file: &Path) -> Option<String> {
    packages
        .iter()
        .find(|(_, manifest, _)| {
            file.starts_with(Path::new(manifest).parent().unwrap_or(Path::new("")))
        })
        .map(|p| p.0.clone())
}
fn item_end(lines: &[&str], start: usize) -> usize {
    let mut depth = 0i32;
    let mut seen = false;
    for (i, line) in lines.iter().enumerate().skip(start) {
        for c in line.chars() {
            if c == '{' {
                depth += 1;
                seen = true
            } else if c == '}' {
                depth -= 1
            }
        }
        if (seen && depth <= 0) || (!seen && i > start && line.trim().ends_with(';')) {
            return i + 1;
        }
    }
    (start + 1).min(lines.len())
}
fn short_name(name: &str) -> &str {
    name.rsplit("::").next().unwrap_or(name)
}
fn terms(input: &str) -> Vec<String> {
    let re = Regex::new(r"[A-Za-z_][A-Za-z0-9_]*").unwrap();
    let values: Vec<_> = re
        .find_iter(input)
        .map(|m| m.as_str().to_string())
        .collect();
    if values.is_empty() {
        vec!["".into()]
    } else {
        values
    }
}
fn dedupe_nodes(nodes: &mut Vec<Node>) {
    let mut seen = BTreeSet::new();
    nodes.retain(|n| seen.insert(n.id));
}
fn files_for_nodes(a: &[Node], b: &[Node]) -> Vec<String> {
    let files: BTreeSet<String> = a.iter().chain(b).map(|n| n.file.clone()).collect();
    files.into_iter().collect()
}
fn risk(targets: &[Node], references: &[Node]) -> &'static str {
    if targets.iter().any(|n| n.visibility == "public") || references.len() > 12 {
        "high"
    } else if references.len() > 3 {
        "medium"
    } else {
        "low"
    }
}
fn slug(value: &str) -> String {
    value
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|x| !x.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}
fn decision_markdown(id: &str, d: &RecordDecision) -> String {
    format!(
        "# {id}: {}\n\nStatus: {}\n\n## Rationale\n\n{}\n\n## Applies to\n\n{}\n\n## Consequences\n\n{}\n",
        d.title,
        d.status,
        d.reason,
        d.applies_to
            .iter()
            .map(|x| format!("- {x}"))
            .collect::<Vec<_>>()
            .join("\n"),
        d.consequences
            .iter()
            .map(|x| format!("- {x}"))
            .collect::<Vec<_>>()
            .join("\n")
    )
}
fn check(root: &Path, args: &[&str]) -> Value {
    let out = Command::new("cargo").args(args).current_dir(root).output();
    match out {
        Ok(o) => {
            json!({"command":format!("cargo {}",args.join(" ")),"success":o.status.success(),"output":String::from_utf8_lossy(&o.stderr)})
        }
        Err(e) => {
            json!({"command":format!("cargo {}",args.join(" ")),"success":false,"error":e.to_string()})
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    fn fixture() -> tempfile::TempDir {
        let d = tempdir().unwrap();
        fs::write(
            d.path().join("Cargo.toml"),
            "[package]\nname='demo'\nversion='0.1.0'\nedition='2024'\n",
        )
        .unwrap();
        fs::create_dir(d.path().join("src")).unwrap();
        fs::write(
            d.path().join("src/lib.rs"),
            "pub trait Store { fn load(&self); }\nfn uses(s: &dyn Store) { s.load(); }\n",
        )
        .unwrap();
        d
    }

    #[test]
    fn rust_analyzer_uses_bounded_background_resources() {
        let options = rust_analyzer_initialization_options();
        assert_eq!(options["numThreads"], RUST_ANALYZER_THREADS);
        assert_eq!(options["cachePriming"]["enable"], false);
        assert_eq!(
            options["cargo"]["extraEnv"]["CARGO_BUILD_JOBS"],
            RUST_ANALYZER_THREADS.to_string()
        );
        assert_eq!(options["checkOnSave"], false);
    }

    #[test]
    fn indexes_and_prepares_context() {
        let d = fixture();
        let mut service = Service::open(d.path()).unwrap();
        service.refresh_if_stale().unwrap();
        let v = service
            .prepare_change("change Store", &["Store".into()], 2, None)
            .unwrap();
        assert!(!v["primary_symbols"].as_array().unwrap().is_empty());
    }

    #[test]
    fn decisions_and_change_contexts_are_persistent_resources() {
        let d = fixture();
        let mut service = Service::open(d.path()).unwrap();
        service.refresh_if_stale().unwrap();
        let decision = service
            .record_decision(RecordDecision {
                title: "Keep Store injected".into(),
                status: "accepted".into(),
                reason: "Permits isolated testing".into(),
                applies_to: vec!["Store".into()],
                consequences: vec!["Do not construct stores in handlers".into()],
                supersedes: None,
                materialize: false,
            })
            .unwrap();
        let decision_id = decision["id"].as_str().unwrap();
        assert_eq!(
            service
                .resource(&format!("rustrepo://decision/{decision_id}"))
                .unwrap()["title"],
            "Keep Store injected"
        );
        let context = service
            .prepare_change("change Store", &["Store".into()], 1, Some(1_000))
            .unwrap();
        let context_id = context["context_id"].as_str().unwrap();
        assert_eq!(
            service
                .resource(&format!("rustrepo://change/{context_id}"))
                .unwrap()["context_id"],
            context_id
        );
    }

    #[test]
    fn source_changes_refresh_symbols_without_rebuilding_decisions() {
        let d = fixture();
        let mut service = Service::open(d.path()).unwrap();
        let decision = service
            .record_decision(RecordDecision {
                title: "Keep Store injected".into(),
                status: "accepted".into(),
                reason: "Permits isolated testing".into(),
                applies_to: vec!["Store".into()],
                consequences: vec![],
                supersedes: None,
                materialize: false,
            })
            .unwrap();
        fs::write(
            d.path().join("src/lib.rs"),
            "pub trait Store { fn load(&self); }\npub fn replacement() {}\n",
        )
        .unwrap();
        service.refresh_if_stale().unwrap();
        assert!(
            !service
                .search_nodes(&terms("replacement"), 1)
                .unwrap()
                .is_empty()
        );
        assert!(
            service
                .resource(&format!(
                    "rustrepo://decision/{}",
                    decision["id"].as_str().unwrap()
                ))
                .is_ok()
        );
        let linked_node: Option<i64> = service
            .db
            .query_row(
                "SELECT node_id FROM decision_targets WHERE decision_id=?1 AND target_ref='Store'",
                [decision["id"].as_str().unwrap()],
                |row| row.get(0),
            )
            .unwrap();
        assert!(linked_node.is_some());
    }

    #[test]
    fn cache_schema_enables_foreign_keys_and_required_tables() {
        let d = fixture();
        let service = Service::open(d.path()).unwrap();
        let foreign_keys: i64 = service
            .db
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .unwrap();
        assert_eq!(foreign_keys, 1);
        let tables: BTreeSet<String> = service
            .db
            .prepare("SELECT name FROM sqlite_master WHERE type='table'")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        for table in [
            "metadata",
            "packages",
            "package_dependencies",
            "package_targets",
            "package_features",
            "nodes",
            "edges",
            "decisions",
            "decision_targets",
            "commits",
            "commit_files",
            "co_changes",
            "use_cases",
            "use_case_nodes",
            "change_contexts",
        ] {
            assert!(tables.contains(table), "missing table: {table}");
        }
    }

    #[test]
    fn indexes_cargo_feature_target_matrix() {
        let d = fixture();
        let mut service = Service::open(d.path()).unwrap();
        service.refresh_if_stale().unwrap();
        let matrix = service.matrix().unwrap();
        assert!(!matrix["targets"].as_array().unwrap().is_empty());
        assert!(matrix["targets"][0]["package"].is_string());
        assert!(
            service.status().unwrap()["counts"]["package_targets"]
                .as_i64()
                .unwrap()
                > 0
        );
    }

    #[test]
    fn finds_referenced_but_explicitly_superseded_compatibility_path() {
        let d = fixture();
        fs::write(
            d.path().join("src/lib.rs"),
            "pub fn canonical_store() {}\n\n/// Legacy adapter replaced with canonical_store.\npub fn legacy_store() { canonical_store(); }\n\npub fn consumer() { legacy_store(); }\n// TODO: remove the compatibility adapter after external clients migrate\n",
        )
        .unwrap();
        let mut service = Service::open(d.path()).unwrap();
        service.refresh_if_stale().unwrap();
        let candidates = service
            .obsolete_candidates(Some("legacy_store"), 10)
            .unwrap();
        let candidate = candidates["candidates"]
            .as_array()
            .unwrap()
            .first()
            .unwrap();
        assert_eq!(candidate["classification"], "architecturally-superseded");
        assert_eq!(candidate["replacement"], "src::lib::canonical_store");
        assert!(!candidate["current_callers"].as_array().unwrap().is_empty());
        let work = service
            .work_list(Some("compatibility adapter"), 10)
            .unwrap();
        assert_eq!(work["items"][0]["status"], "proposed");
        assert_eq!(work["items"][0]["provenance"], "SourceDoc");
    }

    #[test]
    fn explicit_work_is_editable_and_snapshot_aware() {
        let d = fixture();
        let mut service = Service::open(d.path()).unwrap();
        service.refresh_if_stale().unwrap();
        let proposed = service
            .work_propose(WorkItemInput {
                title: "Audit public Store callers".into(),
                status: "accepted".into(),
                priority: "high".into(),
                kind: "test_gap".into(),
                scope: vec!["Store".into()],
                evidence: vec!["integration test is missing".into()],
                depends_on: vec![],
                blocked_by: vec![],
                acceptance_criteria: vec!["public callers are covered".into()],
                verification: vec!["cargo test".into()],
            })
            .unwrap();
        let id = proposed["id"].as_str().unwrap();
        service
            .work_update(id, &json!({"status":"in_progress"}))
            .unwrap();
        assert_eq!(service.work_next(None).unwrap()["next"]["id"], id);
        let snapshot = &service.status().unwrap()["snapshot"];
        assert!(snapshot["revision"].is_object());
        assert!(snapshot["indexing_timestamp"].is_string());
    }

    #[test]
    fn validation_reads_all_unified_diff_file_headers() {
        let d = fixture();
        let mut service = Service::open(d.path()).unwrap();
        service.refresh_if_stale().unwrap();
        let context = service
            .prepare_change("change Store", &["Store".into()], 1, Some(1_000))
            .unwrap();
        let validated = service
            .validate_change(
                context["context_id"].as_str().unwrap(),
                Some("+++ b/src/lib.rs\n+++ b/Cargo.toml\n"),
                false,
            )
            .unwrap();
        let changed = validated["changed_files"].as_array().unwrap();
        assert!(changed.iter().any(|path| path == "src/lib.rs"));
        assert!(changed.iter().any(|path| path == "Cargo.toml"));
    }

    #[test]
    fn removed_todo_evidence_is_retained_as_stale_proposed_work() {
        let d = fixture();
        fs::write(
            d.path().join("src/lib.rs"),
            "pub trait Store { fn load(&self); }\n// TODO: support a third storage backend\n",
        )
        .unwrap();
        let mut service = Service::open(d.path()).unwrap();
        service.refresh_if_stale().unwrap();
        let before = service
            .work_list(Some("support a third storage"), 10)
            .unwrap();
        assert_eq!(before["items"][0]["confidence"], 0.7);
        fs::write(
            d.path().join("src/lib.rs"),
            "pub trait Store { fn load(&self); }\n",
        )
        .unwrap();
        service.refresh_if_stale().unwrap();
        let after = service
            .work_list(Some("support a third storage"), 10)
            .unwrap();
        assert_eq!(after["items"][0]["status"], "proposed");
        assert_eq!(after["items"][0]["confidence"], 0.2);
        assert!(
            after["items"][0]["evidence"][0]
                .as_str()
                .unwrap()
                .contains("absent")
        );
    }
}

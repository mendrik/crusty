//! Contextual architecture analysis for Rust workspaces.
//!
//! Facts are versioned observations from manifests and syntax trees. Findings
//! are deterministic interpretations which include their evidence,
//! counter-evidence, confidence, and known limitations. They are advisory until
//! a human promotes a finding through Crusty's existing finding lifecycle.

use anyhow::Result;
use chrono::Utc;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};
use syn::{
    Block, Expr, ExprAsync, ExprAwait, ExprCall, ExprForLoop, ExprLoop, ExprMethodCall, ExprUnsafe,
    ExprWhile, Fields, FnArg, ImplItemFn, Item, ItemFn, ItemStruct, Pat, Stmt, Type, Visibility,
    spanned::Spanned,
    visit::{self, Visit},
};
use walkdir::WalkDir;

pub const FACT_SCHEMA_VERSION: &str = "architecture-fact-v1";
pub const ANALYZER_VERSION: &str = "rustitect-guard-v1";
const MAX_RUST_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ArchitectureLocation {
    pub file: String,
    pub line: usize,
    pub end_line: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ArchitectureFact {
    pub id: String,
    pub kind: String,
    pub subject: String,
    pub location: ArchitectureLocation,
    pub summary: String,
    pub provenance: String,
    pub confidence: f64,
    pub profile: String,
    pub details: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ArchitectureFinding {
    pub id: String,
    pub rule_id: String,
    pub title: String,
    pub category: String,
    pub severity: String,
    pub confidence: f64,
    pub subject: String,
    pub summary: String,
    pub locations: Vec<ArchitectureLocation>,
    pub occurrence_count: usize,
    pub evidence: Vec<String>,
    pub counter_evidence: Vec<String>,
    pub recommendation: String,
    pub limitations: Vec<String>,
    pub origin: String,
}

/// Compact change-context representation. Rich evidence remains in audit
/// reports; a prepared context needs only stable identity and delta metrics.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ArchitectureFindingBaseline {
    pub id: String,
    pub rule_id: String,
    pub title: String,
    pub severity: String,
    pub subject: String,
    pub locations: Vec<ArchitectureLocation>,
    pub occurrence_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ArchitectureComponent {
    pub path: String,
    pub role: String,
    pub public_symbols: usize,
    pub uses: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ArchitectureMap {
    pub rust_files: usize,
    pub manifests: Vec<String>,
    pub manifest_count: usize,
    pub manifests_truncated: bool,
    pub dependency_declarations: usize,
    pub components: Vec<ArchitectureComponent>,
    pub component_count: usize,
    pub components_truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ArchitectureReport {
    pub id: String,
    pub fact_schema_version: String,
    pub analyzer_version: String,
    pub revision: Value,
    pub profile: String,
    pub scope: Option<String>,
    pub generated_at: String,
    pub architecture: ArchitectureMap,
    pub facts: Vec<ArchitectureFact>,
    pub findings: Vec<ArchitectureFinding>,
    pub summary: Value,
    pub limitations: Vec<String>,
    pub authority: String,
}

#[derive(Debug, Clone)]
struct Candidate {
    kind: &'static str,
    subject: String,
    location: ArchitectureLocation,
    summary: String,
    details: Value,
}

struct ParsedFile {
    relative: String,
    source: String,
    role: String,
    uses: Vec<String>,
    public_symbols: usize,
    candidates: Vec<Candidate>,
    state_structs: Vec<StateStruct>,
    public_functions: Vec<PublicFunction>,
    serde_domain_types: Vec<NamedLocation>,
}

#[derive(Debug)]
struct StateStruct {
    name: String,
    location: ArchitectureLocation,
    bool_fields: Vec<String>,
    state_fields: Vec<String>,
}

#[derive(Debug)]
struct PublicFunction {
    name: String,
    location: ArchitectureLocation,
    signature: String,
    string_ids: Vec<String>,
}

#[derive(Debug, Clone)]
struct NamedLocation {
    name: String,
    location: ArchitectureLocation,
}

struct ManifestScan {
    manifests: Vec<String>,
    dependency_declarations: usize,
    facts: Vec<ArchitectureFact>,
    findings: Vec<ArchitectureFinding>,
    limitations: Vec<String>,
}

pub fn analyze(
    root: &Path,
    revision: Value,
    scope: Option<&str>,
    max_findings: usize,
) -> Result<ArchitectureReport> {
    let profile = active_profile();
    let mut parsed = Vec::new();
    let (rust_files, mut limitations) = rust_files(root);
    for path in rust_files {
        let relative = relative(root, &path);
        if fs::metadata(&path).is_ok_and(|metadata| metadata.len() > MAX_RUST_FILE_BYTES) {
            limitations.push(format!(
                "Skipped {relative} because it exceeds the {MAX_RUST_FILE_BYTES}-byte per-file analysis budget."
            ));
            continue;
        }
        let source = match fs::read_to_string(&path) {
            Ok(source) => source,
            Err(error) => {
                limitations.push(format!("Could not read {relative}: {error}"));
                continue;
            }
        };
        match syn::parse_file(&source) {
            Ok(syntax) => parsed.push(parse_file(relative, source, syntax)),
            Err(error) => limitations.push(format!(
                "Could not parse {relative}; facts from this file are absent: {error}"
            )),
        }
    }

    let manifest_scan = scan_manifests(root, &profile)?;
    limitations.extend(manifest_scan.limitations);
    let mut facts = build_facts(&parsed, &profile);
    facts.extend(manifest_scan.facts);
    let mut findings = contextual_findings(&parsed);
    findings.extend(manifest_scan.findings);
    findings.extend(configuration_scatter(&parsed));
    findings.extend(serialization_coupling(&parsed));
    findings = coalesce_findings(findings);

    if let Some(scope) = scope.filter(|scope| !scope.trim().is_empty()) {
        let scope = scope.to_ascii_lowercase();
        facts.retain(|fact| {
            fact.subject.to_ascii_lowercase().contains(&scope)
                || fact.location.file.to_ascii_lowercase().contains(&scope)
                || fact.summary.to_ascii_lowercase().contains(&scope)
        });
        findings.retain(|finding| finding_matches_scope(finding, &scope));
    }
    let observed_fact_count = facts.len();
    let detected_finding_count = findings.len();
    let max_facts = max_findings.saturating_mul(10).clamp(50, 500);
    facts.truncate(max_facts);
    findings.sort_by(|left, right| {
        severity_rank(&right.severity)
            .cmp(&severity_rank(&left.severity))
            .then_with(|| {
                right
                    .confidence
                    .partial_cmp(&left.confidence)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| left.id.cmp(&right.id))
    });
    findings.truncate(max_findings.clamp(1, 1_000));

    let mut components = parsed
        .iter()
        .filter(|file| {
            scope.is_none_or(|scope| {
                file.relative
                    .to_ascii_lowercase()
                    .contains(&scope.to_ascii_lowercase())
                    || file
                        .source
                        .to_ascii_lowercase()
                        .contains(&scope.to_ascii_lowercase())
            })
        })
        .map(|file| ArchitectureComponent {
            path: file.relative.clone(),
            role: file.role.clone(),
            public_symbols: file.public_symbols,
            uses: file.uses.clone(),
        })
        .collect::<Vec<_>>();
    let component_count = components.len();
    components.truncate(max_findings.saturating_mul(5).clamp(25, 200));
    let manifest_count = manifest_scan.manifests.len();
    let mut manifests = manifest_scan.manifests;
    manifests.truncate(200);
    let architecture = ArchitectureMap {
        rust_files: parsed.len(),
        manifests,
        manifest_count,
        manifests_truncated: manifest_count > 200,
        dependency_declarations: manifest_scan.dependency_declarations,
        components,
        component_count,
        components_truncated: component_count > max_findings.saturating_mul(5).clamp(25, 200),
    };
    let report_key = format!(
        "{}:{}:{}:{}:{}",
        revision,
        profile,
        scope.unwrap_or("workspace"),
        max_findings.clamp(1, 1_000),
        ANALYZER_VERSION
    );
    let id = format!(
        "arch_{}",
        &blake3::hash(report_key.as_bytes()).to_hex()[..12]
    );
    let summary = finding_summary(
        &findings,
        detected_finding_count,
        facts.len(),
        observed_fact_count,
    );
    limitations.extend([
        "Static syntax cannot prove runtime registration, cancellation behavior, external-consumer compatibility, or deployment behavior.".to_owned(),
        "Type and call relationships which require name resolution remain candidates until rust-analyzer or the compiler confirms them.".to_owned(),
        "Only new or worsened findings become change-delta concerns; existing findings remain visible baseline debt.".to_owned(),
    ]);
    Ok(ArchitectureReport {
        id,
        fact_schema_version: FACT_SCHEMA_VERSION.to_owned(),
        analyzer_version: ANALYZER_VERSION.to_owned(),
        revision,
        profile,
        scope: scope.map(str::to_owned),
        generated_at: Utc::now().to_rfc3339(),
        architecture,
        facts,
        findings,
        summary,
        limitations,
        authority: "Advisory static analysis. Current source, compiler/runtime behavior, and human decisions remain authoritative.".to_owned(),
    })
}

pub fn delta(
    baseline: &[ArchitectureFindingBaseline],
    current: &[ArchitectureFinding],
    changed_files: &BTreeSet<String>,
) -> Value {
    let before = baseline
        .iter()
        .map(|finding| (finding.id.as_str(), finding))
        .collect::<BTreeMap<_, _>>();
    let after = current
        .iter()
        .map(|finding| (finding.id.as_str(), finding))
        .collect::<BTreeMap<_, _>>();
    let current_touches_change = |finding: &&ArchitectureFinding| {
        finding
            .locations
            .iter()
            .any(|location| changed_files.contains(&location.file))
    };
    let baseline_touches_change = |finding: &&ArchitectureFindingBaseline| {
        finding
            .locations
            .iter()
            .any(|location| changed_files.contains(&location.file))
    };
    let new = after
        .iter()
        .filter(|(id, finding)| !before.contains_key(**id) && current_touches_change(finding))
        .map(|(_, finding)| *finding)
        .collect::<Vec<_>>();
    let resolved = before
        .iter()
        .filter(|(id, finding)| !after.contains_key(**id) && baseline_touches_change(finding))
        .map(|(_, finding)| *finding)
        .collect::<Vec<_>>();
    let worsened = after
        .iter()
        .filter_map(|(id, finding)| {
            let previous = before.get(id)?;
            let worse = severity_rank(&finding.severity) > severity_rank(&previous.severity)
                || finding.occurrence_count > previous.occurrence_count;
            (worse && current_touches_change(finding)).then_some(*finding)
        })
        .collect::<Vec<_>>();
    json!({
        "policy":"advisory_only",
        "baseline_count":baseline.len(),
        "current_count":current.len(),
        "new":new,
        "worsened":worsened,
        "resolved":resolved,
        "existing_count":after.keys().filter(|id| before.contains_key(**id)).count(),
        "attention_required":!new.is_empty() || !worsened.is_empty(),
        "note":"Inferred architecture findings never block a change. Only separately human-approved quality constraints may block validation."
    })
}

pub fn baseline(findings: &[ArchitectureFinding]) -> Vec<ArchitectureFindingBaseline> {
    findings
        .iter()
        .map(|finding| ArchitectureFindingBaseline {
            id: finding.id.clone(),
            rule_id: finding.rule_id.clone(),
            title: finding.title.clone(),
            severity: finding.severity.clone(),
            subject: finding.subject.clone(),
            locations: finding.locations.clone(),
            occurrence_count: finding.occurrence_count,
        })
        .collect()
}

fn parse_file(relative: String, source: String, syntax: syn::File) -> ParsedFile {
    let uses = use_roots(&source);
    let role = infer_role_with_dependencies(&relative, &uses).to_owned();
    let public_symbols = syntax
        .items
        .iter()
        .filter(|item| item_is_public(item))
        .count();
    let candidates = {
        let mut visitor = SignalVisitor::new(&relative, &source);
        visitor.visit_file(&syntax);
        visitor.candidates
    };
    let public_types = syntax
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Enum(item) if is_public(&item.vis) => Some(item.ident.to_string()),
            Item::Struct(item) if is_public(&item.vis) => Some(item.ident.to_string()),
            Item::Type(item) if is_public(&item.vis) => Some(item.ident.to_string()),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    let mut state_structs = Vec::new();
    let mut public_functions = Vec::new();
    let mut serde_domain_types = Vec::new();
    for item in &syntax.items {
        match item {
            Item::Struct(item) => {
                if let Some(state) = state_struct(&relative, item) {
                    state_structs.push(state);
                }
                if is_domain_role(&role) && has_serde_derive(&item.attrs) {
                    serde_domain_types.push(NamedLocation {
                        name: item.ident.to_string(),
                        location: location(&relative, item.span()),
                    });
                }
            }
            Item::Enum(item) if is_domain_role(&role) && has_serde_derive(&item.attrs) => {
                serde_domain_types.push(NamedLocation {
                    name: item.ident.to_string(),
                    location: location(&relative, item.span()),
                });
            }
            Item::Fn(item) if is_public(&item.vis) => {
                public_functions.push(public_function(&relative, &source, item));
            }
            Item::Impl(item) => {
                let publicly_reachable =
                    self_type_name(&item.self_ty).is_some_and(|name| public_types.contains(&name));
                for child in &item.items {
                    if let syn::ImplItem::Fn(method) = child
                        && is_public(&method.vis)
                        && publicly_reachable
                    {
                        public_functions.push(public_method(&relative, &source, method));
                    }
                }
            }
            _ => {}
        }
    }
    ParsedFile {
        relative,
        source,
        role,
        uses,
        public_symbols,
        candidates,
        state_structs,
        public_functions,
        serde_domain_types,
    }
}

fn build_facts(files: &[ParsedFile], profile: &str) -> Vec<ArchitectureFact> {
    let mut facts = Vec::new();
    for file in files {
        for candidate in &file.candidates {
            facts.push(fact(candidate, profile, "SyntaxTree", 0.95));
        }
        for state in &file.state_structs {
            let candidate = Candidate {
                kind: "state_representation",
                subject: state.name.clone(),
                location: state.location.clone(),
                summary: format!(
                    "{} stores {} boolean fields, including state-like fields {}",
                    state.name,
                    state.bool_fields.len(),
                    state.state_fields.join(", ")
                ),
                details: json!({"bool_fields":state.bool_fields,"state_fields":state.state_fields}),
            };
            facts.push(fact(&candidate, profile, "SyntaxTree", 1.0));
        }
        for function in &file.public_functions {
            let candidate = Candidate {
                kind: "public_signature",
                subject: function.name.clone(),
                location: function.location.clone(),
                summary: function.signature.clone(),
                details: json!({"string_id_parameters":function.string_ids}),
            };
            facts.push(fact(&candidate, profile, "SyntaxTree", 0.98));
        }
    }
    facts
}

fn contextual_findings(files: &[ParsedFile]) -> Vec<ArchitectureFinding> {
    let mut findings = Vec::new();
    for file in files {
        for candidate in &file.candidates {
            match candidate.kind {
                "unsafe_without_contract" => findings.push(finding(
                    "unsafe-contract",
                    "Unsafe code has no local safety contract",
                    "correctness",
                    "high",
                    0.97,
                    &candidate.subject,
                    "An unsafe function or block lacks a nearby `SAFETY:` justification or `# Safety` contract.",
                    vec![candidate.location.clone()],
                    1,
                    vec![candidate.summary.clone()],
                    vec!["No nearby safety contract was found in the source text.".to_owned()],
                    "Document the invariants which make this unsafe operation sound, then add a focused test or Miri target for the boundary.",
                    vec!["A distant module-level safety explanation may exist and requires human review.".to_owned()],
                )),
                "unbounded_spawn_loop" => findings.push(finding(
                    "unbounded-spawn-loop",
                    "Task spawning appears unbounded",
                    "concurrency",
                    "high",
                    0.91,
                    &candidate.subject,
                    "A task is spawned inside a loop without visible local concurrency control.",
                    vec![candidate.location.clone()],
                    1,
                    vec![candidate.summary.clone()],
                    vec!["No Semaphore, JoinSet, FuturesUnordered, buffer_unordered, or for_each_concurrent guard was visible in the containing function.".to_owned()],
                    "Use structured concurrency and make the concurrency bound, cancellation, and error propagation explicit.",
                    vec!["A bound enforced by a caller or custom wrapper is not visible to syntax-only analysis.".to_owned()],
                )),
                "detached_task" => findings.push(finding(
                    "detached-task",
                    "Spawned task is detached from supervision",
                    "concurrency",
                    "medium",
                    0.94,
                    &candidate.subject,
                    "A spawn handle is immediately discarded, so failure, cancellation, and shutdown ownership are unclear.",
                    vec![candidate.location.clone()],
                    1,
                    vec![candidate.summary.clone()],
                    vec!["The source line does not retain or await the task handle.".to_owned()],
                    "Retain the handle in an owning task set, await it, or document the daemon lifecycle and failure policy.",
                    vec!["Intentionally process-lifetime background tasks can be valid after explicit lifecycle review.".to_owned()],
                )),
                "blocking_in_async" => findings.push(finding(
                    "blocking-in-async",
                    "Blocking operation runs in async context",
                    "concurrency",
                    "high",
                    0.9,
                    &candidate.subject,
                    "A known blocking standard-library operation is called from async code outside a visible blocking worker.",
                    vec![candidate.location.clone()],
                    1,
                    vec![candidate.summary.clone()],
                    vec!["The call is not nested beneath spawn_blocking or block_in_place in this syntax tree.".to_owned()],
                    "Move blocking work behind spawn_blocking or use an async API, and preserve cancellation/error semantics.",
                    vec!["Small, bounded operations may be acceptable after latency measurement.".to_owned()],
                )),
                "sync_guard_across_await" => findings.push(finding(
                    "sync-guard-across-await",
                    "Synchronous lock guard appears live across an await",
                    "concurrency",
                    "high",
                    0.93,
                    &candidate.subject,
                    "A guard acquired from a synchronous mutex or read/write lock remains in scope when execution yields.",
                    vec![candidate.location.clone()],
                    1,
                    vec![candidate.summary.clone()],
                    vec!["The file imports std::sync or parking_lot locking types, the acquisition is not awaited, and no explicit drop appears before the await.".to_owned()],
                    "Keep the critical section synchronous and drop the guard before awaiting, or redesign ownership/message passing so async suspension cannot hold the lock.",
                    vec!["A guard can be released through control flow or a helper that syntax-only statement analysis does not model.".to_owned()],
                )),
                "discarded_error" => findings.push(finding(
                    "discarded-error",
                    "Failure is explicitly discarded",
                    "error_semantics",
                    "medium",
                    0.95,
                    &candidate.subject,
                    "Calling `.ok()` without using the returned Option erases a failure path.",
                    vec![candidate.location.clone()],
                    1,
                    vec![candidate.summary.clone()],
                    vec!["No local log, metric, comment, or returned value explains the discard.".to_owned()],
                    "Handle or propagate the error; if best-effort behavior is intentional, record observable context and document the policy.",
                    vec!["Syntax analysis cannot infer whether the discarded result is deliberately non-critical.".to_owned()],
                )),
                _ => {}
            }
        }

        for state in &file.state_structs {
            if state.bool_fields.len() >= 3 && state.state_fields.len() >= 2 {
                findings.push(finding(
                    "boolean-state-cluster",
                    "Boolean fields encode an implicit state machine",
                    "invariants",
                    "medium",
                    0.88,
                    &state.name,
                    "Several state-like booleans can represent contradictory combinations which the type system does not exclude.",
                    vec![state.location.clone()],
                    state.state_fields.len(),
                    vec![format!("State-like fields: {}", state.state_fields.join(", "))],
                    vec!["Configuration, options, flags, and request/response carrier types are excluded.".to_owned()],
                    "Model mutually exclusive states with an enum or validated state type, and keep transitions behind methods.",
                    vec!["Independent booleans can be correct; confirm whether the fields are truly coupled before refactoring.".to_owned()],
                ));
            }
        }

        let opaque = file
            .public_functions
            .iter()
            .filter(|function| exposes_opaque_error(&file.source, &function.signature))
            .collect::<Vec<_>>();
        if file.relative != "src/main.rs"
            && !file.relative.starts_with("src/bin/")
            && opaque.len() >= 2
        {
            findings.push(finding(
                "opaque-library-errors",
                "Public library boundary exposes opaque errors",
                "error_semantics",
                "medium",
                0.9,
                &file.relative,
                "Multiple public functions expose anyhow or boxed dynamic errors, leaving callers without stable error categories.",
                opaque
                    .iter()
                    .take(8)
                    .map(|function| function.location.clone())
                    .collect(),
                opaque.len(),
                vec![format!(
                    "{} public functions in this module use an opaque Result boundary.",
                    opaque.len()
                )],
                vec!["Binary and command entry points are excluded because contextual errors are appropriate at an application boundary.".to_owned()],
                "Define a stable domain-facing error enum and add context only when translating into the application/transport boundary.",
                vec!["An internal-only module may not need a public compatibility contract even when items are declared pub.".to_owned()],
            ));
        }

        if is_domain_role(&file.role) {
            for function in &file.public_functions {
                if !function.string_ids.is_empty() {
                    findings.push(finding(
                        "primitive-domain-id",
                        "Domain identifier is represented as a string",
                        "invariants",
                        "medium",
                        0.86,
                        &function.name,
                        "A public domain-facing operation accepts identifier-shaped String or &str parameters, allowing unrelated identifiers to be mixed.",
                        vec![function.location.clone()],
                        function.string_ids.len(),
                        vec![format!("String identifier parameters: {}", function.string_ids.join(", "))],
                        vec!["Only files inferred as domain/core/model code are considered.".to_owned()],
                        "Introduce semantic identifier newtypes at the domain boundary and parse transport strings in an adapter.",
                        vec!["Generic lookup/search APIs may intentionally accept untyped text.".to_owned()],
                    ));
                }
                let leaked = [
                    "axum::",
                    "rmcp::",
                    "rusqlite::",
                    "sqlx::",
                    "diesel::",
                    "tonic::",
                ]
                .into_iter()
                .filter(|dependency| function.signature.contains(dependency))
                .collect::<Vec<_>>();
                if !leaked.is_empty() {
                    findings.push(finding(
                        "boundary-representation-leak",
                        "Infrastructure type leaks into a domain boundary",
                        "boundaries",
                        "medium",
                        0.92,
                        &function.name,
                        "A public domain-facing signature directly exposes a transport or persistence representation.",
                        vec![function.location.clone()],
                        leaked.len(),
                        vec![format!("Exposed dependencies: {}", leaked.join(", "))],
                        vec!["Files inferred as adapters, transport, CLI, or persistence code are excluded.".to_owned()],
                        "Translate at the boundary and keep the domain signature expressed in domain-owned types.",
                        vec!["The path-based role inference should be confirmed against the repository's documented architecture.".to_owned()],
                    ));
                }
            }
        }
    }
    findings
}

fn configuration_scatter(files: &[ParsedFile]) -> Vec<ArchitectureFinding> {
    let locations = files
        .iter()
        .flat_map(|file| {
            file.candidates
                .iter()
                .filter(|candidate| candidate.kind == "environment_read")
                .map(|candidate| candidate.location.clone())
        })
        .collect::<Vec<_>>();
    let modules = locations
        .iter()
        .map(|location| location.file.as_str())
        .collect::<BTreeSet<_>>();
    let central_only = modules
        .iter()
        .all(|file| infer_role(file) == "configuration");
    if modules.len() < 3 || central_only {
        return Vec::new();
    }
    vec![finding(
        "configuration-scatter",
        "Environment configuration is read across multiple modules",
        "boundaries",
        "medium",
        0.93,
        "workspace configuration",
        "Direct environment reads are distributed across the codebase, so validation, defaults, and test setup can diverge.",
        locations.iter().take(12).cloned().collect(),
        locations.len(),
        vec![format!(
            "Environment reads occur in {} files.",
            modules.len()
        )],
        vec![
            "A repository whose reads are confined to config/settings modules is not reported."
                .to_owned(),
        ],
        "Load and validate configuration once into a typed object, then inject it into consumers.",
        vec![
            "Build scripts and tests may intentionally read environment variables independently."
                .to_owned(),
        ],
    )]
}

fn serialization_coupling(files: &[ParsedFile]) -> Vec<ArchitectureFinding> {
    let mut findings = Vec::new();
    for domain in files {
        for named in &domain.serde_domain_types {
            let boundary = files
                .iter()
                .filter(|file| matches!(file.role.as_str(), "transport" | "adapter"))
                .filter(|file| word_occurrences(&file.source, &named.name) > 0)
                .map(|file| file.relative.clone())
                .collect::<Vec<_>>();
            let storage = files
                .iter()
                .filter(|file| file.role == "persistence")
                .filter(|file| word_occurrences(&file.source, &named.name) > 0)
                .map(|file| file.relative.clone())
                .collect::<Vec<_>>();
            if !boundary.is_empty() && !storage.is_empty() {
                findings.push(finding(
                    "cross-boundary-serialization",
                    "One domain type appears to serve transport and persistence",
                    "boundaries",
                    "medium",
                    0.8,
                    &named.name,
                    "A serde-derived domain type is referenced from both boundary and persistence modules, coupling independent representations.",
                    vec![named.location.clone()],
                    boundary.len() + storage.len(),
                    vec![
                        format!("Boundary references: {}", boundary.join(", ")),
                        format!("Persistence references: {}", storage.join(", ")),
                    ],
                    vec!["The finding requires both role categories and a serde derive; a derive alone is never treated as a problem.".to_owned()],
                    "Give transport, persistence, and domain models distinct ownership and explicit conversions where their evolution differs.",
                    vec!["Name-based references require semantic confirmation before refactoring.".to_owned()],
                ));
            }
        }
    }
    findings
}

fn scan_manifests(root: &Path, profile: &str) -> Result<ManifestScan> {
    let dependency = Regex::new(r#"^\s*[A-Za-z0-9_-]+\s*=\s*"#)?;
    let wildcard = Regex::new(r#"^\s*([A-Za-z0-9_-]+)\s*=\s*["{][^\n]*\*"#)?;
    let mut manifests = Vec::new();
    let mut declarations = 0;
    let mut facts = Vec::new();
    let mut findings = Vec::new();
    let mut limitations = Vec::new();
    for entry in WalkDir::new(root).into_iter().filter_entry(allowed_entry) {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                limitations.push(format!("Could not traverse a manifest path: {error}"));
                continue;
            }
        };
        if !entry.file_type().is_file() || entry.file_name() != "Cargo.toml" {
            continue;
        }
        let path = entry.path();
        let relative = relative(root, path);
        if fs::metadata(path).is_ok_and(|metadata| metadata.len() > MAX_MANIFEST_BYTES) {
            limitations.push(format!(
                "Skipped {relative} because it exceeds the {MAX_MANIFEST_BYTES}-byte manifest budget."
            ));
            continue;
        }
        let source = match fs::read_to_string(path) {
            Ok(source) => source,
            Err(error) => {
                limitations.push(format!("Could not read manifest {relative}: {error}"));
                continue;
            }
        };
        manifests.push(relative.clone());
        let mut in_dependencies = false;
        for (index, line) in source.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.starts_with('[') {
                in_dependencies = trimmed.contains("dependencies");
                continue;
            }
            if !in_dependencies || !dependency.is_match(line) {
                continue;
            }
            declarations += 1;
            let location = ArchitectureLocation {
                file: relative.clone(),
                line: index + 1,
                end_line: index + 1,
            };
            let name = line.split('=').next().unwrap_or("dependency").trim();
            facts.push(fact(
                &Candidate {
                    kind: "cargo_dependency",
                    subject: name.to_owned(),
                    location: location.clone(),
                    summary: format!("Cargo dependency declaration `{}`", line.trim()),
                    details: json!({"declaration":line.trim()}),
                },
                profile,
                "CargoManifest",
                1.0,
            ));
            if let Some(capture) = wildcard.captures(line) {
                let name = capture.get(1).map_or("dependency", |value| value.as_str());
                findings.push(finding(
                    "wildcard-dependency",
                    "Dependency version is unconstrained",
                    "dependencies",
                    "high",
                    0.99,
                    name,
                    "A wildcard dependency prevents reproducible compatibility intent and can admit breaking releases.",
                    vec![location],
                    1,
                    vec![line.trim().to_owned()],
                    vec!["Cargo.lock protects one resolved application build but does not define a library's supported dependency range.".to_owned()],
                    "Declare the narrowest compatible semver range and verify the minimum and locked versions in CI.",
                    Vec::new(),
                ));
            } else if line.contains("git =") && !line.contains("rev =") && !line.contains("tag =") {
                findings.push(finding(
                    "floating-git-dependency",
                    "Git dependency is not pinned",
                    "dependencies",
                    "high",
                    0.99,
                    line.split('=').next().unwrap_or("dependency").trim(),
                    "A git dependency follows a moving branch because no immutable revision or tag is declared.",
                    vec![location],
                    1,
                    vec![line.trim().to_owned()],
                    vec!["Cargo.lock fixes the current checkout but updates can silently select a different commit.".to_owned()],
                    "Pin an immutable revision (and document the update process) or publish/use a versioned crate.",
                    Vec::new(),
                ));
            }
        }
    }
    manifests.sort();
    Ok(ManifestScan {
        manifests,
        dependency_declarations: declarations,
        facts,
        findings,
        limitations,
    })
}

struct SignalVisitor<'a> {
    file: &'a str,
    source: &'a str,
    candidates: Vec<Candidate>,
    function: String,
    async_depth: usize,
    loop_depth: usize,
    blocking_worker_depth: usize,
    test_depth: usize,
    function_has_concurrency_bound: bool,
}

impl<'a> SignalVisitor<'a> {
    fn new(file: &'a str, source: &'a str) -> Self {
        Self {
            file,
            source,
            candidates: Vec::new(),
            function: "module scope".to_owned(),
            async_depth: 0,
            loop_depth: 0,
            blocking_worker_depth: 0,
            test_depth: 0,
            function_has_concurrency_bound: false,
        }
    }

    fn record(&mut self, kind: &'static str, span: proc_macro2::Span, summary: String) {
        self.candidates.push(Candidate {
            kind,
            subject: self.function.clone(),
            location: location(self.file, span),
            summary,
            details: json!({}),
        });
    }

    fn enter_function<F>(
        &mut self,
        name: String,
        is_async: bool,
        is_test: bool,
        has_concurrency_bound: bool,
        visit: F,
    ) where
        F: FnOnce(&mut Self),
    {
        let old_name = std::mem::replace(&mut self.function, name);
        let old_bound = std::mem::replace(
            &mut self.function_has_concurrency_bound,
            has_concurrency_bound,
        );
        self.async_depth += usize::from(is_async);
        self.test_depth += usize::from(is_test);
        visit(self);
        self.test_depth -= usize::from(is_test);
        self.async_depth -= usize::from(is_async);
        self.function_has_concurrency_bound = old_bound;
        self.function = old_name;
    }

    fn line(&self, line: usize) -> &str {
        self.source
            .lines()
            .nth(line.saturating_sub(1))
            .unwrap_or("")
    }

    fn function_source_has_concurrency_bound(&self) -> bool {
        self.function_has_concurrency_bound
    }
}

impl<'ast> Visit<'ast> for SignalVisitor<'_> {
    fn visit_item_fn(&mut self, node: &'ast ItemFn) {
        let name = node.sig.ident.to_string();
        let is_test = is_test_name_or_attrs(&name, &node.attrs);
        if self.test_depth == 0 && !is_test && node.sig.asyncness.is_some() {
            for (span, guard) in sync_guards_across_await(&node.block, self.source) {
                self.candidates.push(Candidate {
                    kind: "sync_guard_across_await",
                    subject: name.clone(),
                    location: location(self.file, span),
                    summary: format!("guard `{guard}` remains live at this await"),
                    details: json!({"guard":guard}),
                });
            }
        }
        if self.test_depth == 0
            && !is_test
            && node.sig.unsafety.is_some()
            && !has_nearby_contract(self.source, node.span().start().line, "# Safety")
        {
            self.candidates.push(Candidate {
                kind: "unsafe_without_contract",
                subject: name.clone(),
                location: location(self.file, node.span()),
                summary: "unsafe function has no nearby `# Safety` documentation".to_owned(),
                details: json!({}),
            });
        }
        self.enter_function(
            name,
            node.sig.asyncness.is_some(),
            is_test,
            contains_concurrency_guard(&node.block),
            |visitor| visit::visit_item_fn(visitor, node),
        );
    }

    fn visit_impl_item_fn(&mut self, node: &'ast ImplItemFn) {
        let name = node.sig.ident.to_string();
        let is_test = is_test_name_or_attrs(&name, &node.attrs);
        if self.test_depth == 0 && !is_test && node.sig.asyncness.is_some() {
            for (span, guard) in sync_guards_across_await(&node.block, self.source) {
                self.candidates.push(Candidate {
                    kind: "sync_guard_across_await",
                    subject: name.clone(),
                    location: location(self.file, span),
                    summary: format!("guard `{guard}` remains live at this await"),
                    details: json!({"guard":guard}),
                });
            }
        }
        if self.test_depth == 0
            && !is_test
            && node.sig.unsafety.is_some()
            && !has_nearby_contract(self.source, node.span().start().line, "# Safety")
        {
            self.candidates.push(Candidate {
                kind: "unsafe_without_contract",
                subject: name.clone(),
                location: location(self.file, node.span()),
                summary: "unsafe method has no nearby `# Safety` documentation".to_owned(),
                details: json!({}),
            });
        }
        self.enter_function(
            name,
            node.sig.asyncness.is_some(),
            is_test,
            contains_concurrency_guard(&node.block),
            |visitor| visit::visit_impl_item_fn(visitor, node),
        );
    }

    fn visit_expr_async(&mut self, node: &'ast ExprAsync) {
        self.async_depth += 1;
        visit::visit_expr_async(self, node);
        self.async_depth -= 1;
    }

    fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
        let is_test_module = attrs_contain_test_cfg(&node.attrs);
        self.test_depth += usize::from(is_test_module);
        visit::visit_item_mod(self, node);
        self.test_depth -= usize::from(is_test_module);
    }

    fn visit_expr_for_loop(&mut self, node: &'ast ExprForLoop) {
        if is_small_static_range(&node.expr) {
            visit::visit_expr_for_loop(self, node);
        } else {
            self.loop_depth += 1;
            visit::visit_expr_for_loop(self, node);
            self.loop_depth -= 1;
        }
    }

    fn visit_expr_while(&mut self, node: &'ast ExprWhile) {
        self.loop_depth += 1;
        visit::visit_expr_while(self, node);
        self.loop_depth -= 1;
    }

    fn visit_expr_loop(&mut self, node: &'ast ExprLoop) {
        self.loop_depth += 1;
        visit::visit_expr_loop(self, node);
        self.loop_depth -= 1;
    }

    fn visit_expr_unsafe(&mut self, node: &'ast ExprUnsafe) {
        let line = node.unsafe_token.span.start().line;
        if self.test_depth == 0 && !has_nearby_contract(self.source, line, "SAFETY:") {
            self.record(
                "unsafe_without_contract",
                node.span(),
                "unsafe block has no nearby `SAFETY:` justification".to_owned(),
            );
        }
        visit::visit_expr_unsafe(self, node);
    }

    fn visit_expr_call(&mut self, node: &'ast ExprCall) {
        let path = call_path(&node.func);
        let lower = path.to_ascii_lowercase();
        if lower.ends_with("spawn_blocking") || lower.ends_with("block_in_place") {
            self.blocking_worker_depth += 1;
            visit::visit_expr_call(self, node);
            self.blocking_worker_depth -= 1;
            return;
        }
        if self.test_depth == 0
            && (path.ends_with("tokio::spawn") || path.ends_with("async_std::task::spawn"))
        {
            let line = node.span().start().line;
            let source_line = self.line(line).trim().to_owned();
            if self.loop_depth > 0 && !self.function_source_has_concurrency_bound() {
                self.record(
                    "unbounded_spawn_loop",
                    node.span(),
                    format!("spawn in loop at `{source_line}`"),
                );
            }
            if (source_line.starts_with("tokio::spawn")
                || source_line.starts_with("async_std::task::spawn"))
                || source_line.starts_with("let _ = tokio::spawn")
                || source_line.starts_with("let _ = async_std::task::spawn")
            {
                if source_line.contains(".await") {
                    visit::visit_expr_call(self, node);
                    return;
                }
                self.record(
                    "detached_task",
                    node.span(),
                    format!("spawn handle discarded at `{source_line}`"),
                );
            }
        }
        if self.test_depth == 0
            && self.async_depth > 0
            && self.blocking_worker_depth == 0
            && is_blocking_path(&path, self.source)
        {
            self.record(
                "blocking_in_async",
                node.span(),
                format!("blocking call `{path}` occurs in async code"),
            );
        }
        if self.test_depth == 0 && (path.ends_with("env::var") || path == "env::var") {
            self.record(
                "environment_read",
                node.span(),
                format!("environment read through `{path}`"),
            );
        }
        visit::visit_expr_call(self, node);
    }

    fn visit_expr_method_call(&mut self, node: &'ast ExprMethodCall) {
        if self.test_depth == 0 && node.method == "ok" {
            let line = node.span().start().line;
            let text = self.line(line).trim();
            if text.ends_with(".ok();") && !text.contains('=') && !text.starts_with("return ") {
                self.record(
                    "discarded_error",
                    node.span(),
                    format!("result discarded at `{text}`"),
                );
            }
        }
        visit::visit_expr_method_call(self, node);
    }
}

fn fact(
    candidate: &Candidate,
    profile: &str,
    provenance: &str,
    confidence: f64,
) -> ArchitectureFact {
    let key = format!(
        "{}:{}:{}:{}",
        candidate.kind, candidate.subject, candidate.location.file, candidate.location.line
    );
    ArchitectureFact {
        id: format!("fact_{}", &blake3::hash(key.as_bytes()).to_hex()[..12]),
        kind: candidate.kind.to_owned(),
        subject: candidate.subject.clone(),
        location: candidate.location.clone(),
        summary: candidate.summary.clone(),
        provenance: provenance.to_owned(),
        confidence,
        profile: profile.to_owned(),
        details: candidate.details.clone(),
    }
}

#[allow(clippy::too_many_arguments)]
fn finding(
    rule_id: &str,
    title: &str,
    category: &str,
    severity: &str,
    confidence: f64,
    subject: &str,
    summary: &str,
    locations: Vec<ArchitectureLocation>,
    occurrence_count: usize,
    evidence: Vec<String>,
    counter_evidence: Vec<String>,
    recommendation: &str,
    limitations: Vec<String>,
) -> ArchitectureFinding {
    let primary_file = locations
        .first()
        .map_or("workspace", |location| location.file.as_str());
    let key = format!("{rule_id}:{subject}:{primary_file}");
    ArchitectureFinding {
        id: format!("af_{}", &blake3::hash(key.as_bytes()).to_hex()[..12]),
        rule_id: rule_id.to_owned(),
        title: title.to_owned(),
        category: category.to_owned(),
        severity: severity.to_owned(),
        confidence,
        subject: subject.to_owned(),
        summary: summary.to_owned(),
        locations,
        occurrence_count,
        evidence,
        counter_evidence,
        recommendation: recommendation.to_owned(),
        limitations,
        origin: "architecture_audit".to_owned(),
    }
}

fn state_struct(file: &str, item: &ItemStruct) -> Option<StateStruct> {
    let excluded = [
        "config", "options", "flags", "request", "response", "params", "settings",
    ];
    let name = item.ident.to_string();
    if excluded
        .iter()
        .any(|suffix| name.to_ascii_lowercase().contains(suffix))
    {
        return None;
    }
    let Fields::Named(fields) = &item.fields else {
        return None;
    };
    let bool_fields = fields
        .named
        .iter()
        .filter(|field| is_bool(&field.ty))
        .filter_map(|field| field.ident.as_ref().map(ToString::to_string))
        .collect::<Vec<_>>();
    let state_words = [
        "active",
        "running",
        "complete",
        "completed",
        "failed",
        "enabled",
        "ready",
        "closed",
        "open",
        "pending",
        "cancelled",
        "success",
        "started",
        "stopped",
    ];
    let state_fields = bool_fields
        .iter()
        .filter(|name| state_words.iter().any(|word| name.contains(word)))
        .cloned()
        .collect::<Vec<_>>();
    if bool_fields.is_empty() {
        return None;
    }
    Some(StateStruct {
        name,
        location: location(file, item.span()),
        bool_fields,
        state_fields,
    })
}

fn public_function(file: &str, source: &str, item: &ItemFn) -> PublicFunction {
    PublicFunction {
        name: item.sig.ident.to_string(),
        location: location(file, item.sig.span()),
        signature: signature_source(source, item.sig.span().start().line),
        string_ids: string_id_parameters(&item.sig.inputs),
    }
}

fn public_method(file: &str, source: &str, item: &ImplItemFn) -> PublicFunction {
    PublicFunction {
        name: item.sig.ident.to_string(),
        location: location(file, item.sig.span()),
        signature: signature_source(source, item.sig.span().start().line),
        string_ids: string_id_parameters(&item.sig.inputs),
    }
}

fn string_id_parameters(
    inputs: &syn::punctuated::Punctuated<FnArg, syn::token::Comma>,
) -> Vec<String> {
    inputs
        .iter()
        .filter_map(|argument| {
            let FnArg::Typed(argument) = argument else {
                return None;
            };
            let Pat::Ident(name) = argument.pat.as_ref() else {
                return None;
            };
            let name = name.ident.to_string();
            (name.ends_with("_id") && is_string_like(&argument.ty)).then_some(name)
        })
        .collect()
}

fn exposes_opaque_error(source: &str, signature: &str) -> bool {
    let result_alias = source.contains("use anyhow::Result")
        || (source.contains("use anyhow::{") && source.contains("Result"));
    signature.contains("anyhow::Result")
        || signature.contains("Box<dyn Error")
        || signature.contains("Box<dyn std::error::Error")
        || (result_alias && signature.contains("-> Result"))
}

fn is_blocking_path(path: &str, source: &str) -> bool {
    path.starts_with("std::fs::")
        || path == "std::thread::sleep"
        || path.starts_with("std::process::Command::")
        || (path.starts_with("fs::")
            && (source.contains("use std::fs") || source.contains("fs::{self")))
        || (path.starts_with("Command::") && source.contains("process::Command"))
}

fn call_path(expression: &Expr) -> String {
    let Expr::Path(path) = expression else {
        return String::new();
    };
    path.path
        .segments
        .iter()
        .map(|segment| segment.ident.to_string())
        .collect::<Vec<_>>()
        .join("::")
}

#[derive(Default)]
struct StatementSignalVisitor {
    await_span: Option<proc_macro2::Span>,
    dropped: BTreeSet<String>,
}

impl<'ast> Visit<'ast> for StatementSignalVisitor {
    fn visit_expr_await(&mut self, expression: &'ast ExprAwait) {
        self.await_span.get_or_insert(expression.await_token.span);
        visit::visit_expr_await(self, expression);
    }

    fn visit_expr_call(&mut self, call: &'ast ExprCall) {
        if call_path(&call.func) == "drop"
            && let Some(Expr::Path(argument)) = call.args.first()
            && let Some(name) = argument.path.get_ident()
        {
            self.dropped.insert(name.to_string());
        }
        visit::visit_expr_call(self, call);
    }
}

fn sync_guards_across_await(block: &Block, source: &str) -> Vec<(proc_macro2::Span, String)> {
    let has_std_locks = source.contains("std::sync")
        || (source.contains("sync::{") && (source.contains("Mutex") || source.contains("RwLock")));
    let has_parking_lot = source.contains("parking_lot");
    if !has_std_locks && !has_parking_lot {
        return Vec::new();
    }
    let mut held = BTreeSet::<String>::new();
    let mut reported = BTreeSet::new();
    let mut findings = Vec::new();
    for statement in &block.stmts {
        let mut signals = StatementSignalVisitor::default();
        signals.visit_stmt(statement);
        for dropped in signals.dropped {
            held.remove(&dropped);
        }
        if let Some(span) = signals.await_span {
            for guard in held.difference(&reported).cloned().collect::<Vec<_>>() {
                findings.push((span, guard.clone()));
                reported.insert(guard);
            }
        }
        let Stmt::Local(local) = statement else {
            continue;
        };
        if signals.await_span.is_none()
            && local.init.as_ref().is_some_and(|initializer| {
                sync_lock_initializer(&initializer.expr, has_parking_lot)
            })
            && let Some(binding) = pattern_ident(&local.pat)
        {
            held.insert(binding);
        }
    }
    findings
}

fn sync_lock_initializer(expression: &Expr, has_parking_lot: bool) -> bool {
    match expression {
        Expr::MethodCall(call)
            if matches!(call.method.to_string().as_str(), "unwrap" | "expect") =>
        {
            direct_lock_call(&call.receiver)
        }
        Expr::Try(expression) => direct_lock_call(&expression.expr),
        Expr::Paren(expression) => sync_lock_initializer(&expression.expr, has_parking_lot),
        expression => has_parking_lot && direct_lock_call(expression),
    }
}

fn direct_lock_call(expression: &Expr) -> bool {
    let Expr::MethodCall(call) = expression else {
        return false;
    };
    matches!(call.method.to_string().as_str(), "lock" | "read" | "write")
}

fn pattern_ident(pattern: &Pat) -> Option<String> {
    match pattern {
        Pat::Ident(pattern) => Some(pattern.ident.to_string()),
        Pat::Type(pattern) => pattern_ident(&pattern.pat),
        _ => None,
    }
}

#[derive(Default)]
struct ConcurrencyGuardVisitor {
    found: bool,
}

impl<'ast> Visit<'ast> for ConcurrencyGuardVisitor {
    fn visit_path(&mut self, path: &'ast syn::Path) {
        let guarded = path.segments.iter().any(|segment| {
            matches!(
                segment.ident.to_string().to_ascii_lowercase().as_str(),
                "semaphore" | "joinset" | "futuresunordered"
            )
        });
        self.found |= guarded;
        visit::visit_path(self, path);
    }

    fn visit_expr_method_call(&mut self, call: &'ast ExprMethodCall) {
        self.found |= matches!(
            call.method.to_string().as_str(),
            "acquire_owned" | "acquire_many" | "buffer_unordered" | "for_each_concurrent"
        );
        visit::visit_expr_method_call(self, call);
    }
}

fn contains_concurrency_guard(block: &syn::Block) -> bool {
    let mut visitor = ConcurrencyGuardVisitor::default();
    visitor.visit_block(block);
    visitor.found
}

fn is_small_static_range(expression: &Expr) -> bool {
    let Expr::Range(range) = expression else {
        return false;
    };
    let Some(end) = range.end.as_deref() else {
        return false;
    };
    let Expr::Lit(end) = end else {
        return false;
    };
    let syn::Lit::Int(end) = &end.lit else {
        return false;
    };
    let upper = end.base10_parse::<u64>().ok();
    let lower = range
        .start
        .as_deref()
        .and_then(|start| match start {
            Expr::Lit(start) => match &start.lit {
                syn::Lit::Int(start) => start.base10_parse::<u64>().ok(),
                _ => None,
            },
            _ => None,
        })
        .unwrap_or(0);
    upper.is_some_and(|upper| upper.saturating_sub(lower) <= 64)
}

fn self_type_name(ty: &Type) -> Option<String> {
    let Type::Path(path) = ty else {
        return None;
    };
    path.path
        .segments
        .last()
        .map(|segment| segment.ident.to_string())
}

fn is_bool(ty: &Type) -> bool {
    matches!(ty, Type::Path(path) if path.path.is_ident("bool"))
}

fn is_string_like(ty: &Type) -> bool {
    match ty {
        Type::Path(path) => path
            .path
            .segments
            .last()
            .is_some_and(|segment| segment.ident == "String" || segment.ident == "str"),
        Type::Reference(reference) => is_string_like(&reference.elem),
        _ => false,
    }
}

fn is_public(visibility: &Visibility) -> bool {
    matches!(visibility, Visibility::Public(_))
}

fn item_is_public(item: &Item) -> bool {
    match item {
        Item::Const(item) => is_public(&item.vis),
        Item::Enum(item) => is_public(&item.vis),
        Item::Fn(item) => is_public(&item.vis),
        Item::Mod(item) => is_public(&item.vis),
        Item::Static(item) => is_public(&item.vis),
        Item::Struct(item) => is_public(&item.vis),
        Item::Trait(item) => is_public(&item.vis),
        Item::Type(item) => is_public(&item.vis),
        Item::Union(item) => is_public(&item.vis),
        _ => false,
    }
}

fn location(file: &str, span: proc_macro2::Span) -> ArchitectureLocation {
    ArchitectureLocation {
        file: file.to_owned(),
        line: span.start().line.max(1),
        end_line: span.end().line.max(span.start().line).max(1),
    }
}

fn has_nearby_contract(source: &str, line: usize, marker: &str) -> bool {
    let start = line.saturating_sub(8);
    source
        .lines()
        .skip(start)
        .take(line.saturating_sub(start))
        .any(|text| {
            let text = text.trim_start();
            (text.starts_with("//") || text.starts_with("#[doc")) && text.contains(marker)
        })
}

fn has_serde_derive(attributes: &[syn::Attribute]) -> bool {
    attributes.iter().any(|attribute| {
        if !attribute.path().is_ident("derive") {
            return false;
        }
        let mut found = false;
        let _ = attribute.parse_nested_meta(|meta| {
            found |= meta.path.is_ident("Serialize") || meta.path.is_ident("Deserialize");
            Ok(())
        });
        found
    })
}

fn is_test_name_or_attrs(name: &str, attrs: &[syn::Attribute]) -> bool {
    name.starts_with("test_")
        || attrs.iter().any(|attribute| {
            attribute
                .path()
                .segments
                .last()
                .is_some_and(|segment| segment.ident == "test")
        })
}

fn attrs_contain_test_cfg(attributes: &[syn::Attribute]) -> bool {
    attributes.iter().any(|attribute| {
        if !attribute.path().is_ident("cfg") {
            return false;
        }
        let mut test = false;
        let _ = attribute.parse_nested_meta(|meta| {
            test |= meta.path.is_ident("test");
            Ok(())
        });
        test
    })
}

fn signature_source(source: &str, start_line: usize) -> String {
    let mut signature = String::new();
    for line in source.lines().skip(start_line.saturating_sub(1)).take(12) {
        if let Some(index) = line.find('{') {
            signature.push_str(line[..index].trim());
            break;
        }
        signature.push_str(line.trim());
        signature.push(' ');
        if line.trim_end().ends_with(';') {
            break;
        }
    }
    signature.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn use_roots(source: &str) -> Vec<String> {
    let expression =
        Regex::new(r"(?m)^\s*(?:pub\s+)?use\s+(?:crate::|self::|super::)?([A-Za-z_][A-Za-z0-9_]*)")
            .expect("valid use regex");
    expression
        .captures_iter(source)
        .filter_map(|capture| capture.get(1).map(|value| value.as_str().to_owned()))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn active_profile() -> String {
    let target = std::env::var("CARGO_BUILD_TARGET")
        .ok()
        .filter(|target| !target.trim().is_empty())
        .unwrap_or_else(|| "host-default".to_owned());
    let features = std::env::var("RUST_REPO_INTELLIGENCE_FEATURES")
        .ok()
        .filter(|features| !features.trim().is_empty())
        .unwrap_or_else(|| "cargo-default".to_owned());
    format!("target={target};features={features}")
}

fn infer_role(path: &str) -> &'static str {
    let path = path.to_ascii_lowercase();
    let parts = path
        .split(|character: char| !character.is_ascii_alphanumeric())
        .collect::<BTreeSet<_>>();
    if parts.contains("config") || parts.contains("configuration") || parts.contains("settings") {
        "configuration"
    } else if parts.contains("domain")
        || parts.contains("core")
        || parts.contains("model")
        || parts.contains("models")
    {
        "domain"
    } else if parts.contains("http")
        || parts.contains("api")
        || parts.contains("handler")
        || parts.contains("handlers")
        || parts.contains("route")
        || parts.contains("routes")
        || parts.contains("transport")
        || parts.contains("mcp")
    {
        "transport"
    } else if parts.contains("adapter")
        || parts.contains("adapters")
        || parts.contains("integration")
        || parts.contains("integrations")
    {
        "adapter"
    } else if parts.contains("persistence")
        || parts.contains("storage")
        || parts.contains("database")
        || parts.contains("repository")
        || parts.contains("repositories")
        || parts.contains("store")
        || parts.contains("db")
    {
        "persistence"
    } else if parts.contains("test") || parts.contains("tests") || parts.contains("fixture") {
        "test"
    } else if parts.contains("analysis") || parts.contains("audit") {
        "analysis"
    } else if parts.contains("quality") || parts.contains("policy") {
        "policy"
    } else if parts.contains("examples") || parts.contains("example") {
        "example"
    } else if path == "src/main.rs"
        || path.contains("/bin/")
        || path.contains("cli")
        || parts.contains("service")
        || parts.contains("services")
        || parts.contains("usecase")
        || parts.contains("observatory")
    {
        "application"
    } else {
        "unclassified"
    }
}

fn infer_role_with_dependencies(path: &str, uses: &[String]) -> &'static str {
    let role = infer_role(path);
    if role != "unclassified" {
        return role;
    }
    if uses
        .iter()
        .any(|dependency| matches!(dependency.as_str(), "axum" | "actix_web" | "rmcp" | "tonic"))
    {
        "transport"
    } else if uses.iter().any(|dependency| {
        matches!(
            dependency.as_str(),
            "rusqlite" | "sqlx" | "diesel" | "sea_orm"
        )
    }) {
        "persistence"
    } else {
        role
    }
}

fn is_domain_role(role: &str) -> bool {
    role == "domain"
}

fn word_occurrences(source: &str, word: &str) -> usize {
    Regex::new(&format!(r"\b{}\b", regex::escape(word)))
        .map_or(0, |expression| expression.find_iter(source).count())
}

fn finding_matches_scope(finding: &ArchitectureFinding, scope: &str) -> bool {
    finding.subject.to_ascii_lowercase().contains(scope)
        || finding.title.to_ascii_lowercase().contains(scope)
        || finding.rule_id.to_ascii_lowercase().contains(scope)
        || finding
            .locations
            .iter()
            .any(|location| location.file.to_ascii_lowercase().contains(scope))
}

fn finding_summary(
    findings: &[ArchitectureFinding],
    detected_finding_count: usize,
    returned_fact_count: usize,
    observed_fact_count: usize,
) -> Value {
    let mut severity = BTreeMap::<String, usize>::new();
    let mut category = BTreeMap::<String, usize>::new();
    for finding in findings {
        *severity.entry(finding.severity.clone()).or_default() += 1;
        *category.entry(finding.category.clone()).or_default() += 1;
    }
    json!({
        "returned_findings":findings.len(),
        "detected_findings":detected_finding_count,
        "findings_truncated":findings.len() < detected_finding_count,
        "returned_facts":returned_fact_count,
        "observed_facts":observed_fact_count,
        "facts_truncated":returned_fact_count < observed_fact_count,
        "by_severity":severity,
        "by_category":category
    })
}

fn severity_rank(severity: &str) -> usize {
    match severity {
        "critical" => 4,
        "high" => 3,
        "medium" => 2,
        "low" => 1,
        _ => 0,
    }
}

fn coalesce_findings(findings: Vec<ArchitectureFinding>) -> Vec<ArchitectureFinding> {
    let mut coalesced = BTreeMap::<String, ArchitectureFinding>::new();
    for finding in findings {
        if let Some(existing) = coalesced.get_mut(&finding.id) {
            existing.occurrence_count += finding.occurrence_count;
            for location in finding.locations {
                if !existing.locations.contains(&location) {
                    existing.locations.push(location);
                }
            }
            for evidence in finding.evidence {
                if !existing.evidence.contains(&evidence) {
                    existing.evidence.push(evidence);
                }
            }
            existing.confidence = existing.confidence.max(finding.confidence);
        } else {
            coalesced.insert(finding.id.clone(), finding);
        }
    }
    coalesced.into_values().collect()
}

fn rust_files(root: &Path) -> (Vec<PathBuf>, Vec<String>) {
    let mut files = Vec::new();
    let mut limitations = Vec::new();
    for entry in WalkDir::new(root).into_iter().filter_entry(allowed_entry) {
        match entry {
            Ok(entry)
                if entry.file_type().is_file()
                    && entry
                        .path()
                        .extension()
                        .is_some_and(|extension| extension == "rs") =>
            {
                files.push(entry.into_path());
            }
            Ok(_) => {}
            Err(error) => {
                limitations.push(format!("Could not traverse a Rust source path: {error}"))
            }
        }
    }
    files.sort();
    (files, limitations)
}

fn allowed_entry(entry: &walkdir::DirEntry) -> bool {
    let name = entry.file_name().to_string_lossy();
    !matches!(name.as_ref(), ".git" | "target" | ".rust-repo-intelligence")
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn fixture(source: &str) -> Result<(tempfile::TempDir, ArchitectureReport)> {
        let directory = tempdir()?;
        fs::create_dir(directory.path().join("src"))?;
        fs::write(
            directory.path().join("Cargo.toml"),
            "[package]\nname='fixture'\nversion='0.1.0'\nedition='2024'\n",
        )?;
        fs::write(directory.path().join("src/lib.rs"), source)?;
        let report = analyze(directory.path(), json!({"digest":"fixture"}), None, 100)?;
        Ok((directory, report))
    }

    #[test]
    fn contextual_rules_detect_unsafe_async_and_state_failures() -> Result<()> {
        let (_directory, report) = fixture(
            r#"
use std::sync::Mutex;

pub struct Job {
    active: bool,
    running: bool,
    failed: bool,
}

pub async fn run() {
    while should_continue() {
        tokio::spawn(async {});
    }
    std::fs::read_to_string("input").ok();
    unsafe { std::ptr::read(std::ptr::null()) };
}

pub async fn lock_then_wait(lock: &Mutex<u8>) {
    let guard = lock.lock().unwrap();
    ready().await;
    drop(guard);
}
"#,
        )?;
        let rules = report
            .findings
            .iter()
            .map(|finding| finding.rule_id.as_str())
            .collect::<BTreeSet<_>>();
        assert!(rules.contains("boolean-state-cluster"));
        assert!(rules.contains("unbounded-spawn-loop"));
        assert!(rules.contains("detached-task"));
        assert!(rules.contains("blocking-in-async"));
        assert!(rules.contains("sync-guard-across-await"));
        assert!(rules.contains("discarded-error"));
        assert!(rules.contains("unsafe-contract"));
        Ok(())
    }

    #[test]
    fn contextual_rules_respect_local_counter_evidence() -> Result<()> {
        let (_directory, report) = fixture(
            r#"
pub struct WorkerOptions {
    active: bool,
    running: bool,
    failed: bool,
}

pub async fn run() {
    let handle = tokio::spawn(async {});
    let _blocking_result = tokio::task::spawn_blocking(|| std::fs::read_to_string("input")).await;
    let parsed = "42".parse::<u8>().ok();
    let _ = parsed;
    let _ = handle.await;
    // SAFETY: the pointer comes from Box::into_raw immediately above.
    unsafe { std::ptr::read(Box::into_raw(Box::new(1))) };
}
"#,
        )?;
        let rules = report
            .findings
            .iter()
            .map(|finding| finding.rule_id.as_str())
            .collect::<BTreeSet<_>>();
        assert!(!rules.contains("boolean-state-cluster"));
        assert!(!rules.contains("detached-task"));
        assert!(!rules.contains("blocking-in-async"));
        assert!(!rules.contains("discarded-error"));
        assert!(!rules.contains("unsafe-contract"));
        Ok(())
    }

    #[test]
    fn delta_only_escalates_findings_touching_the_change() -> Result<()> {
        let location = ArchitectureLocation {
            file: "src/domain/job.rs".to_owned(),
            line: 4,
            end_line: 4,
        };
        let baseline_finding = finding(
            "rule",
            "Rule",
            "invariants",
            "medium",
            0.9,
            "Job",
            "summary",
            vec![location.clone()],
            1,
            Vec::new(),
            Vec::new(),
            "fix",
            Vec::new(),
        );
        let mut worsened = baseline_finding.clone();
        worsened.occurrence_count = 2;
        let files = BTreeSet::from(["src/domain/job.rs".to_owned()]);
        let value = delta(&baseline(&[baseline_finding]), &[worsened], &files);
        assert_eq!(value["new"].as_array().map(Vec::len), Some(0));
        assert_eq!(value["worsened"].as_array().map(Vec::len), Some(1));
        assert_eq!(value["attention_required"], true);
        Ok(())
    }
}

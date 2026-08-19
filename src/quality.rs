use super::{Node, Service, terms, trim_text};
use anyhow::{Context, Result, bail, ensure};
use chrono::Utc;
use regex::Regex;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
    process::{Command, Stdio},
};

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS problem_records(
    id TEXT PRIMARY KEY, sequence INTEGER NOT NULL, fingerprint TEXT NOT NULL UNIQUE,
    original_report TEXT NOT NULL, summary TEXT NOT NULL, family TEXT NOT NULL,
    status TEXT NOT NULL, confidence REAL NOT NULL, scope_json TEXT NOT NULL,
    reproduction TEXT, diagnostic_signature TEXT, root_cause TEXT, fix_reference TEXT,
    evidence_json TEXT NOT NULL, related_json TEXT NOT NULL, provenance TEXT NOT NULL,
    revision TEXT NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS problem_family_status ON problem_records(family,status);
CREATE TABLE IF NOT EXISTS problem_occurrences(
    id INTEGER PRIMARY KEY, problem_id TEXT NOT NULL REFERENCES problem_records(id) ON DELETE CASCADE,
    report TEXT NOT NULL, evidence_json TEXT NOT NULL, revision TEXT NOT NULL, created_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS problem_links(
    problem_id TEXT NOT NULL REFERENCES problem_records(id) ON DELETE CASCADE,
    related_id TEXT NOT NULL REFERENCES problem_records(id) ON DELETE CASCADE,
    relationship TEXT NOT NULL, PRIMARY KEY(problem_id,related_id,relationship)
);
CREATE TABLE IF NOT EXISTS quality_constraints(
    id TEXT PRIMARY KEY, sequence INTEGER NOT NULL, fingerprint TEXT NOT NULL UNIQUE,
    rule TEXT NOT NULL, category TEXT NOT NULL, status TEXT NOT NULL,
    scope_json TEXT NOT NULL, exclusions_json TEXT NOT NULL, activation_json TEXT NOT NULL,
    recipe_json TEXT NOT NULL, enforcement TEXT NOT NULL, confidence REAL NOT NULL,
    maturity TEXT NOT NULL, provenance_json TEXT NOT NULL, last_successful_validation TEXT,
    expires_at TEXT, invalidation_json TEXT NOT NULL, merged_into TEXT REFERENCES quality_constraints(id),
    revision TEXT NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS quality_constraint_state ON quality_constraints(status,maturity,category);
CREATE TABLE IF NOT EXISTS problem_quality_constraints(
    problem_id TEXT NOT NULL REFERENCES problem_records(id) ON DELETE CASCADE,
    constraint_id TEXT NOT NULL REFERENCES quality_constraints(id) ON DELETE CASCADE,
    PRIMARY KEY(problem_id,constraint_id)
);
CREATE TABLE IF NOT EXISTS validation_obligations(
    id TEXT PRIMARY KEY, context_id TEXT NOT NULL, source TEXT NOT NULL,
    selected_reason TEXT NOT NULL, matched_surfaces_json TEXT NOT NULL,
    recipe_json TEXT NOT NULL, expected TEXT NOT NULL, enforcement TEXT NOT NULL,
    status TEXT NOT NULL, blocking INTEGER NOT NULL, evidence_json TEXT NOT NULL,
    created_at TEXT NOT NULL, updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS validation_obligation_context ON validation_obligations(context_id,status);
CREATE TABLE IF NOT EXISTS validation_obligation_constraints(
    obligation_id TEXT NOT NULL REFERENCES validation_obligations(id) ON DELETE CASCADE,
    constraint_id TEXT NOT NULL REFERENCES quality_constraints(id) ON DELETE CASCADE,
    PRIMARY KEY(obligation_id,constraint_id)
);
CREATE TABLE IF NOT EXISTS quality_validation_history(
    id INTEGER PRIMARY KEY, constraint_id TEXT NOT NULL REFERENCES quality_constraints(id) ON DELETE CASCADE,
    obligation_id TEXT NOT NULL REFERENCES validation_obligations(id) ON DELETE CASCADE,
    status TEXT NOT NULL, evidence_json TEXT NOT NULL, provenance TEXT NOT NULL,
    revision TEXT NOT NULL, created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS quality_history_constraint ON quality_validation_history(constraint_id,created_at);
";

const FAMILIES: &[&str] = &[
    "compiler_warning",
    "clippy_warning",
    "runtime_diagnostic",
    "gtk_diagnostic",
    "action_wiring",
    "visual_consistency",
    "dynamic_text_markup",
    "database_migration",
    "performance_regression",
    "test_regression",
    "uncategorized",
];

const PROBLEM_STATUSES: &[&str] = &[
    "reported",
    "reproduced",
    "diagnosed",
    "fixed",
    "verified",
    "obsolete",
];
const CONSTRAINT_STATUSES: &[&str] = &[
    "proposed", "active", "rejected", "disabled", "retired", "merged",
];
const MATURITIES: &[&str] = &["proposed", "approved", "established", "deprecated"];
const ENFORCEMENTS: &[&str] = &["observe", "validate", "block"];
const RECIPE_KINDS: &[&str] = &[
    "cargo",
    "focused_test",
    "runtime_log",
    "ui_smoke",
    "structural_source",
    "widget_tree",
    "visual_comparison",
    "manual",
];

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct QualityScope {
    #[serde(default)]
    pub crates: Vec<String>,
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub modules: Vec<String>,
    #[serde(default)]
    pub symbols: Vec<String>,
    #[serde(default)]
    pub components: Vec<String>,
    #[serde(default)]
    pub dependencies: Vec<String>,
    #[serde(default)]
    pub concepts: Vec<String>,
    #[serde(default)]
    pub widget_types: Vec<String>,
    #[serde(default)]
    pub css_classes: Vec<String>,
    #[serde(default)]
    pub design_tokens: Vec<String>,
    #[serde(default)]
    pub diagnostics: Vec<String>,
    #[serde(default)]
    pub configurations: Vec<String>,
}

impl QualityScope {
    fn normalize(&mut self) {
        for values in self.fields_mut() {
            values.retain(|value| !value.trim().is_empty());
            values
                .iter_mut()
                .for_each(|value| *value = value.trim().to_owned());
            values.sort();
            values.dedup();
        }
    }

    fn has_selectors(&self) -> bool {
        self.fields().iter().any(|(_, values)| !values.is_empty())
    }

    fn fields(&self) -> [(&'static str, &Vec<String>); 12] {
        [
            ("crate", &self.crates),
            ("file", &self.files),
            ("module", &self.modules),
            ("symbol", &self.symbols),
            ("component", &self.components),
            ("dependency", &self.dependencies),
            ("concept", &self.concepts),
            ("widget_type", &self.widget_types),
            ("css_class", &self.css_classes),
            ("design_token", &self.design_tokens),
            ("diagnostic", &self.diagnostics),
            ("configuration", &self.configurations),
        ]
    }

    fn fields_mut(&mut self) -> [&mut Vec<String>; 12] {
        [
            &mut self.crates,
            &mut self.files,
            &mut self.modules,
            &mut self.symbols,
            &mut self.components,
            &mut self.dependencies,
            &mut self.concepts,
            &mut self.widget_types,
            &mut self.css_classes,
            &mut self.design_tokens,
            &mut self.diagnostics,
            &mut self.configurations,
        ]
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ValidationRecipe {
    pub kind: String,
    pub expected: String,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub procedure: Option<String>,
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProblemInput {
    pub report: String,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub defect_family: Option<String>,
    #[serde(default = "reported")]
    pub status: String,
    #[serde(default = "default_confidence")]
    pub confidence: f64,
    #[serde(default)]
    pub scope: QualityScope,
    #[serde(default)]
    pub reproduction: Option<String>,
    #[serde(default)]
    pub diagnostic_signature: Option<String>,
    #[serde(default)]
    pub root_cause: Option<String>,
    #[serde(default)]
    pub fix_reference: Option<String>,
    #[serde(default)]
    pub evidence: Vec<String>,
    #[serde(default)]
    pub related: Vec<String>,
    #[serde(default = "human_report")]
    pub provenance: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct QualityConstraintInput {
    pub rule: String,
    pub category: String,
    #[serde(default)]
    pub problem_ids: Vec<String>,
    pub scope: QualityScope,
    #[serde(default)]
    pub exclusions: QualityScope,
    #[serde(default)]
    pub activation_criteria: Vec<String>,
    pub recipe: ValidationRecipe,
    #[serde(default = "observe")]
    pub enforcement: String,
    #[serde(default = "default_confidence")]
    pub confidence: f64,
    #[serde(default = "proposed")]
    pub maturity: String,
    #[serde(default = "problem_derived")]
    pub provenance: Vec<String>,
    #[serde(default)]
    pub expires_at: Option<String>,
    #[serde(default)]
    pub invalidation_conditions: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ValidationOutcomeInput {
    pub obligation_id: String,
    pub status: String,
    #[serde(default)]
    pub evidence: Vec<String>,
}

#[derive(Default)]
struct ChangeSurface {
    scope: QualityScope,
    text: String,
}

fn reported() -> String {
    "reported".into()
}
fn observe() -> String {
    "observe".into()
}
fn proposed() -> String {
    "proposed".into()
}
fn default_confidence() -> f64 {
    0.65
}
fn human_report() -> String {
    "HumanReport".into()
}
fn problem_derived() -> Vec<String> {
    vec!["ProblemDerived".into()]
}

pub(crate) fn initialize(db: &Connection) -> Result<()> {
    db.execute_batch(SCHEMA)?;
    Ok(())
}

pub(crate) fn append_search_index(db: &Connection) -> Result<()> {
    db.execute(
        "INSERT INTO search_index(entity_type,entity_id,title,path,body) SELECT 'problem',id,summary,'',family || ' ' || original_report || ' ' || COALESCE(diagnostic_signature,'') || ' ' || COALESCE(root_cause,'') || ' ' || scope_json FROM problem_records",
        [],
    )?;
    db.execute(
        "INSERT INTO search_index(entity_type,entity_id,title,path,body) SELECT 'quality_constraint',id,rule,'',category || ' ' || scope_json || ' ' || exclusions_json || ' ' || activation_json || ' ' || recipe_json FROM quality_constraints",
        [],
    )?;
    Ok(())
}

impl Service {
    pub fn problem_record(&self, mut input: ProblemInput) -> Result<Value> {
        ensure!(
            !input.report.trim().is_empty(),
            "problem report must not be empty"
        );
        validate_choice("problem status", &input.status, PROBLEM_STATUSES)?;
        validate_confidence(input.confidence)?;
        let report = redact_text(input.report.trim());
        let summary = redact_text(
            input
                .summary
                .take()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| summarize(&report))
                .trim(),
        );
        let family = input
            .defect_family
            .take()
            .unwrap_or_else(|| classify_problem(&format!("{summary} {report}")))
            .to_ascii_lowercase();
        validate_choice("defect family", &family, FAMILIES)?;
        input.scope = infer_scope(input.scope, &family, &format!("{summary} {report}"));
        let diagnostic_signature = input
            .diagnostic_signature
            .take()
            .map(|value| redact_text(&value));
        let fingerprint = problem_fingerprint(
            &family,
            diagnostic_signature.as_deref().unwrap_or(&summary),
            &input.scope,
        );
        let existing: Option<String> = self
            .db
            .query_row(
                "SELECT id FROM problem_records WHERE fingerprint=?1 AND status!='obsolete'",
                [&fingerprint],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(id) = existing {
            self.insert_problem_occurrence(&id, &report, &input.evidence)?;
            self.insert_problem_links(&id, &input.related, "related")?;
            self.db.execute(
                "UPDATE problem_records SET updated_at=?1 WHERE id=?2",
                params![Utc::now().to_rfc3339(), id],
            )?;
            self.rebuild_search_index()?;
            return Ok(json!({
                "problem": self.problem_resource(&id)?,
                "constraint": self.constraint_for_problem(&id)?,
                "deduplicated": true,
                "snapshot": self.snapshot(),
            }));
        }
        let sequence: i64 = self.db.query_row(
            "SELECT COALESCE(MAX(sequence),0)+1 FROM problem_records",
            [],
            |row| row.get(0),
        )?;
        let id = format!("PRB-{sequence:04}");
        let now = Utc::now().to_rfc3339();
        let evidence = redact_strings(&input.evidence);
        let reproduction = input.reproduction.as_deref().map(redact_text);
        let root_cause = input.root_cause.as_deref().map(redact_text);
        let fix_reference = input.fix_reference.as_deref().map(redact_text);
        self.db.execute(
            "INSERT INTO problem_records(id,sequence,fingerprint,original_report,summary,family,status,confidence,scope_json,reproduction,diagnostic_signature,root_cause,fix_reference,evidence_json,related_json,provenance,revision,created_at,updated_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?18)",
            params![id,sequence,fingerprint,report,summary,family,input.status,input.confidence,serde_json::to_string(&input.scope)?,reproduction,diagnostic_signature,root_cause,fix_reference,serde_json::to_string(&evidence)?,serde_json::to_string(&input.related)?,redact_text(&input.provenance),self.revision().workspace_digest,now],
        )?;
        self.insert_problem_occurrence(&id, &report, &input.evidence)?;
        self.insert_problem_links(&id, &input.related, "related")?;
        let constraint = self.propose_constraint_from_problem(&id)?;
        self.rebuild_search_index()?;
        Ok(json!({
            "problem": self.problem_resource(&id)?,
            "constraint": constraint,
            "deduplicated": false,
            "snapshot": self.snapshot(),
        }))
    }

    pub fn problem_update(&self, id: &str, patch: &Value) -> Result<Value> {
        let current = self.problem_resource(id)?;
        validate_patch_keys(
            patch,
            &[
                "summary",
                "status",
                "confidence",
                "scope",
                "reproduction",
                "diagnostic_signature",
                "root_cause",
                "fix_reference",
                "evidence",
                "related",
            ],
        )?;
        let status = patch
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_else(|| current["status"].as_str().unwrap_or("reported"));
        validate_choice("problem status", status, PROBLEM_STATUSES)?;
        let confidence = patch
            .get("confidence")
            .and_then(Value::as_f64)
            .unwrap_or_else(|| current["confidence"].as_f64().unwrap_or(0.65));
        validate_confidence(confidence)?;
        let scope = patch
            .get("scope")
            .cloned()
            .unwrap_or_else(|| current["scope"].clone());
        let mut scope: QualityScope =
            serde_json::from_value(scope).context("invalid problem scope")?;
        scope.normalize();
        let text_field = |name: &str| -> Option<String> {
            patch
                .get(name)
                .or_else(|| current.get(name))
                .and_then(Value::as_str)
                .map(redact_text)
        };
        let summary = text_field("summary").context("problem summary must be a string")?;
        let evidence = patch
            .get("evidence")
            .cloned()
            .unwrap_or_else(|| current["evidence"].clone());
        let evidence = redact_value(evidence);
        let related = patch
            .get("related")
            .cloned()
            .unwrap_or_else(|| current["related"].clone());
        self.db.execute(
            "UPDATE problem_records SET summary=?1,status=?2,confidence=?3,scope_json=?4,reproduction=?5,diagnostic_signature=?6,root_cause=?7,fix_reference=?8,evidence_json=?9,related_json=?10,revision=?11,updated_at=?12 WHERE id=?13",
            params![summary,status,confidence,serde_json::to_string(&scope)?,text_field("reproduction"),text_field("diagnostic_signature"),text_field("root_cause"),text_field("fix_reference"),evidence.to_string(),related.to_string(),self.revision().workspace_digest,Utc::now().to_rfc3339(),id],
        )?;
        if let Some(related) = related.as_array() {
            let ids = related
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect::<Vec<_>>();
            self.insert_problem_links(id, &ids, "related")?;
        }
        self.rebuild_search_index()?;
        Ok(json!({"problem":self.problem_resource(id)?,"snapshot":self.snapshot()}))
    }

    pub fn problem_list(&self, query: Option<&str>, limit: usize) -> Result<Value> {
        let ids = if let Some(query) = query.filter(|query| !query.trim().is_empty()) {
            self.search_hits(&terms(query), Some("problem"), limit)?
                .into_iter()
                .filter_map(|hit| hit["entity_id"].as_str().map(str::to_owned))
                .collect::<Vec<_>>()
        } else {
            self.db
                .prepare("SELECT id FROM problem_records ORDER BY sequence DESC LIMIT ?1")?
                .query_map([limit as i64], |row| row.get(0))?
                .collect::<rusqlite::Result<_>>()?
        };
        let records = ids
            .iter()
            .map(|id| self.problem_resource(id))
            .collect::<Result<Vec<_>>>()?;
        Ok(json!({"query":query,"problems":records,"snapshot":self.snapshot()}))
    }

    pub fn quality_constraint_propose(&self, mut input: QualityConstraintInput) -> Result<Value> {
        input.scope.normalize();
        input.exclusions.normalize();
        self.insert_constraint(input)
    }

    pub fn quality_constraint_update(&self, id: &str, patch: &Value) -> Result<Value> {
        let current = self.quality_constraint_resource(id)?;
        validate_patch_keys(
            patch,
            &[
                "rule",
                "category",
                "status",
                "scope",
                "exclusions",
                "activation_criteria",
                "recipe",
                "enforcement",
                "confidence",
                "maturity",
                "expires_at",
                "invalidation_conditions",
            ],
        )?;
        let field = |name: &str| {
            patch
                .get(name)
                .cloned()
                .unwrap_or_else(|| current[name].clone())
        };
        let rule = field("rule")
            .as_str()
            .map(redact_text)
            .context("constraint rule must be a string")?;
        ensure!(!rule.trim().is_empty(), "constraint rule must not be empty");
        let category = field("category")
            .as_str()
            .context("constraint category must be a string")?
            .to_ascii_lowercase();
        validate_choice("constraint category", &category, FAMILIES)?;
        let status = field("status")
            .as_str()
            .context("constraint status must be a string")?
            .to_owned();
        validate_choice("constraint status", &status, CONSTRAINT_STATUSES)?;
        let mut maturity = field("maturity")
            .as_str()
            .context("constraint maturity must be a string")?
            .to_owned();
        if status == "active" && maturity == "proposed" {
            maturity = "approved".into();
        }
        validate_choice("constraint maturity", &maturity, MATURITIES)?;
        let enforcement = field("enforcement")
            .as_str()
            .context("constraint enforcement must be a string")?
            .to_owned();
        validate_choice("constraint enforcement", &enforcement, ENFORCEMENTS)?;
        let confidence = field("confidence")
            .as_f64()
            .context("constraint confidence must be a number")?;
        validate_confidence(confidence)?;
        let mut scope: QualityScope = serde_json::from_value(field("scope"))?;
        let mut exclusions: QualityScope = serde_json::from_value(field("exclusions"))?;
        scope.normalize();
        exclusions.normalize();
        ensure!(
            scope.has_selectors(),
            "constraint scope must contain at least one selector"
        );
        let recipe: ValidationRecipe = serde_json::from_value(field("recipe"))?;
        validate_recipe(&recipe, &enforcement)?;
        let activation = redact_value(field("activation_criteria"));
        let invalidation = redact_value(field("invalidation_conditions"));
        let expires_at = patch
            .get("expires_at")
            .or_else(|| current.get("expires_at"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        validate_expiration(expires_at.as_deref())?;
        self.db.execute(
            "UPDATE quality_constraints SET rule=?1,category=?2,status=?3,scope_json=?4,exclusions_json=?5,activation_json=?6,recipe_json=?7,enforcement=?8,confidence=?9,maturity=?10,expires_at=?11,invalidation_json=?12,revision=?13,updated_at=?14 WHERE id=?15",
            params![rule,category,status,serde_json::to_string(&scope)?,serde_json::to_string(&exclusions)?,activation.to_string(),serde_json::to_string(&redact_recipe(recipe))?,enforcement,confidence,maturity,expires_at,invalidation.to_string(),self.revision().workspace_digest,Utc::now().to_rfc3339(),id],
        )?;
        self.cancel_inactive_obligations()?;
        self.rebuild_search_index()?;
        Ok(json!({"constraint":self.quality_constraint_resource(id)?,"snapshot":self.snapshot()}))
    }

    pub fn quality_constraint_merge(&self, source: &str, target: &str) -> Result<Value> {
        ensure!(source != target, "cannot merge a constraint into itself");
        self.quality_constraint_resource(source)?;
        self.quality_constraint_resource(target)?;
        self.db.execute(
            "INSERT OR IGNORE INTO problem_quality_constraints(problem_id,constraint_id) SELECT problem_id,?1 FROM problem_quality_constraints WHERE constraint_id=?2",
            params![target, source],
        )?;
        self.db.execute(
            "UPDATE quality_constraints SET status='merged',maturity='deprecated',merged_into=?1,updated_at=?2 WHERE id=?3",
            params![target,Utc::now().to_rfc3339(),source],
        )?;
        self.cancel_inactive_obligations()?;
        self.rebuild_search_index()?;
        Ok(
            json!({"source":self.quality_constraint_resource(source)?,"target":self.quality_constraint_resource(target)?,"snapshot":self.snapshot()}),
        )
    }

    pub fn quality_constraint_list(&self, query: Option<&str>, limit: usize) -> Result<Value> {
        let ids = if let Some(query) = query.filter(|query| !query.trim().is_empty()) {
            self.search_hits(&terms(query), Some("quality_constraint"), limit)?
                .into_iter()
                .filter_map(|hit| hit["entity_id"].as_str().map(str::to_owned))
                .collect::<Vec<_>>()
        } else {
            self.db
                .prepare("SELECT id FROM quality_constraints ORDER BY sequence DESC LIMIT ?1")?
                .query_map([limit as i64], |row| row.get(0))?
                .collect::<rusqlite::Result<_>>()?
        };
        let constraints = ids
            .iter()
            .map(|id| self.quality_constraint_resource(id))
            .collect::<Result<Vec<_>>>()?;
        Ok(json!({"query":query,"constraints":constraints,"snapshot":self.snapshot()}))
    }

    pub fn quality_constraint_explain(
        &self,
        id: &str,
        change: &str,
        targets: &[String],
    ) -> Result<Value> {
        let nodes = self.nodes_for_change_targets(targets)?;
        let surface = self.change_surface(change, targets, &nodes, &[], &BTreeSet::new())?;
        let constraint = self.quality_constraint_resource(id)?;
        Ok(json!({
            "constraint": constraint,
            "change": change,
            "match": match_constraint(&constraint,&surface),
            "surface": surface.scope,
            "snapshot": self.snapshot(),
        }))
    }

    pub fn quality_validation_queue(&self, context_id: &str) -> Result<Value> {
        let context_exists: bool = self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM change_contexts WHERE id=?1)",
            [context_id],
            |row| row.get(0),
        )?;
        ensure!(context_exists, "unknown change context `{context_id}`");
        self.quality_validation_queue_unchecked(context_id)
    }

    pub fn quality_validation_record(&self, input: ValidationOutcomeInput) -> Result<Value> {
        validate_choice(
            "validation status",
            &input.status,
            &["passed", "failed", "skipped", "unavailable"],
        )?;
        let exists: bool = self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM validation_obligations WHERE id=?1)",
            [&input.obligation_id],
            |row| row.get(0),
        )?;
        ensure!(
            exists,
            "unknown validation obligation `{}`",
            input.obligation_id
        );
        let evidence = redact_strings(&input.evidence);
        let now = Utc::now().to_rfc3339();
        self.db.execute(
            "UPDATE validation_obligations SET status=?1,evidence_json=?2,updated_at=?3 WHERE id=?4",
            params![input.status,serde_json::to_string(&evidence)?,now,input.obligation_id],
        )?;
        let constraint_ids = self.obligation_constraint_ids(&input.obligation_id)?;
        for constraint_id in &constraint_ids {
            self.db.execute(
                "INSERT INTO quality_validation_history(constraint_id,obligation_id,status,evidence_json,provenance,revision,created_at) VALUES (?1,?2,?3,?4,'ValidationResult',?5,?6)",
                params![constraint_id,input.obligation_id,input.status,serde_json::to_string(&evidence)?,self.revision().workspace_digest,now],
            )?;
            if input.status == "passed" {
                self.db.execute(
                    "UPDATE quality_constraints SET last_successful_validation=?1,updated_at=?1 WHERE id=?2",
                    params![now,constraint_id],
                )?;
            }
        }
        Ok(json!({
            "obligation": self.validation_obligation_resource(&input.obligation_id)?,
            "constraints": constraint_ids,
            "snapshot": self.snapshot(),
        }))
    }

    pub(crate) fn activate_quality_constraints(
        &self,
        context_id: &str,
        intent: &str,
        targets: &[String],
        nodes: &[Node],
        references: &[Node],
    ) -> Result<Value> {
        let surface = self.change_surface(intent, targets, nodes, references, &BTreeSet::new())?;
        self.persist_matching_obligations(context_id, &surface)?;
        self.persist_change_inferred_obligations(context_id, &surface.scope.files)?;
        self.quality_validation_queue_unchecked(context_id)
    }

    pub(crate) fn activate_quality_constraints_for_diff(
        &self,
        context_id: &str,
        diff: &str,
        changed_files: &BTreeSet<String>,
    ) -> Result<Value> {
        let nodes = self
            .all_nodes()?
            .into_iter()
            .filter(|node| changed_files.contains(&node.file))
            .collect::<Vec<_>>();
        let targets = changed_files.iter().cloned().collect::<Vec<_>>();
        let surface = self.change_surface(diff, &targets, &nodes, &[], changed_files)?;
        self.persist_matching_obligations(context_id, &surface)?;
        self.persist_change_inferred_obligations(context_id, changed_files)?;
        self.quality_validation_queue_unchecked(context_id)
    }

    pub(crate) fn execute_quality_obligations(&self, context_id: &str) -> Result<Vec<Value>> {
        let queue = self.quality_validation_queue_unchecked(context_id)?;
        let obligations = queue["learned"].as_array().cloned().unwrap_or_default();
        let mut results = Vec::new();
        for obligation in obligations
            .into_iter()
            .filter(|item| item["status"] == "queued")
        {
            let recipe: ValidationRecipe = serde_json::from_value(obligation["recipe"].clone())?;
            let (status, evidence) = self.run_validation_recipe(&recipe);
            let result = self.quality_validation_record(ValidationOutcomeInput {
                obligation_id: obligation["id"].as_str().unwrap_or_default().to_owned(),
                status,
                evidence,
            })?;
            results.push(result["obligation"].clone());
        }
        Ok(results)
    }

    pub(crate) fn record_change_inferred_results(
        &self,
        context_id: &str,
        results: &[Value],
    ) -> Result<()> {
        for result in results {
            let Some(command) = result["command"].as_str() else {
                continue;
            };
            let obligation_id = self
                .db
                .query_row(
                    "SELECT id FROM validation_obligations WHERE context_id=?1 AND source='current_change' AND json_extract(recipe_json,'$.command')=?2",
                    params![context_id, command],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            let Some(obligation_id) = obligation_id else {
                continue;
            };
            let status = if result["skipped"] == true {
                "unavailable"
            } else if result["success"] == true {
                "passed"
            } else {
                "failed"
            };
            let evidence = ["reason", "error", "stdout", "stderr"]
                .into_iter()
                .filter_map(|field| result[field].as_str())
                .filter(|value| !value.trim().is_empty())
                .map(str::to_owned)
                .collect();
            self.quality_validation_record(ValidationOutcomeInput {
                obligation_id,
                status: status.into(),
                evidence,
            })?;
        }
        Ok(())
    }

    pub(crate) fn quality_counts(&self) -> Result<Value> {
        let problems: i64 =
            self.db
                .query_row("SELECT COUNT(*) FROM problem_records", [], |row| row.get(0))?;
        let constraints: i64 =
            self.db
                .query_row("SELECT COUNT(*) FROM quality_constraints", [], |row| {
                    row.get(0)
                })?;
        let queued: i64 = self.db.query_row(
            "SELECT COUNT(*) FROM validation_obligations WHERE status='queued'",
            [],
            |row| row.get(0),
        )?;
        Ok(
            json!({"problems":problems,"quality_constraints":constraints,"queued_validation_obligations":queued}),
        )
    }

    pub(crate) fn relevant_quality_constraints(&self, change: &str) -> Result<Vec<Value>> {
        let surface = self.change_surface(change, &[], &[], &[], &BTreeSet::new())?;
        let mut output = Vec::new();
        for constraint in self.all_constraint_records()? {
            let matched = match_constraint(&constraint, &surface);
            if matched["matched"] == true {
                output.push(json!({"constraint":constraint,"match":matched}));
            }
        }
        Ok(output)
    }

    pub(crate) fn quality_resource(&self, path: &str) -> Result<Option<Value>> {
        if let Some(id) = path.strip_prefix("problem/") {
            return Ok(Some(self.problem_resource(id)?));
        }
        if let Some(id) = path.strip_prefix("quality/") {
            return Ok(Some(self.quality_constraint_resource(id)?));
        }
        if let Some(id) = path.strip_prefix("validation/") {
            return Ok(Some(self.quality_validation_queue(id)?));
        }
        Ok(None)
    }

    fn insert_problem_occurrence(&self, id: &str, report: &str, evidence: &[String]) -> Result<()> {
        self.db.execute(
            "INSERT INTO problem_occurrences(problem_id,report,evidence_json,revision,created_at) VALUES (?1,?2,?3,?4,?5)",
            params![id,redact_text(report),serde_json::to_string(&redact_strings(evidence))?,self.revision().workspace_digest,Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    fn insert_problem_links(&self, id: &str, related: &[String], relationship: &str) -> Result<()> {
        for related_id in related {
            if related_id == id {
                continue;
            }
            let exists: bool = self.db.query_row(
                "SELECT EXISTS(SELECT 1 FROM problem_records WHERE id=?1)",
                [related_id],
                |row| row.get(0),
            )?;
            ensure!(exists, "unknown related problem `{related_id}`");
            self.db.execute(
                "INSERT OR IGNORE INTO problem_links(problem_id,related_id,relationship) VALUES (?1,?2,?3)",
                params![id,related_id,relationship],
            )?;
        }
        Ok(())
    }

    fn propose_constraint_from_problem(&self, problem_id: &str) -> Result<Value> {
        if let Some(existing) = self.constraint_for_problem(problem_id)? {
            return Ok(existing);
        }
        let problem = self.problem_resource(problem_id)?;
        let family = problem["defect_family"].as_str().unwrap_or("uncategorized");
        let scope: QualityScope = serde_json::from_value(problem["scope"].clone())?;
        let (rule, recipe, activation) = generated_constraint(family, &scope);
        let result = self.insert_constraint(QualityConstraintInput {
            rule,
            category: family.into(),
            problem_ids: vec![problem_id.into()],
            scope,
            exclusions: QualityScope::default(),
            activation_criteria: activation,
            recipe,
            enforcement: "observe".into(),
            confidence: problem["confidence"].as_f64().unwrap_or(0.65),
            maturity: "proposed".into(),
            provenance: vec![problem_id.into(), "DeterministicClassifier".into()],
            expires_at: None,
            invalidation_conditions: vec![
                "The affected component or validation mechanism is removed or replaced.".into(),
            ],
        })?;
        Ok(result["constraint"].clone())
    }

    fn insert_constraint(&self, mut input: QualityConstraintInput) -> Result<Value> {
        input.scope.normalize();
        input.exclusions.normalize();
        ensure!(
            !input.rule.trim().is_empty(),
            "constraint rule must not be empty"
        );
        input.category = input.category.to_ascii_lowercase();
        validate_choice("constraint category", &input.category, FAMILIES)?;
        ensure!(
            input.scope.has_selectors(),
            "constraint scope must contain at least one selector"
        );
        validate_choice("constraint enforcement", &input.enforcement, ENFORCEMENTS)?;
        validate_choice("constraint maturity", &input.maturity, MATURITIES)?;
        validate_confidence(input.confidence)?;
        validate_expiration(input.expires_at.as_deref())?;
        validate_recipe(&input.recipe, &input.enforcement)?;
        for problem_id in &input.problem_ids {
            self.problem_resource(problem_id)?;
        }
        input.rule = redact_text(&input.rule);
        input.recipe = redact_recipe(input.recipe);
        input.activation_criteria = redact_strings(&input.activation_criteria);
        input.invalidation_conditions = redact_strings(&input.invalidation_conditions);
        input.provenance = redact_strings(&input.provenance);
        let fingerprint = constraint_fingerprint(&input);
        if let Some(id) = self
            .db
            .query_row(
                "SELECT id FROM quality_constraints WHERE fingerprint=?1 AND status NOT IN ('rejected','retired','merged')",
                [&fingerprint],
                |row| row.get::<_,String>(0),
            )
            .optional()?
        {
            for problem_id in &input.problem_ids {
                self.db.execute(
                    "INSERT OR IGNORE INTO problem_quality_constraints(problem_id,constraint_id) VALUES (?1,?2)",
                    params![problem_id,id],
                )?;
            }
            return Ok(json!({"constraint":self.quality_constraint_resource(&id)?,"deduplicated":true,"snapshot":self.snapshot()}));
        }
        let sequence: i64 = self.db.query_row(
            "SELECT COALESCE(MAX(sequence),0)+1 FROM quality_constraints",
            [],
            |row| row.get(0),
        )?;
        let id = format!("QLT-{sequence:04}");
        let now = Utc::now().to_rfc3339();
        self.db.execute(
            "INSERT INTO quality_constraints(id,sequence,fingerprint,rule,category,status,scope_json,exclusions_json,activation_json,recipe_json,enforcement,confidence,maturity,provenance_json,last_successful_validation,expires_at,invalidation_json,merged_into,revision,created_at,updated_at) VALUES (?1,?2,?3,?4,?5,'proposed',?6,?7,?8,?9,?10,?11,?12,?13,NULL,?14,?15,NULL,?16,?17,?17)",
            params![id,sequence,fingerprint,input.rule,input.category,serde_json::to_string(&input.scope)?,serde_json::to_string(&input.exclusions)?,serde_json::to_string(&input.activation_criteria)?,serde_json::to_string(&input.recipe)?,input.enforcement,input.confidence,input.maturity,serde_json::to_string(&input.provenance)?,input.expires_at,serde_json::to_string(&input.invalidation_conditions)?,self.revision().workspace_digest,now],
        )?;
        for problem_id in &input.problem_ids {
            self.db.execute(
                "INSERT INTO problem_quality_constraints(problem_id,constraint_id) VALUES (?1,?2)",
                params![problem_id, id],
            )?;
        }
        self.rebuild_search_index()?;
        Ok(
            json!({"constraint":self.quality_constraint_resource(&id)?,"deduplicated":false,"snapshot":self.snapshot()}),
        )
    }

    fn problem_resource(&self, id: &str) -> Result<Value> {
        let mut value = self.db.query_row(
            "SELECT id,status,original_report,summary,family,confidence,scope_json,reproduction,diagnostic_signature,root_cause,fix_reference,evidence_json,related_json,provenance,revision,created_at,updated_at FROM problem_records WHERE id=?1",
            [id],
            |row| Ok(json!({
                "id":row.get::<_,String>(0)?,"status":row.get::<_,String>(1)?,
                "original_report":row.get::<_,String>(2)?,"summary":row.get::<_,String>(3)?,
                "defect_family":row.get::<_,String>(4)?,"confidence":row.get::<_,f64>(5)?,
                "scope":json_column(row,6),"reproduction":row.get::<_,Option<String>>(7)?,
                "diagnostic_signature":row.get::<_,Option<String>>(8)?,"root_cause":row.get::<_,Option<String>>(9)?,
                "fix_reference":row.get::<_,Option<String>>(10)?,"evidence":json_column(row,11),
                "related":json_column(row,12),"provenance":row.get::<_,String>(13)?,
                "revision":row.get::<_,String>(14)?,"created_at":row.get::<_,String>(15)?,"updated_at":row.get::<_,String>(16)?,
            })),
        ).with_context(|| format!("unknown problem `{id}`"))?;
        let occurrences: i64 = self.db.query_row(
            "SELECT COUNT(*) FROM problem_occurrences WHERE problem_id=?1",
            [id],
            |row| row.get(0),
        )?;
        value["occurrences"] = json!(occurrences);
        Ok(value)
    }

    fn quality_constraint_resource(&self, id: &str) -> Result<Value> {
        let mut value = self.db.query_row(
            "SELECT id,rule,category,status,scope_json,exclusions_json,activation_json,recipe_json,enforcement,confidence,maturity,provenance_json,last_successful_validation,expires_at,invalidation_json,merged_into,revision,created_at,updated_at FROM quality_constraints WHERE id=?1",
            [id],
            |row| Ok(json!({
                "id":row.get::<_,String>(0)?,"rule":row.get::<_,String>(1)?,"category":row.get::<_,String>(2)?,
                "status":row.get::<_,String>(3)?,"scope":json_column(row,4),"exclusions":json_column(row,5),
                "activation_criteria":json_column(row,6),"recipe":json_column(row,7),"enforcement":row.get::<_,String>(8)?,
                "confidence":row.get::<_,f64>(9)?,"maturity":row.get::<_,String>(10)?,"provenance":json_column(row,11),
                "last_successful_validation":row.get::<_,Option<String>>(12)?,"expires_at":row.get::<_,Option<String>>(13)?,
                "invalidation_conditions":json_column(row,14),"merged_into":row.get::<_,Option<String>>(15)?,
                "revision":row.get::<_,String>(16)?,"created_at":row.get::<_,String>(17)?,"updated_at":row.get::<_,String>(18)?,
            })),
        ).with_context(|| format!("unknown quality constraint `{id}`"))?;
        let problems = self.db.prepare(
            "SELECT problem_id FROM problem_quality_constraints WHERE constraint_id=?1 ORDER BY problem_id",
        )?.query_map([id], |row| row.get(0))?.collect::<rusqlite::Result<Vec<String>>>()?;
        let history = self.db.prepare(
            "SELECT obligation_id,status,evidence_json,provenance,revision,created_at FROM quality_validation_history WHERE constraint_id=?1 ORDER BY id DESC LIMIT 20",
        )?.query_map([id], |row| Ok(json!({"obligation_id":row.get::<_,String>(0)?,"status":row.get::<_,String>(1)?,"evidence":json_column(row,2),"provenance":row.get::<_,String>(3)?,"revision":row.get::<_,String>(4)?,"created_at":row.get::<_,String>(5)?})))?.collect::<rusqlite::Result<Vec<_>>>()?;
        value["problem_ids"] = json!(problems);
        value["validation_history"] = json!(history);
        Ok(value)
    }

    fn constraint_for_problem(&self, problem_id: &str) -> Result<Option<Value>> {
        let id = self.db.query_row(
            "SELECT constraint_id FROM problem_quality_constraints WHERE problem_id=?1 ORDER BY constraint_id LIMIT 1",
            [problem_id],
            |row| row.get::<_,String>(0),
        ).optional()?;
        id.map(|id| self.quality_constraint_resource(&id))
            .transpose()
    }

    fn all_constraint_records(&self) -> Result<Vec<Value>> {
        let ids = self
            .db
            .prepare("SELECT id FROM quality_constraints ORDER BY sequence")?
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ids.iter()
            .map(|id| self.quality_constraint_resource(id))
            .collect()
    }

    fn nodes_for_change_targets(&self, targets: &[String]) -> Result<Vec<Node>> {
        let mut nodes = Vec::new();
        for target in targets {
            if let Some(exact) = self.node_by_canonical_name(target)? {
                nodes.push(exact);
            } else {
                nodes.extend(self.search_nodes(&terms(target), 8)?);
            }
        }
        let mut seen = BTreeSet::new();
        nodes.retain(|node| seen.insert(node.id));
        Ok(nodes)
    }

    fn change_surface(
        &self,
        text: &str,
        targets: &[String],
        nodes: &[Node],
        references: &[Node],
        changed_files: &BTreeSet<String>,
    ) -> Result<ChangeSurface> {
        let mut surface = ChangeSurface {
            text: format!("{text} {}", targets.join(" ")),
            ..Default::default()
        };
        surface.scope.concepts.push("workspace".into());
        for term in terms(&surface.text) {
            surface.scope.concepts.push(term);
        }
        for target in targets {
            let path = self.root.join(target);
            if path.is_file() {
                surface.scope.files.push(target.clone());
                collect_artifact_surface(&path, &mut surface);
            }
        }
        surface.scope.files.extend(changed_files.iter().cloned());
        for node in nodes.iter().chain(references) {
            surface.scope.files.push(node.file.clone());
            surface.scope.symbols.push(node.canonical_name.clone());
            if let Some(crate_name) = &node.crate_name {
                surface.scope.crates.push(crate_name.clone());
            }
            if let Some((module, _)) = node.canonical_name.rsplit_once("::") {
                surface.scope.modules.push(module.into());
            }
        }
        for file in surface.scope.files.clone() {
            let path = Path::new(&file);
            if let Some(extension) = path.extension().and_then(|value| value.to_str()) {
                surface.scope.configurations.push(extension.into());
                if matches!(extension, "ui" | "blp" | "css" | "scss" | "xml") {
                    surface.scope.concepts.push("gtk-ui".into());
                }
                if matches!(extension, "rs" | "toml") {
                    surface.scope.concepts.push("rust-build".into());
                }
            }
        }
        let crates = surface.scope.crates.clone();
        for crate_name in crates {
            let mut statement = self.db.prepare(
                "SELECT target FROM package_dependencies WHERE source=?1 UNION SELECT source FROM package_dependencies WHERE target=?1",
            )?;
            surface.scope.dependencies.extend(
                statement
                    .query_map([crate_name], |row| row.get(0))?
                    .filter_map(Result::ok),
            );
        }
        surface.scope.normalize();
        Ok(surface)
    }

    fn persist_matching_obligations(
        &self,
        context_id: &str,
        surface: &ChangeSurface,
    ) -> Result<()> {
        self.retire_expired_constraints()?;
        let mut groups: BTreeMap<String, Vec<(Value, Value)>> = BTreeMap::new();
        for constraint in self.all_constraint_records()? {
            if constraint["status"] != "active"
                || !matches!(
                    constraint["maturity"].as_str(),
                    Some("approved" | "established")
                )
            {
                continue;
            }
            let matched = match_constraint(&constraint, surface);
            if matched["matched"] != true {
                continue;
            }
            let key = serde_json::to_string(&constraint["recipe"])?;
            groups.entry(key).or_default().push((constraint, matched));
        }
        for (recipe_key, matches) in groups {
            let obligation_id = format!(
                "OBL-{}",
                &blake3::hash(format!("{context_id}:{recipe_key}").as_bytes()).to_hex()[..12]
            );
            let recipe = matches[0].0["recipe"].clone();
            let expected = recipe["expected"].as_str().unwrap_or("validation succeeds");
            let enforcement = matches
                .iter()
                .map(|(constraint, _)| constraint["enforcement"].as_str().unwrap_or("observe"))
                .max_by_key(|value| enforcement_rank(value))
                .unwrap_or("observe");
            let blocking = enforcement == "block";
            let constraint_ids = matches
                .iter()
                .filter_map(|(constraint, _)| constraint["id"].as_str())
                .collect::<Vec<_>>();
            let reasons = matches
                .iter()
                .flat_map(|(_, matched)| matched["reasons"].as_array().cloned().unwrap_or_default())
                .collect::<Vec<_>>();
            let surfaces = matches
                .iter()
                .flat_map(|(_, matched)| {
                    matched["matched_surfaces"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default()
                })
                .collect::<Vec<_>>();
            let selected_reason = format!(
                "Learned constraints {} matched: {}",
                constraint_ids.join(", "),
                reasons
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join("; ")
            );
            let now = Utc::now().to_rfc3339();
            self.db.execute(
                "INSERT INTO validation_obligations(id,context_id,source,selected_reason,matched_surfaces_json,recipe_json,expected,enforcement,status,blocking,evidence_json,created_at,updated_at) VALUES (?1,?2,'learned_quality_constraint',?3,?4,?5,?6,?7,'queued',?8,'[]',?9,?9) ON CONFLICT(id) DO UPDATE SET selected_reason=excluded.selected_reason,matched_surfaces_json=excluded.matched_surfaces_json,enforcement=excluded.enforcement,blocking=excluded.blocking,updated_at=excluded.updated_at",
                params![obligation_id,context_id,selected_reason,serde_json::to_string(&surfaces)?,recipe.to_string(),expected,enforcement,blocking,now],
            )?;
            for constraint_id in constraint_ids {
                self.db.execute(
                    "INSERT OR IGNORE INTO validation_obligation_constraints(obligation_id,constraint_id) VALUES (?1,?2)",
                    params![obligation_id,constraint_id],
                )?;
            }
        }
        Ok(())
    }

    fn persist_change_inferred_obligations(
        &self,
        context_id: &str,
        files: impl IntoIterator<Item = impl AsRef<str>>,
    ) -> Result<()> {
        for file in files {
            let file = file.as_ref();
            let Some((program, arguments)) = super::artifact_validator(Path::new(file)) else {
                continue;
            };
            let command = std::iter::once(program.to_owned())
                .chain(
                    arguments
                        .into_iter()
                        .map(|argument| argument.to_string_lossy().into_owned()),
                )
                .collect::<Vec<_>>()
                .join(" ");
            let recipe = ValidationRecipe {
                kind: if file.ends_with(".ui") || file.ends_with(".blp") {
                    "widget_tree".into()
                } else {
                    "structural_source".into()
                },
                expected: format!("{file} passes its native artifact validator."),
                command: Some(command),
                procedure: None,
                environment: BTreeMap::new(),
            };
            let id = format!(
                "OBL-{}",
                &blake3::hash(format!("{context_id}:change:{file}").as_bytes()).to_hex()[..12]
            );
            let now = Utc::now().to_rfc3339();
            self.db.execute(
                "INSERT INTO validation_obligations(id,context_id,source,selected_reason,matched_surfaces_json,recipe_json,expected,enforcement,status,blocking,evidence_json,created_at,updated_at) VALUES (?1,?2,'current_change',?3,?4,?5,?6,'validate','queued',0,'[]',?7,?7) ON CONFLICT(id) DO UPDATE SET selected_reason=excluded.selected_reason,matched_surfaces_json=excluded.matched_surfaces_json,recipe_json=excluded.recipe_json,expected=excluded.expected,updated_at=excluded.updated_at",
                params![id,context_id,format!("Changed artifact `{file}` has a repository-supported native validator."),json!([{"kind":"file","selector":file,"matched":file}]).to_string(),serde_json::to_string(&recipe)?,recipe.expected,now],
            )?;
        }
        Ok(())
    }

    fn quality_validation_queue_unchecked(&self, context_id: &str) -> Result<Value> {
        let mut statement = self.db.prepare(
            "SELECT id,source,selected_reason,matched_surfaces_json,recipe_json,expected,enforcement,status,blocking,evidence_json,created_at,updated_at FROM validation_obligations WHERE context_id=?1 ORDER BY CASE json_extract(recipe_json,'$.kind') WHEN 'focused_test' THEN 0 WHEN 'structural_source' THEN 1 WHEN 'widget_tree' THEN 2 WHEN 'cargo' THEN 3 WHEN 'runtime_log' THEN 4 WHEN 'ui_smoke' THEN 5 WHEN 'visual_comparison' THEN 6 ELSE 7 END,blocking DESC,id",
        )?;
        let mut obligations = statement
            .query_map([context_id], obligation_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for obligation in &mut obligations {
            let id = obligation["id"].as_str().unwrap_or_default();
            obligation["constraint_ids"] = json!(self.obligation_constraint_ids(id)?);
        }
        let (learned, change_inferred): (Vec<_>, Vec<_>) = obligations
            .into_iter()
            .partition(|obligation| obligation["source"] == "learned_quality_constraint");
        let queued_commands = learned
            .iter()
            .chain(&change_inferred)
            .filter_map(|obligation| obligation["recipe"]["command"].as_str())
            .collect::<BTreeSet<_>>();
        let repository_policy = [
            ("cargo fmt --check", "formatting is clean"),
            (
                "cargo check --all-targets",
                "workspace checks without compiler warnings",
            ),
            (
                "cargo clippy --all-targets -- -D warnings",
                "static analysis is warning-free",
            ),
            ("cargo test", "repository tests pass"),
        ]
        .into_iter()
        .filter(|(command, _)| !queued_commands.contains(command))
        .map(|(command, expected)| json!({"command":command,"expected":expected}))
        .collect::<Vec<_>>();
        Ok(
            json!({"context_id":context_id,"repository_policy":repository_policy,"learned":learned,"change_inferred":change_inferred,"snapshot":self.snapshot()}),
        )
    }

    fn validation_obligation_resource(&self, id: &str) -> Result<Value> {
        let mut value = self.db.query_row(
            "SELECT id,source,selected_reason,matched_surfaces_json,recipe_json,expected,enforcement,status,blocking,evidence_json,created_at,updated_at FROM validation_obligations WHERE id=?1",
            [id], obligation_row,
        ).with_context(|| format!("unknown validation obligation `{id}`"))?;
        value["constraint_ids"] = json!(self.obligation_constraint_ids(id)?);
        Ok(value)
    }

    fn obligation_constraint_ids(&self, id: &str) -> Result<Vec<String>> {
        Ok(self.db.prepare(
            "SELECT constraint_id FROM validation_obligation_constraints WHERE obligation_id=?1 ORDER BY constraint_id",
        )?.query_map([id], |row| row.get(0))?.collect::<rusqlite::Result<_>>()?)
    }

    fn cancel_inactive_obligations(&self) -> Result<()> {
        self.db.execute(
            "UPDATE validation_obligations SET status='cancelled',updated_at=?1 WHERE status='queued' AND NOT EXISTS (SELECT 1 FROM validation_obligation_constraints link JOIN quality_constraints qc ON qc.id=link.constraint_id WHERE link.obligation_id=validation_obligations.id AND qc.status='active' AND qc.maturity IN ('approved','established'))",
            [Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    fn retire_expired_constraints(&self) -> Result<()> {
        let now = Utc::now();
        let ids = self.db.prepare(
            "SELECT id,expires_at FROM quality_constraints WHERE status='active' AND expires_at IS NOT NULL",
        )?.query_map([], |row| Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?)))?.filter_map(Result::ok).filter_map(|(id,expiration)| chrono::DateTime::parse_from_rfc3339(&expiration).ok().filter(|expiration| *expiration <= now).map(|_| id)).collect::<Vec<_>>();
        for id in ids {
            self.db.execute("UPDATE quality_constraints SET status='retired',maturity='deprecated',updated_at=?1 WHERE id=?2",params![now.to_rfc3339(),id])?;
        }
        self.cancel_inactive_obligations()
    }

    fn run_validation_recipe(&self, recipe: &ValidationRecipe) -> (String, Vec<String>) {
        let Some(command) = recipe.command.as_deref() else {
            return (
                "unavailable".into(),
                vec![format!(
                    "No deterministic command is configured. Procedure: {}",
                    recipe
                        .procedure
                        .as_deref()
                        .unwrap_or("manual review required")
                )],
            );
        };
        let parts = command.split_whitespace().collect::<Vec<_>>();
        let Some(program) = parts.first().copied() else {
            return (
                "unavailable".into(),
                vec!["Validation command is empty".into()],
            );
        };
        if !matches!(
            program,
            "cargo" | "gtk4-builder-tool" | "blueprint-compiler" | "xmllint"
        ) || parts.iter().any(|part| {
            part.chars()
                .any(|character| ";&|><`$()".contains(character))
        }) {
            return (
                "unavailable".into(),
                vec![format!(
                    "Command `{}` is outside the executable validation allowlist",
                    redact_text(command)
                )],
            );
        }
        let mut process = Command::new(program);
        process
            .args(&parts[1..])
            .current_dir(&self.root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in &recipe.environment {
            if key.chars().all(|character| {
                character.is_ascii_uppercase() || character.is_ascii_digit() || character == '_'
            }) {
                process.env(key, value);
            }
        }
        match process.output() {
            Ok(output) => {
                let mut evidence = Vec::new();
                let stdout = trim_text(&String::from_utf8_lossy(&output.stdout), 4_000);
                let stderr = trim_text(&String::from_utf8_lossy(&output.stderr), 4_000);
                if !stdout.trim().is_empty() {
                    evidence.push(format!("stdout: {}", redact_text(&stdout)));
                }
                if !stderr.trim().is_empty() {
                    evidence.push(format!("stderr: {}", redact_text(&stderr)));
                }
                if evidence.is_empty() {
                    evidence.push(format!(
                        "`{}` exited with {}",
                        redact_text(command),
                        output.status
                    ));
                }
                (
                    if output.status.success() {
                        "passed"
                    } else {
                        "failed"
                    }
                    .into(),
                    evidence,
                )
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (
                "unavailable".into(),
                vec![format!("`{program}` is not installed")],
            ),
            Err(error) => ("failed".into(), vec![redact_text(&error.to_string())]),
        }
    }
}

fn generated_constraint(
    family: &str,
    _scope: &QualityScope,
) -> (String, ValidationRecipe, Vec<String>) {
    match family {
        "gtk_diagnostic" => (
            "Representative UI runs for the scoped GTK surfaces emit no Gtk-WARNING, Adwaita-WARNING, or critical diagnostics.".into(),
            ValidationRecipe { kind:"runtime_log".into(), expected:"No Gtk-WARNING, Adwaita-WARNING, GLib critical, or GTK critical diagnostic is emitted.".into(), command:None, procedure:Some("Run the repository's representative GTK UI smoke scenario with G_DEBUG=fatal-warnings (or the repository-equivalent fatal diagnostic mechanism).".into()), environment:BTreeMap::from([("G_DEBUG".into(),"fatal-warnings".into())]) },
            vec!["A GTK UI artifact, widget, or component in scope changes.".into()],
        ),
        "compiler_warning" => (
            "The scoped Rust package or workspace builds without compiler warnings.".into(),
            ValidationRecipe { kind:"cargo".into(), expected:"Cargo check succeeds without compiler warnings.".into(), command:Some("cargo check --workspace --all-targets".into()), procedure:None, environment:BTreeMap::new() },
            vec!["Rust source, Cargo configuration, dependency versions, or scoped packages change.".into()],
        ),
        "clippy_warning" => (
            "The scoped Rust package or workspace is Clippy-warning-free.".into(),
            ValidationRecipe { kind:"cargo".into(), expected:"Clippy succeeds with warnings denied.".into(), command:Some("cargo clippy --workspace --all-targets -- -D warnings".into()), procedure:None, environment:BTreeMap::new() },
            vec!["Rust source, features, targets, or dependencies in scope change.".into()],
        ),
        "action_wiring" => (
            "Enabled controls in the scoped component have a reachable handler and an observable outcome.".into(),
            manual_recipe("Exercise the scoped control and verify its enabled action reaches the intended handler and produces the documented observable outcome."),
            vec!["The scoped action, widget, handler, or component changes.".into()],
        ),
        "visual_consistency" => (
            "Scoped UI components use their approved centralized spacing, sizing, component, or design token.".into(),
            ValidationRecipe { kind:"structural_source".into(), expected:"The scoped component uses its approved centralized style/component token; visual comparison has no unexplained drift.".into(), command:None, procedure:Some("Inspect the scoped component/style token structurally, then run the repository's focused visual smoke check if available.".into()), environment:BTreeMap::new() },
            vec!["The named component, CSS class, design token, or narrowly scoped visual role changes.".into()],
        ),
        "dynamic_text_markup" => (
            "Untrusted or dynamic text in the scoped UI surface is rendered as plain text unless markup is explicitly validated.".into(),
            ValidationRecipe { kind:"structural_source".into(), expected:"Dynamic text reaches a plain-text rendering API or is explicitly escaped and validated before markup interpretation.".into(), command:None, procedure:Some("Trace the scoped dynamic value to the widget property and verify markup interpretation is disabled or the value is escaped by the approved helper.".into()), environment:BTreeMap::new() },
            vec!["Dynamic text rendering, Pango markup, labels, or the scoped widget changes.".into()],
        ),
        "database_migration" => (
            "The scoped database change migrates forward and exercises repository-supported compatibility paths.".into(),
            manual_recipe("Run the repository's focused migration test against the supported previous schema and a fresh database."),
            vec!["Migration files, database models, queries, or persistence dependencies change.".into()],
        ),
        "performance_regression" => (
            "The scoped workload remains within its recorded performance budget.".into(),
            ValidationRecipe { kind:"manual".into(), expected:"A representative release-mode benchmark remains within the approved regression budget.".into(), command:None, procedure:Some("Run the recorded release benchmark on a comparable environment and compare its distribution with the accepted baseline.".into()), environment:BTreeMap::new() },
            vec!["The scoped algorithm, dependency, configuration, or workload changes.".into()],
        ),
        "test_regression" => (
            "The scoped regression remains covered and passing.".into(),
            ValidationRecipe { kind:"focused_test".into(), expected:"The repository test suite, including the focused regression, passes.".into(), command:Some("cargo test --workspace".into()), procedure:None, environment:BTreeMap::new() },
            vec!["The tested symbol, behavior, package, or dependency changes.".into()],
        ),
        _ => (
            "The scoped runtime diagnostic does not recur under its minimal reproduction.".into(),
            manual_recipe("Run the recorded minimal reproduction and assert that its diagnostic signature is absent."),
            vec!["The scoped component or recorded diagnostic path changes.".into()],
        ),
    }
}

fn manual_recipe(procedure: &str) -> ValidationRecipe {
    ValidationRecipe {
        kind: "manual".into(),
        expected: "The recorded defect category does not recur.".into(),
        command: None,
        procedure: Some(procedure.into()),
        environment: BTreeMap::new(),
    }
}

fn infer_scope(mut scope: QualityScope, family: &str, text: &str) -> QualityScope {
    let lower = text.to_ascii_lowercase();
    match family {
        "gtk_diagnostic" => scope.concepts.push("gtk-ui".into()),
        "compiler_warning" | "clippy_warning" => {
            if scope.crates.is_empty() {
                scope.concepts.push("workspace-build".into());
            }
        }
        "visual_consistency" => {
            if lower.contains("sidebar") && lower.contains("button") {
                scope
                    .components
                    .push("sidebar-navigation-action-button".into());
                scope.concepts.push("full-width-navigation-action".into());
            } else if scope.components.is_empty()
                && scope.css_classes.is_empty()
                && scope.design_tokens.is_empty()
            {
                scope.concepts.push("scoped-visual-component".into());
            }
        }
        "dynamic_text_markup" => scope.concepts.push("dynamic-ui-text".into()),
        "action_wiring" => scope.concepts.push("enabled-action".into()),
        "database_migration" => scope.concepts.push("database-migration".into()),
        "performance_regression" => scope.concepts.push("performance-budget".into()),
        "test_regression" => scope.concepts.push("regression-test".into()),
        _ => scope.concepts.push(family.replace('_', "-")),
    }
    scope.normalize();
    scope
}

fn classify_problem(text: &str) -> String {
    let text = text.to_ascii_lowercase();
    if text.contains("gtk-warning")
        || text.contains("adwaita-warning")
        || text.contains("gtk critical")
        || text.contains("libadwaita")
    {
        "gtk_diagnostic"
    } else if text.contains("clippy") || text.contains("static analysis warning") {
        "clippy_warning"
    } else if text.contains("compiler warning")
        || text.contains("rustc warning")
        || text.contains("warning emitted by cargo")
    {
        "compiler_warning"
    } else if text.contains("pango")
        || text.contains("markup")
            && (text.contains("dynamic") || text.contains("untrusted") || text.contains("email"))
    {
        "dynamic_text_markup"
    } else if (text.contains("button") || text.contains("control") || text.contains("action"))
        && (text.contains("non-functional")
            || text.contains("does not work")
            || text.contains("not work")
            || text.contains("handler"))
    {
        "action_wiring"
    } else if text.contains("padding")
        || text.contains("spacing")
        || text.contains("sizing")
        || text.contains("visual consistency")
        || text.contains("layout")
    {
        "visual_consistency"
    } else if text.contains("migration")
        || text.contains("database failure")
        || text.contains("sqlite")
        || text.contains("postgres")
    {
        "database_migration"
    } else if text.contains("performance")
        || text.contains("latency")
        || text.contains("regression") && text.contains("slow")
    {
        "performance_regression"
    } else if text.contains("test regression") || text.contains("test fail") {
        "test_regression"
    } else if text.contains("warning")
        || text.contains("critical")
        || text.contains("diagnostic")
        || text.contains("panic")
    {
        "runtime_diagnostic"
    } else {
        "uncategorized"
    }
    .into()
}

fn match_constraint(constraint: &Value, surface: &ChangeSurface) -> Value {
    let scope: QualityScope =
        serde_json::from_value(constraint["scope"].clone()).unwrap_or_default();
    let exclusions: QualityScope =
        serde_json::from_value(constraint["exclusions"].clone()).unwrap_or_default();
    let (reasons, matched_surfaces) = scope_matches(&scope, &surface.scope);
    let (excluded, exclusion_surfaces) = scope_matches(&exclusions, &surface.scope);
    let category = constraint["category"].as_str().unwrap_or("uncategorized");
    let category_active = category_gate(category, &surface.scope, &reasons);
    let eligible = constraint["status"] == "active"
        && matches!(
            constraint["maturity"].as_str(),
            Some("approved" | "established")
        );
    let matched = eligible && !reasons.is_empty() && excluded.is_empty() && category_active;
    json!({
        "matched": matched,
        "eligible": eligible,
        "category_active": category_active,
        "reasons": reasons,
        "matched_surfaces": matched_surfaces,
        "excluded_by": excluded,
        "excluded_surfaces": exclusion_surfaces,
        "explanation": if matched { "Constraint scope and category activation matched the semantic change surface." } else if !eligible { "Constraint is not approved and active." } else if !excluded.is_empty() { "A constraint exclusion matched the change surface." } else { "No applicable scoped selector and category activation matched." },
    })
}

fn scope_matches(scope: &QualityScope, surface: &QualityScope) -> (Vec<String>, Vec<Value>) {
    let mut reasons = Vec::new();
    let mut surfaces = Vec::new();
    for ((field, selectors), (_, candidates)) in scope.fields().into_iter().zip(surface.fields()) {
        for selector in selectors {
            if let Some(candidate) = candidates
                .iter()
                .find(|candidate| selector_matches(field, selector, candidate))
            {
                reasons.push(format!("{field} `{selector}` matched `{candidate}`"));
                surfaces.push(json!({"kind":field,"selector":selector,"matched":candidate}));
            }
        }
    }
    reasons.sort();
    reasons.dedup();
    surfaces.sort_by_key(Value::to_string);
    surfaces.dedup();
    (reasons, surfaces)
}

fn selector_matches(field: &str, selector: &str, candidate: &str) -> bool {
    let selector = normalize_selector(selector);
    let candidate = normalize_selector(candidate);
    if field == "file" {
        return wildcard_match(&selector, &candidate)
            || candidate.starts_with(selector.trim_end_matches('/'));
    }
    selector == candidate
        || (selector.len() > 3 && (candidate.contains(&selector) || selector.contains(&candidate)))
}

fn category_gate(category: &str, surface: &QualityScope, reasons: &[String]) -> bool {
    let has = |value: &str| {
        surface
            .concepts
            .iter()
            .any(|concept| normalize_selector(concept).contains(value))
    };
    match category {
        "gtk_diagnostic" => {
            has("gtk-ui")
                || reasons.iter().any(|reason| {
                    reason.starts_with("component")
                        || reason.starts_with("widget_type")
                        || reason.starts_with("file")
                })
        }
        "compiler_warning" | "clippy_warning" => {
            has("rust-build") || has("workspace") || !surface.crates.is_empty()
        }
        "visual_consistency" => reasons.iter().any(|reason| {
            [
                "component",
                "css_class",
                "design_token",
                "widget_type",
                "file",
            ]
            .iter()
            .any(|kind| reason.starts_with(kind))
        }),
        "dynamic_text_markup" => {
            has("dynamic-ui-text")
                || reasons
                    .iter()
                    .any(|reason| reason.starts_with("component") || reason.starts_with("symbol"))
        }
        _ => true,
    }
}

fn collect_artifact_surface(path: &Path, surface: &mut ChangeSurface) {
    let Ok(text) = fs::read_to_string(path) else {
        return;
    };
    surface.text.push(' ');
    surface.text.push_str(&trim_text(&text, 48_000));
    let widget = Regex::new(r"\b(?:Gtk|Adw)[A-Za-z0-9_]+\b").expect("widget regex");
    surface.scope.widget_types.extend(
        widget
            .find_iter(&text)
            .map(|value| value.as_str().to_owned()),
    );
    let css = Regex::new(r"\.([A-Za-z_][A-Za-z0-9_-]*)").expect("css regex");
    surface.scope.css_classes.extend(
        css.captures_iter(&text)
            .filter_map(|capture| capture.get(1).map(|value| value.as_str().to_owned())),
    );
    surface.scope.concepts.extend(terms(&text));
}

fn problem_fingerprint(family: &str, signature: &str, scope: &QualityScope) -> String {
    let basis = if family == "gtk_diagnostic" {
        "gtk-warning-critical"
    } else {
        signature
    };
    format!(
        "b3:{}",
        blake3::hash(
            format!(
                "{family}:{}:{}",
                normalize_selector(basis),
                serde_json::to_string(scope).unwrap_or_default()
            )
            .as_bytes()
        )
        .to_hex()
    )
}

fn constraint_fingerprint(input: &QualityConstraintInput) -> String {
    format!(
        "b3:{}",
        blake3::hash(
            format!(
                "{}:{}:{}:{}",
                input.category,
                normalize_selector(&input.rule),
                serde_json::to_string(&input.scope).unwrap_or_default(),
                serde_json::to_string(&input.recipe).unwrap_or_default()
            )
            .as_bytes()
        )
        .to_hex()
    )
}

fn summarize(report: &str) -> String {
    let first = report
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or(report)
        .trim();
    trim_text(first, 240)
}

fn normalize_selector(value: &str) -> String {
    value
        .to_ascii_lowercase()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

fn wildcard_match(pattern: &str, value: &str) -> bool {
    if !pattern.contains('*') {
        return pattern == value;
    }
    let parts = pattern.split('*').collect::<Vec<_>>();
    let mut rest = value;
    for (index, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        let Some(position) = rest.find(part) else {
            return false;
        };
        if index == 0 && !pattern.starts_with('*') && position != 0 {
            return false;
        }
        rest = &rest[position + part.len()..];
    }
    pattern.ends_with('*') || rest.is_empty()
}

fn validate_choice(label: &str, value: &str, allowed: &[&str]) -> Result<()> {
    if !allowed.contains(&value) {
        bail!(
            "unsupported {label} `{value}`; expected one of {}",
            allowed.join(", ")
        );
    }
    Ok(())
}

fn validate_confidence(value: f64) -> Result<()> {
    ensure!(
        value.is_finite() && (0.0..=1.0).contains(&value),
        "confidence must be between 0.0 and 1.0"
    );
    Ok(())
}

fn validate_expiration(value: Option<&str>) -> Result<()> {
    if let Some(value) = value {
        chrono::DateTime::parse_from_rfc3339(value).context("expires_at must be RFC 3339")?;
    }
    Ok(())
}

fn validate_recipe(recipe: &ValidationRecipe, enforcement: &str) -> Result<()> {
    validate_choice("validation recipe kind", &recipe.kind, RECIPE_KINDS)?;
    ensure!(
        !recipe.expected.trim().is_empty(),
        "validation recipe expected result must not be empty"
    );
    ensure!(
        recipe
            .command
            .as_ref()
            .is_some_and(|command| !command.trim().is_empty())
            || recipe
                .procedure
                .as_ref()
                .is_some_and(|procedure| !procedure.trim().is_empty()),
        "validation recipe needs a command or procedure"
    );
    if enforcement == "block" {
        ensure!(
            recipe.command.is_some(),
            "blocking constraints require a deterministic command"
        );
        ensure!(
            !matches!(recipe.kind.as_str(), "manual" | "visual_comparison"),
            "manual and visual-only recipes cannot block completion"
        );
    }
    Ok(())
}

fn validate_patch_keys(patch: &Value, allowed: &[&str]) -> Result<()> {
    let object = patch.as_object().context("patch must be an object")?;
    for key in object.keys() {
        ensure!(
            allowed.contains(&key.as_str()),
            "unsupported patch field `{key}`"
        );
    }
    Ok(())
}

fn redact_text(value: &str) -> String {
    let mut output = value.to_owned();
    for (pattern, replacement) in [
        (
            r"(?i)\b[A-Z0-9._%+-]+@[A-Z0-9.-]+\.[A-Z]{2,}\b",
            "[REDACTED_EMAIL]",
        ),
        (
            r"(?i)\b(?:gh[pousr]_[A-Za-z0-9_]{10,}|sk-[A-Za-z0-9_-]{10,}|AKIA[A-Z0-9]{16})\b",
            "[REDACTED_SECRET]",
        ),
        (
            r"(?i)\bbearer\s+[A-Za-z0-9._~+/=-]+",
            "Bearer [REDACTED_SECRET]",
        ),
        (
            r"(?i)\b(password|passwd|token|api[_-]?key|secret)\s*[:=]\s*[^\s,;]+",
            "$1=[REDACTED_SECRET]",
        ),
    ] {
        output = Regex::new(pattern)
            .expect("redaction regex")
            .replace_all(&output, replacement)
            .into_owned();
    }
    output
}

fn redact_strings(values: &[String]) -> Vec<String> {
    values.iter().map(|value| redact_text(value)).collect()
}

fn redact_value(value: Value) -> Value {
    match value {
        Value::String(value) => Value::String(redact_text(&value)),
        Value::Array(values) => Value::Array(values.into_iter().map(redact_value).collect()),
        Value::Object(values) => Value::Object(
            values
                .into_iter()
                .map(|(key, value)| (key, redact_value(value)))
                .collect(),
        ),
        value => value,
    }
}

fn redact_recipe(mut recipe: ValidationRecipe) -> ValidationRecipe {
    recipe.expected = redact_text(&recipe.expected);
    recipe.command = recipe.command.map(|value| redact_text(&value));
    recipe.procedure = recipe.procedure.map(|value| redact_text(&value));
    recipe.environment = recipe
        .environment
        .into_iter()
        .map(|(key, value)| (key, redact_text(&value)))
        .collect();
    recipe
}

fn json_column(row: &rusqlite::Row<'_>, index: usize) -> Value {
    row.get::<_, String>(index)
        .ok()
        .and_then(|value| serde_json::from_str(&value).ok())
        .unwrap_or(Value::Null)
}

fn obligation_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    Ok(json!({
        "id":row.get::<_,String>(0)?,"source":row.get::<_,String>(1)?,"why_selected":row.get::<_,String>(2)?,
        "matched_surfaces":json_column(row,3),"recipe":json_column(row,4),"expected":row.get::<_,String>(5)?,
        "enforcement":row.get::<_,String>(6)?,"status":row.get::<_,String>(7)?,"blocking":row.get::<_,bool>(8)?,
        "evidence":json_column(row,9),"created_at":row.get::<_,String>(10)?,"updated_at":row.get::<_,String>(11)?,
    }))
}

fn enforcement_rank(value: &str) -> usize {
    match value {
        "block" => 2,
        "validate" => 1,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn fixture() -> tempfile::TempDir {
        let directory = tempdir().unwrap();
        fs::write(
            directory.path().join("Cargo.toml"),
            "[package]\nname='quality-demo'\nversion='0.1.0'\nedition='2024'\n",
        )
        .unwrap();
        fs::create_dir(directory.path().join("src")).unwrap();
        fs::write(directory.path().join("src/lib.rs"), "pub fn run() {}\n").unwrap();
        fs::create_dir(directory.path().join("data")).unwrap();
        fs::write(
            directory.path().join("data/window.ui"),
            "<interface><object class=\"GtkButton\" id=\"compose\"/></interface>\n",
        )
        .unwrap();
        directory
    }

    fn gtk_problem(service: &Service) -> Value {
        service
            .problem_record(ProblemInput {
                report: "Fix Gtk-WARNING and Adwaita-WARNING messages from the last UI run".into(),
                summary: None,
                defect_family: None,
                status: "reported".into(),
                confidence: 0.8,
                scope: QualityScope {
                    files: vec!["data/window.ui".into()],
                    components: vec!["main-window".into()],
                    ..Default::default()
                },
                reproduction: Some("Launch the main window".into()),
                diagnostic_signature: Some("Gtk-WARNING".into()),
                root_cause: None,
                fix_reference: None,
                evidence: vec![],
                related: vec![],
                provenance: "HumanReport".into(),
            })
            .unwrap()
    }

    #[test]
    fn gtk_report_proposes_runtime_diagnostic_constraint() {
        let directory = fixture();
        let service = Service::open(directory.path()).unwrap();
        let recorded = gtk_problem(&service);
        assert_eq!(recorded["problem"]["defect_family"], "gtk_diagnostic");
        assert_eq!(recorded["constraint"]["status"], "proposed");
        assert_eq!(recorded["constraint"]["recipe"]["kind"], "runtime_log");
        assert_eq!(
            recorded["constraint"]["recipe"]["environment"]["G_DEBUG"],
            "fatal-warnings"
        );
    }

    #[test]
    fn approved_gtk_constraint_queues_for_later_ui_change_with_explanation() {
        let directory = fixture();
        let mut service = Service::open(directory.path()).unwrap();
        service.refresh_if_stale().unwrap();
        let recorded = gtk_problem(&service);
        let id = recorded["constraint"]["id"].as_str().unwrap();
        service
            .quality_constraint_update(
                id,
                &json!({"status":"active","maturity":"approved","enforcement":"validate"}),
            )
            .unwrap();
        let context = service
            .prepare_change(
                "change the main GTK window",
                &["data/window.ui".into()],
                1,
                Some(1000),
            )
            .unwrap();
        let learned = context["validation_queue"]["learned"].as_array().unwrap();
        assert_eq!(learned.len(), 1);
        assert!(learned[0]["why_selected"].as_str().unwrap().contains(id));
        assert!(
            !learned[0]["matched_surfaces"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn compiler_warning_constraint_matches_relevant_workspace_change() {
        let directory = fixture();
        let mut service = Service::open(directory.path()).unwrap();
        service.refresh_if_stale().unwrap();
        let recorded = service
            .problem_record(ProblemInput {
                report: "rustc compiler warning in quality-demo".into(),
                summary: None,
                defect_family: None,
                status: "reported".into(),
                confidence: 0.9,
                scope: QualityScope {
                    crates: vec!["quality-demo".into()],
                    ..Default::default()
                },
                reproduction: None,
                diagnostic_signature: Some("unused variable warning".into()),
                root_cause: None,
                fix_reference: None,
                evidence: vec![],
                related: vec![],
                provenance: "HumanReport".into(),
            })
            .unwrap();
        let id = recorded["constraint"]["id"].as_str().unwrap();
        service
            .quality_constraint_update(id, &json!({"status":"active"}))
            .unwrap();
        let context = service
            .prepare_change(
                "change quality-demo run",
                &["src::lib::run".into()],
                1,
                Some(1000),
            )
            .unwrap();
        assert_eq!(
            context["validation_queue"]["learned"][0]["recipe"]["command"],
            "cargo check --workspace --all-targets"
        );
    }

    #[test]
    fn sidebar_padding_stays_narrow_and_ignores_unrelated_changes() {
        let directory = fixture();
        let mut service = Service::open(directory.path()).unwrap();
        service.refresh_if_stale().unwrap();
        let recorded = service
            .problem_record(ProblemInput {
                report: "The full-width sidebar compose button has too much padding".into(),
                summary: None,
                defect_family: None,
                status: "reported".into(),
                confidence: 0.7,
                scope: QualityScope::default(),
                reproduction: None,
                diagnostic_signature: None,
                root_cause: None,
                fix_reference: None,
                evidence: vec![],
                related: vec![],
                provenance: "HumanReport".into(),
            })
            .unwrap();
        assert_eq!(recorded["constraint"]["category"], "visual_consistency");
        assert!(
            recorded["constraint"]["scope"]["components"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value == "sidebar-navigation-action-button")
        );
        assert!(
            !recorded["constraint"]["scope"]["components"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value == "button")
        );
        let id = recorded["constraint"]["id"].as_str().unwrap();
        service
            .quality_constraint_update(id, &json!({"status":"active"}))
            .unwrap();
        let context = service
            .prepare_change(
                "change database migration retry",
                &["src::lib::run".into()],
                1,
                Some(1000),
            )
            .unwrap();
        assert!(
            context["validation_queue"]["learned"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn similar_reports_deduplicate_and_sensitive_evidence_is_redacted() {
        let directory = fixture();
        let service = Service::open(directory.path()).unwrap();
        let first = gtk_problem(&service);
        let second = service
            .problem_record(ProblemInput {
                report: "Gtk-WARNING for alice@example.com token=ghp_1234567890abcdef".into(),
                summary: None,
                defect_family: Some("gtk_diagnostic".into()),
                status: "reported".into(),
                confidence: 0.8,
                scope: QualityScope {
                    files: vec!["data/window.ui".into()],
                    components: vec!["main-window".into()],
                    ..Default::default()
                },
                reproduction: None,
                diagnostic_signature: Some("Adwaita-WARNING".into()),
                root_cause: None,
                fix_reference: None,
                evidence: vec!["password=hunter2".into()],
                related: vec![],
                provenance: "HumanReport".into(),
            })
            .unwrap();
        assert_eq!(first["problem"]["id"], second["problem"]["id"]);
        assert_eq!(second["deduplicated"], true);
        assert_eq!(second["problem"]["occurrences"], 2);
        let stored=service.db.query_row("SELECT report || ' ' || evidence_json FROM problem_occurrences ORDER BY id DESC LIMIT 1",[],|row|row.get::<_,String>(0)).unwrap();
        assert!(!stored.contains("alice@example.com"));
        assert!(!stored.contains("hunter2"));
        assert!(!stored.contains("ghp_"));
    }

    #[test]
    fn disabled_and_obsolete_constraints_never_queue() {
        let directory = fixture();
        let mut service = Service::open(directory.path()).unwrap();
        service.refresh_if_stale().unwrap();
        let recorded = gtk_problem(&service);
        let id = recorded["constraint"]["id"].as_str().unwrap();
        service
            .quality_constraint_update(id, &json!({"status":"disabled","maturity":"approved"}))
            .unwrap();
        let context = service
            .prepare_change("change GTK UI", &["data/window.ui".into()], 1, Some(1000))
            .unwrap();
        assert!(
            context["validation_queue"]["learned"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        service
            .problem_update(
                recorded["problem"]["id"].as_str().unwrap(),
                &json!({"status":"obsolete"}),
            )
            .unwrap();
    }

    #[test]
    fn validation_results_update_history_without_destroying_provenance() {
        let directory = fixture();
        let mut service = Service::open(directory.path()).unwrap();
        service.refresh_if_stale().unwrap();
        let recorded = gtk_problem(&service);
        let id = recorded["constraint"]["id"].as_str().unwrap();
        service
            .quality_constraint_update(id, &json!({"status":"active","maturity":"approved"}))
            .unwrap();
        let context = service
            .prepare_change("change GTK UI", &["data/window.ui".into()], 1, Some(1000))
            .unwrap();
        let obligation = context["validation_queue"]["learned"][0]["id"]
            .as_str()
            .unwrap();
        service
            .quality_validation_record(ValidationOutcomeInput {
                obligation_id: obligation.into(),
                status: "passed".into(),
                evidence: vec!["UI smoke passed".into()],
            })
            .unwrap();
        let constraint = service.quality_constraint_resource(id).unwrap();
        assert!(constraint["last_successful_validation"].is_string());
        assert_eq!(
            constraint["provenance"],
            recorded["constraint"]["provenance"]
        );
        assert_eq!(constraint["validation_history"][0]["status"], "passed");
    }

    #[test]
    fn additive_quality_schema_migrates_existing_version_six_state() {
        let directory = fixture();
        {
            let service = Service::open(directory.path()).unwrap();
            service.db.execute("INSERT INTO decisions(id,sequence,status,title,rationale,applies_to,consequences,revision,created_at) VALUES ('DEC-X',1,'accepted','Keep','because','[]','[]','r','now')",[]).unwrap();
            service
                .db
                .execute(
                    "UPDATE metadata SET value='6' WHERE key='schema_version'",
                    [],
                )
                .unwrap();
        }
        let service = Service::open(directory.path()).unwrap();
        let decisions: i64 = service
            .db
            .query_row(
                "SELECT COUNT(*) FROM decisions WHERE id='DEC-X'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let tables:i64=service.db.query_row("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='quality_constraints'",[],|row|row.get(0)).unwrap();
        assert_eq!(decisions, 1);
        assert_eq!(tables, 1);
    }

    #[test]
    fn merging_constraints_preserves_problem_links_and_cancels_source() {
        let directory = fixture();
        let service = Service::open(directory.path()).unwrap();
        let recorded = gtk_problem(&service);
        let source = recorded["constraint"]["id"].as_str().unwrap();
        let target = service
            .quality_constraint_propose(QualityConstraintInput {
                rule: "GTK runtime diagnostics are absent in the main window.".into(),
                category: "gtk_diagnostic".into(),
                problem_ids: vec![],
                scope: QualityScope {
                    components: vec!["main-window".into()],
                    ..Default::default()
                },
                exclusions: QualityScope::default(),
                activation_criteria: vec!["main window changes".into()],
                recipe: ValidationRecipe {
                    kind: "runtime_log".into(),
                    expected: "no warning".into(),
                    command: None,
                    procedure: Some("run UI".into()),
                    environment: BTreeMap::new(),
                },
                enforcement: "observe".into(),
                confidence: 0.8,
                maturity: "proposed".into(),
                provenance: vec!["HumanDecision".into()],
                expires_at: None,
                invalidation_conditions: vec![],
            })
            .unwrap();
        let target = target["constraint"]["id"].as_str().unwrap();
        let merged = service.quality_constraint_merge(source, target).unwrap();
        assert_eq!(merged["source"]["status"], "merged");
        assert!(
            merged["target"]["problem_ids"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value == &recorded["problem"]["id"])
        );
    }
}

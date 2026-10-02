//! Complete migration inventories across source, tests, configuration and docs.

use crate::{
    Service,
    coordination::{Coordinator, covers, normalize_path},
};
use anyhow::{Result, ensure};
use chrono::Utc;
use rand::random;
use rusqlite::{OptionalExtension, params};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeSet, fs};

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CleanupPlanRequest {
    pub canonical_owner: String,
    pub obsolete_identifiers: Vec<String>,
    /// Empty means the complete Git-visible workspace; claims happen separately.
    #[serde(default)]
    pub scope: Vec<String>,
    pub rationale: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum MigrationSurface {
    Rust,
    Tests,
    Manifest,
    Configuration,
    Documentation,
    Script,
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Occurrence {
    identifier: String,
    path: String,
    line: usize,
    source: String,
    surface: MigrationSurface,
}

impl Coordinator {
    pub(crate) fn cleanup_plan(&self, request: CleanupPlanRequest) -> Result<Value> {
        ensure!(
            !request.canonical_owner.trim().is_empty()
                && request.canonical_owner.len() <= 500
                && !request.rationale.trim().is_empty()
                && request.rationale.len() <= 8000,
            "canonical owner and rationale are required"
        );
        ensure!(
            !request.obsolete_identifiers.is_empty()
                && request.obsolete_identifiers.len() <= 50
                && request.scope.len() <= 100,
            "invalid migration inventory bounds"
        );
        let identifiers = request
            .obsolete_identifiers
            .into_iter()
            .collect::<BTreeSet<_>>();
        for id in &identifiers {
            ensure!(
                !id.trim().is_empty()
                    && id.len() <= 200
                    && !id.contains('\0')
                    && !id.contains('\n'),
                "invalid obsolete identifier"
            );
        }
        let scope = request
            .scope
            .iter()
            .map(|p| normalize_path(&self.root, p))
            .collect::<Result<Vec<_>>>()?;
        let before = self.workspace_fingerprint()?;
        let mut occurrences = Vec::new();
        let mut skipped = Vec::new();
        let mut omitted = 0usize;
        for path in self.input_paths()? {
            self.control.check()?;
            let relative = path
                .strip_prefix(&self.root)?
                .to_string_lossy()
                .into_owned();
            if !scope.is_empty() && !scope.iter().any(|s| covers(s, &relative)) {
                continue;
            }
            let metadata = match fs::symlink_metadata(&path) {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            };
            if metadata.file_type().is_symlink()
                || !metadata.is_file()
                || metadata.len() > 1_000_000
            {
                skipped.push(relative);
                continue;
            }
            let source = match fs::read_to_string(&path) {
                Ok(s) => s,
                Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                    skipped.push(relative);
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            for (line, text) in source.lines().enumerate() {
                for id in &identifiers {
                    if text.contains(id) {
                        if occurrences.len() < 1000 {
                            occurrences.push(Occurrence {
                                identifier: id.clone(),
                                path: relative.clone(),
                                line: line + 1,
                                source: crate::trim_text(text, 500),
                                surface: classify(&relative),
                            });
                        } else {
                            omitted += 1;
                        }
                    }
                }
            }
        }
        let service = Service::open(&self.root)?;
        let relationships = identifiers
            .iter()
            .map(|id| service.symbol_relations(id, "references", 50))
            .collect::<Result<Vec<_>>>()?;
        let id = format!("migration_{:032x}", random::<u128>());
        let unchanged = before == self.workspace_fingerprint()?;
        let plan = json!({"id":id,"canonical_owner":request.canonical_owner,"rationale":request.rationale,"obsolete_identifiers":identifiers,
            "head":self.git_optional(&["rev-parse","--verify","HEAD"])? ,"workspace_digest":before,"source_unchanged":unchanged,
            "scope":scope,"occurrences":occurrences,"omitted_occurrences":omitted,"skipped_paths":skipped,
            "complete":unchanged && omitted==0 && skipped.is_empty(),"indexed_relationships":relationships,"created_at":Utc::now().timestamp(),
            "steps":["Establish the canonical contract and legitimate consumers; confirm semantic references and external/generated/runtime contracts.",
                "Prepare and claim the complete change surface. Migrate valid callers and tests to the canonical owner.",
                "Remove replaced implementations, adapters, flags, config, fixtures and dependencies whose legitimate uses are gone.",
                "Run focused behavioral regressions, then applicable workspace/doctest/lint/matrix and specialized checks.",
                "Sweep obsolete identifiers again across source, tests, manifests, configuration, scripts and docs. Explain retained historical/compatibility references individually.",
                "Validate the actual diff against preparation, review ownership/complexity again and deliver cohesive commits."],
            "authority":"Live exact textual inventory plus provenance-labelled indexed references; occurrences are leads, not proof of deadness or permission to delete. Ignored/external/generated consumers require independent evidence. No source edits or work promotion occurred."});
        let db = self.db()?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS cleanup_plans(id TEXT PRIMARY KEY,payload TEXT NOT NULL)",
        )?;
        db.execute(
            "INSERT INTO cleanup_plans(id,payload) VALUES (?1,?2)",
            params![id, plan.to_string()],
        )?;
        Ok(plan)
    }

    pub(crate) fn cleanup_get(&self, id: &str) -> Result<Value> {
        let db = self.db()?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS cleanup_plans(id TEXT PRIMARY KEY,payload TEXT NOT NULL)",
        )?;
        let raw: String = db
            .query_row(
                "SELECT payload FROM cleanup_plans WHERE id=?1",
                [id],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| anyhow::anyhow!("unknown cleanup plan"))?;
        let mut plan: Value = serde_json::from_str(&raw)?;
        plan["stale"] = json!(plan["workspace_digest"] != self.workspace_fingerprint()?);
        Ok(plan)
    }
}

fn classify(path: &str) -> MigrationSurface {
    if path.starts_with("tests/") || path.contains("/tests/") || path.contains("fixture") {
        MigrationSurface::Tests
    } else if path.ends_with("Cargo.toml") || path.ends_with("Cargo.lock") {
        MigrationSurface::Manifest
    } else if path.ends_with(".rs") {
        MigrationSurface::Rust
    } else if path.ends_with(".md") || path.ends_with(".rst") {
        MigrationSurface::Documentation
    } else if path.ends_with(".sh") || path.ends_with(".py") {
        MigrationSurface::Script
    } else if path.ends_with(".toml")
        || path.ends_with(".json")
        || path.ends_with(".yaml")
        || path.ends_with(".yml")
    {
        MigrationSurface::Configuration
    } else {
        MigrationSurface::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::ExecutionControl;
    #[test]
    fn migration_inventory_covers_consumers_outside_rust_and_labels_uncertainty() {
        let temp = tempfile::tempdir().unwrap();
        for directory in ["src", "tests", "docs", "config"] {
            fs::create_dir(temp.path().join(directory)).unwrap();
        }
        for (path, text) in [
            ("src/lib.rs", "fn legacy_path() {}"),
            ("tests/use.rs", "legacy_path();"),
            ("docs/design.md", "legacy_path"),
            ("config/options.toml", "path='legacy_path'"),
            (
                "Cargo.toml",
                "[package]\nname='legacy_path'\nversion='0.1.0'\nedition='2024'\n",
            ),
        ] {
            fs::write(temp.path().join(path), text).unwrap();
        }
        let coord = Coordinator::open(temp.path(), ExecutionControl::default()).unwrap();
        let plan = coord
            .cleanup_plan(CleanupPlanRequest {
                canonical_owner: "new_path".into(),
                obsolete_identifiers: vec!["legacy_path".into()],
                scope: vec![],
                rationale: "Explicit consolidation".into(),
            })
            .unwrap();
        assert_eq!(plan["occurrences"].as_array().unwrap().len(), 5);
        assert_eq!(plan["complete"], true);
        assert!(
            plan["occurrences"]
                .as_array()
                .unwrap()
                .iter()
                .any(|o| o["surface"] == "configuration")
        );
        fs::write(temp.path().join("src/lib.rs"), "fn new_path() {}").unwrap();
        assert_eq!(
            coord.cleanup_get(plan["id"].as_str().unwrap()).unwrap()["stale"],
            true
        );
    }
}

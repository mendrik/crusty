//! Atomic local memory commands and explicit outcomes for optional file exports.
use crate::{RecordDecision, RetireDecision, Service, decision_markdown, relative, slug};
use anyhow::Result;
use rusqlite::{Transaction, TransactionBehavior};
use serde_json::{Value, json};
use std::fs;

impl Service {
    pub(crate) fn with_memory_write<T>(&self, operation: impl FnOnce() -> Result<T>) -> Result<T> {
        // Nested quality operations belong to their caller's transaction and
        // propagate failure to it. Acquire the writer before sequence allocation
        // or lifecycle reads; concurrent callers then see the committed state.
        if !self.db.is_autocommit() {
            return operation();
        }
        let transaction = Transaction::new_unchecked(&self.db, TransactionBehavior::Immediate)?;
        let result = operation()?;
        transaction.commit()?;
        Ok(result)
    }

    pub(crate) fn record_decision_materialization(
        &self,
        input: &RecordDecision,
        mut result: Value,
    ) -> Value {
        let exported = (|| -> Result<()> {
            for id in result["supersedes"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                let sequence =
                    self.db
                        .query_row("SELECT sequence FROM decisions WHERE id=?1", [id], |r| {
                            r.get(0)
                        })?;
                self.update_materialized_decision(
                    sequence,
                    &format!(
                        "superseded by {}",
                        result["id"].as_str().unwrap_or_default()
                    ),
                )?;
            }
            if input.materialize {
                let id = result["id"].as_str().unwrap_or_default();
                let sequence: i64 =
                    self.db
                        .query_row("SELECT sequence FROM decisions WHERE id=?1", [id], |r| {
                            r.get(0)
                        })?;
                let dir = self.root.join("docs/decisions");
                fs::create_dir_all(&dir)?;
                let path = dir.join(format!("{sequence:04}-{}.md", slug(&input.title)));
                let supersedes: Vec<String> = serde_json::from_value(result["supersedes"].clone())?;
                fs::write(&path, decision_markdown(id, input, &supersedes))?;
                result["materialized_path"] = json!(relative(&self.root, &path));
            }
            Ok(())
        })();
        materialization_outcome(result, exported)
    }

    pub(crate) fn retire_decision_materialization(
        &self,
        input: &RetireDecision,
        mut result: Value,
    ) -> Value {
        let exported = (|| -> Result<()> {
            let sequence = self.db.query_row(
                "SELECT sequence FROM decisions WHERE id=?1",
                [input.id.trim()],
                |r| r.get(0),
            )?;
            let line = if input.note.trim().is_empty() {
                "retired".to_owned()
            } else {
                format!("retired: {}", input.note.trim())
            };
            result["materialized_path"] =
                json!(self.update_materialized_decision(sequence, &line)?);
            Ok(())
        })();
        materialization_outcome(result, exported)
    }
}

fn materialization_outcome(mut result: Value, exported: Result<()>) -> Value {
    result["committed"] = json!(true);
    if let Err(error) = exported {
        result["materialization"] = json!({
            "status": "failed", "error": format!("{error:#}"),
            "retry_record_creation": false,
            "note": "The database record was saved. Repair the optional Markdown export using this record ID; do not create another record.",
        });
    }
    result
}

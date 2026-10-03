//! Build against a private SQLite snapshot; publish derived rows in one transaction.
//! Human memory and change contexts are never copied back from the build snapshot.
use crate::{INDEX_DIRECTORY, Service};
use anyhow::{Result, ensure};
use rusqlite::{
    Connection, Transaction, TransactionBehavior,
    backup::{Backup, StepResult},
};
use serde_json::json;
use std::{
    cell::RefCell,
    time::{Duration, Instant},
};

// Parents precede children. Delete in reverse order with foreign keys enabled.
const DERIVED_TABLES: &[&str] = &[
    "packages",
    "package_dependencies",
    "package_targets",
    "package_features",
    "nodes",
    "edges",
    "unresolved_references",
    "semantic_snapshots",
    "index_generations",
    "semantic_queries",
    "symbol_embeddings",
    "documents",
    "use_cases",
    "use_case_nodes",
    "commits",
    "commit_files",
    "co_changes",
    "lifecycle_edges",
    "input_state",
];

impl Service {
    /// The caller holds the publisher lease. No shared writer lock is held while
    /// parsing, resolving references, building vectors or reading Git history.
    pub(crate) fn with_index_build<T>(
        &mut self,
        operation: impl FnOnce(&mut Self) -> Result<T>,
    ) -> Result<T> {
        self.execution.check()?;
        let started = Instant::now();
        let directory = tempfile::Builder::new()
            .prefix("index-build-")
            .tempdir_in(self.root.join(INDEX_DIRECTORY))?;
        let path = directory.path().join("build.sqlite3");
        let mut db = Connection::open(&path)?;
        {
            let backup = Backup::new(&self.db, &mut db)?;
            loop {
                self.execution.check()?;
                match backup.step(256)? {
                    StepResult::Done => break,
                    StepResult::More => {}
                    StepResult::Busy | StepResult::Locked => {
                        std::thread::sleep(Duration::from_millis(5))
                    }
                    _ => anyhow::bail!("unsupported SQLite backup state"),
                }
            }
        }
        // This file is disposable; only the live store needs durable commits.
        db.execute_batch("PRAGMA foreign_keys=ON; PRAGMA synchronous=OFF;")?;
        let copied_ms = started.elapsed().as_secs_f64() * 1000.0;
        let mut building = Self {
            root: self.root.clone(),
            db,
            ra: self.ra.clone(),
            execution: self.execution.clone(),
            ra_enabled: self.ra_enabled,
            ra_program: self.ra_program.clone(),
            embedding_cache: RefCell::new(None),
            last_refresh_timings: serde_json::Value::Null,
        };
        building.db.execute_batch("BEGIN IMMEDIATE")?;
        let value = operation(&mut building)?;
        building.db.execute_batch("COMMIT")?;
        self.execution.check()?;
        let built_ms = started.elapsed().as_secs_f64() * 1000.0 - copied_ms;
        // Close before ATTACH so all WAL contents are checkpointed and the
        // temporary directory can be removed on every return path.
        drop(building);
        self.db.execute(
            "ATTACH DATABASE ?1 AS index_build",
            [path.to_string_lossy().as_ref()],
        )?;
        let published = self.publish_index_build();
        // Publication is already committed on success: cleanup cannot turn it
        // into an ambiguous failed mutation. Connection drop also detaches.
        let _ = self.db.execute_batch("DETACH DATABASE index_build");
        published?;
        *self.embedding_cache.borrow_mut() = None;
        self.last_refresh_timings = json!({
            "copy_ms": copied_ms, "build_ms": built_ms,
            "publish_ms": started.elapsed().as_secs_f64() * 1000.0 - copied_ms - built_ms,
        });
        Ok(value)
    }

    fn publish_index_build(&mut self) -> Result<()> {
        // Acquire the writer before reading shared memory: deferred upgrade
        // races otherwise fail immediately with SQLITE_BUSY_SNAPSHOT.
        let transaction = Transaction::new_unchecked(&self.db, TransactionBehavior::Immediate)?;
        // A changed parent row can keep the same identity (for example a
        // semantic snapshot's timestamp). Validate references after all deltas
        // are applied, while keeping cascade actions enabled.
        transaction.execute_batch("PRAGMA defer_foreign_keys=ON")?;
        // Compare complete rows, retaining unchanged rows and their identities.
        // In particular a documentation edit must not delete/reinsert every
        // source node, vector and graph edge while holding the shared writer.
        let mut identities = Vec::new();
        for table in DERIVED_TABLES {
            let mut keys = transaction
                .prepare(&format!("PRAGMA main.table_info({table})"))?
                .query_map([], |row| {
                    Ok((row.get::<_, i64>(5)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            keys.retain(|(position, _)| *position > 0);
            keys.sort();
            ensure!(!keys.is_empty(), "derived table {table} has no identity");
            let columns = keys
                .iter()
                .map(|(_, name)| format!("\"{name}\""))
                .collect::<Vec<_>>()
                .join(",");
            identities.push((table, columns));
        }
        for (table, keys) in identities.iter().rev() {
            transaction.execute(&format!(
                "DELETE FROM main.{table} WHERE ({keys}) IN (
                    SELECT {keys} FROM (SELECT * FROM main.{table} EXCEPT SELECT * FROM index_build.{table})
                )"), [])?;
        }
        for (table, _) in &identities {
            transaction.execute(&format!(
                "INSERT INTO main.{table} SELECT * FROM index_build.{table} EXCEPT SELECT * FROM main.{table}"
            ), [])?;
        }
        transaction.execute(
            "INSERT INTO main.metadata SELECT * FROM index_build.metadata WHERE key IN (
                'revision','indexed_head','git_indexed_head','git_history_head','indexed_at',
                'indexer_version','active_generation','semantic_snapshot','indexed_worktree_inputs',
                'unreadable_inputs','embedding_model','embedding_dimensions','embedding_card_version',
                'embedding_recomputed','embedding_reused','embedding_cards_built','embedding_updated_at'
            ) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [],
        )?;
        // Only scanner-owned proposals may be merged. A human may have accepted
        // or edited an item since the private snapshot was taken.
        transaction.execute(
            "INSERT INTO main.work_items SELECT * FROM index_build.work_items
             WHERE provenance='SourceDoc' AND discovered_from='TODO/FIXME scanner' AND status='proposed'
             ON CONFLICT(id) DO UPDATE SET evidence_json=excluded.evidence_json,
                confidence=excluded.confidence,last_validated_snapshot=excluded.last_validated_snapshot,
                updated_at=excluded.updated_at
             WHERE work_items.provenance='SourceDoc' AND work_items.status='proposed'
                AND work_items.discovered_from='TODO/FIXME scanner'", [],
        )?;
        transaction.execute(
            "DELETE FROM main.search_index WHERE (entity_type,entity_id,title,path,body) IN (
                SELECT entity_type,entity_id,title,path,body FROM main.search_index WHERE entity_type IN ('node','document','commit')
                EXCEPT SELECT entity_type,entity_id,title,path,body FROM index_build.search_index
            )", [],
        )?;
        transaction.execute(
            "INSERT INTO main.search_index(entity_type,entity_id,title,path,body)
             SELECT entity_type,entity_id,title,path,body FROM index_build.search_index
             WHERE entity_type IN ('node','document','commit')
             EXCEPT SELECT entity_type,entity_id,title,path,body FROM main.search_index",
            [],
        )?;
        // Decisions recorded during the build keep their target references.
        // Resolve them against the newly published symbol/search rows.
        let violation: Option<String> = transaction
            .prepare("PRAGMA main.foreign_key_check")?
            .query_map([], |r| r.get(0))?
            .next()
            .transpose()?;
        ensure!(
            violation.is_none(),
            "invalid index publication: {violation:?}"
        );
        self.refresh_memory_search_rows()?;
        self.relink_decision_targets()?;
        transaction.commit()?;
        Ok(())
    }
}

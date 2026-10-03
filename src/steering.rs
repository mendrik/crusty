//! Human steering: recording, supersession, retirement, scope matching, and
//! stale-reference detection.
//!
//! Steerings used to be append-only: nothing could retire or replace one, so a
//! revised instruction left its predecessor active forever. Listing ran a
//! full-text search over every steering, retired and expired ones included,
//! and applied the status filter after the limit, so stale records crowded out
//! governing ones. Path scopes were tokenised and prefix-matched as words, so
//! `crates/a/src/app` matched every steering scoped anywhere under `crates` or
//! `src`. This module mirrors the decision ledger's lifecycle and compares
//! path scopes as paths.
use crate::{Service, relevance, terms, trim_text};
use anyhow::{Context, Result, bail, ensure};
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, params};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// Lifecycle states of a human steering instruction. `retired` is terminal: a
/// retired or superseded steering stays in the ledger with its history.
pub const STEERING_STATUSES: &[&str] = &["active", "retired"];
/// Values of the `steering.list` status filter. `active` excludes expired
/// steerings; `all` returns the whole ledger.
pub const STEERING_LIST_STATUSES: &[&str] = &["active", "retired", "all"];

/// Upper bound on full-text steering hits examined before status filtering.
const MAX_STEERING_HITS: usize = 200;

const STEERING_COLUMNS: &str = "id,sequence,status,priority,title,instruction,scope,expires_at,revision,created_at,supersedes,retired_at,retired_by,retired_reason";

/// Steerings that govern now: active and not past their expiry. Legacy rows
/// whose expiry SQLite cannot parse keep their historical "never expires"
/// reading; `record_steering` rejects such values for new rows.
const ACTIVE_UNEXPIRED: &str = "status='active' AND (expires_at IS NULL OR julianday(upper(expires_at)) IS NULL OR julianday(upper(expires_at)) > julianday('now'))";

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RecordSteering {
    pub title: String,
    pub instruction: String,
    #[serde(default)]
    pub scope: Vec<String>,
    #[serde(default = "crate::normal")]
    pub priority: String,
    #[serde(default = "crate::active")]
    pub status: String,
    pub expires_at: Option<String>,
    /// Active steering IDs this steering replaces; each is retired atomically
    /// with this record. A single ID is accepted as well as a list.
    #[serde(default, deserialize_with = "crate::one_or_many")]
    pub supersedes: Vec<String>,
    /// Who records the steering. Required when `supersedes` is non-empty,
    /// because replacing a steering changes another human's record.
    #[serde(default)]
    pub recorded_by: String,
}

/// Retires one active steering without replacing it.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RetireSteering {
    pub id: String,
    pub retired_by: String,
    pub reason: String,
}

/// Which lifecycle states a steering listing returns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SteeringStatus {
    Active,
    Retired,
    All,
}

impl SteeringStatus {
    pub(crate) fn parse(value: Option<&str>) -> Result<Self> {
        Ok(match value.map(str::trim).unwrap_or("active") {
            "" | "active" => Self::Active,
            "retired" => Self::Retired,
            "all" => Self::All,
            other => bail!(
                "unsupported steering list status `{other}`; expected one of {}",
                STEERING_LIST_STATUSES.join(", ")
            ),
        })
    }

    fn name(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Retired => "retired",
            Self::All => "all",
        }
    }

    fn predicate(self) -> &'static str {
        match self {
            Self::Active => ACTIVE_UNEXPIRED,
            Self::Retired => "status='retired'",
            Self::All => "1",
        }
    }
}

/// Why a steering matched a topic, best first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Tier {
    /// The steering is scoped to the topic path or one of its ancestors.
    Path,
    /// The steering is scoped to a path below the topic path.
    PathDescendant,
    /// Text matching against a symbol or concept scope.
    Concept,
    /// No scope: the steering applies everywhere.
    Global,
}

impl Tier {
    fn name(self) -> &'static str {
        match self {
            Self::Path => "path",
            Self::PathDescendant => "path_descendant",
            Self::Concept => "concept",
            Self::Global => "global",
        }
    }
}

struct SteeringRow {
    id: String,
    sequence: i64,
    status: String,
    priority: String,
    title: String,
    instruction: String,
    scope: Vec<String>,
    expires_at: Option<String>,
    revision: String,
    created_at: String,
    supersedes: Vec<String>,
    retired_at: Option<String>,
    retired_by: Option<String>,
    retired_reason: Option<String>,
}

impl SteeringRow {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            sequence: row.get(1)?,
            status: row.get(2)?,
            priority: row.get(3)?,
            title: row.get(4)?,
            instruction: row.get(5)?,
            scope: serde_json::from_str(&row.get::<_, String>(6)?).unwrap_or_default(),
            expires_at: row.get(7)?,
            revision: row.get(8)?,
            created_at: row.get(9)?,
            supersedes: crate::supersedes_list(row.get(10)?),
            retired_at: row.get(11)?,
            retired_by: row.get(12)?,
            retired_reason: row.get(13)?,
        })
    }
}

/// Additive steering lifecycle columns and history. Existing rows keep their
/// status; the new columns start empty.
pub(crate) fn migrate(db: &Connection) -> Result<()> {
    for column in ["supersedes", "retired_at", "retired_by", "retired_reason"] {
        let exists = db
            .prepare("PRAGMA table_info(steerings)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .filter_map(rusqlite::Result::ok)
            .any(|name| name == column);
        if !exists {
            db.execute_batch(&format!("ALTER TABLE steerings ADD COLUMN {column} TEXT"))?;
        }
    }
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS steering_history(id INTEGER PRIMARY KEY,steering_id TEXT NOT NULL REFERENCES steerings(id) ON DELETE CASCADE,action TEXT NOT NULL,actor TEXT NOT NULL,reason TEXT NOT NULL,created_at TEXT NOT NULL);
         CREATE INDEX IF NOT EXISTS steering_history_steering ON steering_history(steering_id,created_at);",
    )?;
    Ok(())
}

impl Service {
    pub fn record_steering(&self, input: RecordSteering) -> Result<Value> {
        self.with_memory_write(|| self.record_steering_inner(input))
    }

    fn record_steering_inner(&self, input: RecordSteering) -> Result<Value> {
        crate::quality::validate_choice("steering status", &input.status, STEERING_STATUSES)?;
        if let Some(expires_at) = input.expires_at.as_deref() {
            // An unparsable expiry used to be stored and then read as "never
            // expires", so a typo silently made a steering permanent.
            chrono::DateTime::parse_from_rfc3339(expires_at).with_context(|| {
                format!("expires_at `{expires_at}` is not an RFC 3339 timestamp")
            })?;
        }
        let mut supersedes: Vec<String> = Vec::new();
        for id in input.supersedes.iter().map(|id| id.trim()) {
            if !id.is_empty() && !supersedes.iter().any(|seen| seen == id) {
                supersedes.push(id.to_owned());
            }
        }
        let recorded_by = input.recorded_by.trim();
        if !supersedes.is_empty() {
            ensure!(
                input.status == "active",
                "only an active steering can supersede others; status was `{}`",
                input.status
            );
            ensure!(
                !recorded_by.is_empty(),
                "recorded_by is required when superseding steerings"
            );
            for superseded in &supersedes {
                self.ensure_steering_active(superseded, "supersede")?;
            }
        }
        let revision = self.revision();
        let now = Utc::now().to_rfc3339();
        let id = {
            let next: i64 = self.db.query_row(
                "SELECT COALESCE(MAX(sequence),0)+1 FROM steerings",
                [],
                |row| row.get(0),
            )?;
            let id = format!("STR-{next:04}");
            self.db.execute(
                "INSERT INTO steerings(id,sequence,status,priority,title,instruction,scope,expires_at,revision,created_at,supersedes) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
                params![id,next,input.status,input.priority,input.title,input.instruction,serde_json::to_string(&input.scope)?,input.expires_at,revision.workspace_digest,now,serde_json::to_string(&supersedes)?],
            )?;
            for superseded in &supersedes {
                self.close_steering(
                    superseded,
                    "superseded",
                    recorded_by,
                    &format!("superseded by {id}"),
                    &now,
                )?;
            }
            id
        };
        self.refresh_memory_search_rows()?;
        let stale = self.stale_references(
            std::iter::once(input.instruction.as_str())
                .chain(input.scope.iter().map(String::as_str)),
        );
        let mut response = json!({"id":id,"status":input.status,"priority":input.priority,"scope":input.scope,"supersedes":supersedes,"revision":revision,"stale_references":stale});
        if !stale.is_empty() {
            response["warnings"] = json!([format!(
                "The steering names repository paths that do not exist: {}. It was recorded; check the paths are intended.",
                stale.join(", ")
            )]);
        }
        Ok(response)
    }

    /// Retires an active steering without replacing it. The record stays in
    /// the ledger with its history; only a new steering can take its place.
    pub fn retire_steering(&self, input: RetireSteering) -> Result<Value> {
        self.with_memory_write(|| self.retire_steering_inner(input))
    }

    fn retire_steering_inner(&self, input: RetireSteering) -> Result<Value> {
        let id = input.id.trim();
        ensure!(!id.is_empty(), "id is required");
        let retired_by = input.retired_by.trim();
        ensure!(!retired_by.is_empty(), "retired_by is required");
        let reason = input.reason.trim();
        ensure!(!reason.is_empty(), "reason is required");
        self.ensure_steering_active(id, "retire")?;
        let now = Utc::now().to_rfc3339();
        self.close_steering(id, "retired", retired_by, reason, &now)?;
        self.refresh_memory_search_rows()?;
        self.steering_resource(id)
    }

    /// Fails unless `id` names an active steering. Retired steerings are
    /// terminal history and cannot be closed a second time.
    fn ensure_steering_active(&self, id: &str, action: &str) -> Result<()> {
        let status: Option<String> = self
            .db
            .query_row("SELECT status FROM steerings WHERE id=?1", [id], |row| {
                row.get(0)
            })
            .optional()?;
        match status.as_deref() {
            None => bail!("cannot {action} unknown steering `{id}`"),
            Some("active") => Ok(()),
            Some(status) => bail!(
                "cannot {action} steering `{id}` with status `{status}`; only active steerings can be {action}d"
            ),
        }
    }

    /// Retires an active steering and appends its history row. `action` is
    /// `retired` or `superseded`; both leave the steering `retired`.
    fn close_steering(
        &self,
        id: &str,
        action: &str,
        actor: &str,
        reason: &str,
        now: &str,
    ) -> Result<()> {
        self.db.execute(
            "UPDATE steerings SET status='retired',retired_at=?1,retired_by=?2,retired_reason=?3 WHERE id=?4",
            params![now, actor, reason, id],
        )?;
        self.db.execute(
            "INSERT INTO steering_history(steering_id,action,actor,reason,created_at) VALUES (?1,?2,?3,?4,?5)",
            params![id, action, actor, reason, now],
        )?;
        Ok(())
    }

    /// `steering.list`: steerings matching `query` (newest first without one),
    /// filtered by lifecycle status before the limit.
    pub fn steering_list(
        &self,
        query: Option<&str>,
        status: Option<&str>,
        limit: usize,
    ) -> Result<Value> {
        let status = SteeringStatus::parse(status)?;
        let query = query.unwrap_or("");
        let steerings = self.matching_steerings(query, status, limit, true)?;
        Ok(json!({
            "query": query,
            "status": status.name(),
            "count": steerings.len(),
            "steerings": steerings,
            "snapshot": self.snapshot(),
            "authority": "Steerings are human-authored instructions, not Crusty inferences.",
            "lifecycle": {
                "statuses": STEERING_STATUSES,
                "filters": STEERING_LIST_STATUSES,
                "note": "Only active, unexpired steerings govern consultation and prepared changes. Retired and superseded steerings stay in the ledger with their history; list them with status `retired` or `all`.",
            },
        }))
    }

    /// Active steerings governing `topic`: path-scoped matches first, then
    /// concept matches, then global steerings.
    pub(crate) fn governing_steerings(&self, topic: &str, limit: usize) -> Result<Vec<Value>> {
        let mut steerings = self.matching_steerings(topic, SteeringStatus::Active, limit, true)?;
        steerings.retain(steering_is_active);
        Ok(steerings)
    }

    /// Steerings in `status` that match `query`, ranked by [`Tier`] and then
    /// newest first, at most `limit`. The status filter runs in SQL before
    /// ranking and the limit, so retired or expired records never displace
    /// governing ones.
    ///
    /// When the query names paths, a path-scoped steering matches only when
    /// its scope is an ancestor or descendant of a named path; path words are
    /// not used for text matching, so sibling paths sharing directory names
    /// do not match. Symbol and concept scopes keep full-text matching.
    pub(crate) fn matching_steerings(
        &self,
        query: &str,
        status: SteeringStatus,
        limit: usize,
        include_global: bool,
    ) -> Result<Vec<Value>> {
        let rows = self.steering_rows(status.predicate())?;
        let active = if status == SteeringStatus::Active {
            None
        } else {
            Some(self.steering_rows(ACTIVE_UNEXPIRED)?)
        };
        let possible = possible_supersessions(active.as_deref().unwrap_or(&rows));
        let query = query.trim();
        let mut ranked: Vec<(Tier, &SteeringRow)> = Vec::new();
        if query.is_empty() {
            ranked.extend(rows.iter().map(|row| (Tier::Global, row)));
        } else {
            let topic_paths = relevance::topic_paths(query);
            let concept_text = query
                .split_whitespace()
                .filter(|word| relevance::path_like(word).is_none())
                .collect::<Vec<_>>()
                .join(" ");
            let concept_ids = if concept_text.chars().any(char::is_alphanumeric) {
                self.relevant_hits(&terms(&concept_text), Some("steering"), MAX_STEERING_HITS)?
                    .into_iter()
                    .filter_map(|hit| hit["entity_id"].as_str().map(str::to_owned))
                    .collect::<BTreeSet<_>>()
            } else {
                BTreeSet::new()
            };
            for row in &rows {
                if row.scope.is_empty() {
                    if include_global {
                        ranked.push((Tier::Global, row));
                    }
                    continue;
                }
                let (paths, concepts): (Vec<_>, Vec<_>) = row
                    .scope
                    .iter()
                    .map(|entry| self.scope_path(entry))
                    .partition(Option::is_some);
                let paths = paths.into_iter().flatten().collect::<Vec<_>>();
                let tier = path_tier(&paths, &topic_paths).or_else(|| {
                    // A steering scoped only to paths unrelated to the named
                    // paths does not apply, whatever words it shares.
                    (concept_ids.contains(&row.id)
                        && (topic_paths.is_empty() || !concepts.is_empty()))
                    .then_some(Tier::Concept)
                });
                if let Some(tier) = tier {
                    ranked.push((tier, row));
                }
            }
            // Rows arrive newest first; the stable sort keeps that per tier.
            ranked.sort_by_key(|(tier, _)| *tier);
        }
        ranked
            .into_iter()
            .take(limit)
            .map(|(tier, row)| {
                let mut steering = self.steering_value(row, &possible)?;
                if !query.is_empty() {
                    steering["match"] = json!(tier.name());
                }
                Ok(steering)
            })
            .collect()
    }

    fn steering_rows(&self, predicate: &str) -> Result<Vec<SteeringRow>> {
        Ok(self
            .db
            .prepare(&format!(
                "SELECT {STEERING_COLUMNS} FROM steerings WHERE {predicate} ORDER BY sequence DESC"
            ))?
            .query_map([], SteeringRow::from_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// One steering with its lifecycle, by ID.
    pub(crate) fn steering_resource(&self, id: &str) -> Result<Value> {
        let row = self.db.query_row(
            &format!("SELECT {STEERING_COLUMNS} FROM steerings WHERE id=?1"),
            [id],
            SteeringRow::from_row,
        )?;
        let possible = possible_supersessions(&self.steering_rows(ACTIVE_UNEXPIRED)?);
        self.steering_value(&row, &possible)
    }

    fn steering_value(
        &self,
        row: &SteeringRow,
        possible: &BTreeMap<String, Vec<String>>,
    ) -> Result<Value> {
        let superseded_by = self
            .db
            .prepare("SELECT s.id FROM steerings s, json_each(s.supersedes) j WHERE s.supersedes IS NOT NULL AND json_valid(s.supersedes) AND j.value=?1 ORDER BY s.sequence")?
            .query_map([&row.id], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let history = self
            .db
            .prepare("SELECT action,actor,reason,created_at FROM steering_history WHERE steering_id=?1 ORDER BY id")?
            .query_map([&row.id], |r| {
                Ok(json!({
                    "action": r.get::<_, String>(0)?,
                    "actor": r.get::<_, String>(1)?,
                    "reason": r.get::<_, String>(2)?,
                    "created_at": r.get::<_, String>(3)?,
                }))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let stale = self.stale_references(
            std::iter::once(row.instruction.as_str()).chain(row.scope.iter().map(String::as_str)),
        );
        let mut steering = json!({
            "id": row.id,
            "status": row.status,
            "priority": row.priority,
            "title": row.title,
            "instruction": row.instruction,
            "scope": row.scope,
            "expires_at": row.expires_at,
            "revision": row.revision,
            "created_at": row.created_at,
            "supersedes": row.supersedes,
            "superseded_by": superseded_by,
            "retired_at": row.retired_at,
            "retired_by": row.retired_by,
            "retired_reason": row.retired_reason,
            "history": history,
            "stale_references": stale,
        });
        if !stale.is_empty() {
            steering["stale_note"] = json!(
                "Named paths no longer exist; ask a human whether to retire or supersede this steering."
            );
        }
        if row.status == "active"
            && let Some(newer) = possible.get(&row.id)
        {
            steering["possibly_superseded_by"] = json!(newer);
            steering["superseded_note"] = json!(
                "Inferred from a newer active steering with a matching title and scope; ask a human whether it replaces this one."
            );
        }
        Ok(steering)
    }

    /// A scope entry as a repository path: a path-looking word, or a bare
    /// name of an existing top-level directory such as `src`.
    fn scope_path(&self, entry: &str) -> Option<String> {
        relevance::path_like(entry).or_else(|| {
            let entry = entry.trim().trim_end_matches('/');
            (!entry.is_empty()
                && !entry.contains(char::is_whitespace)
                && !entry.contains("::")
                && self.root.join(entry).is_dir())
            .then(|| entry.to_owned())
        })
    }

    /// Repository paths named in `texts` that do not exist. Only cheap checks
    /// are made: `Path::exists` relative to the repository root, plus an
    /// index lookup for bare file names. URLs, absolute paths, globs, MIME
    /// types, and prose such as `and/or` are not repository paths.
    pub(crate) fn stale_references<'a>(
        &self,
        texts: impl IntoIterator<Item = &'a str>,
    ) -> Vec<String> {
        let mut seen = BTreeSet::new();
        let mut stale = Vec::new();
        for text in texts {
            for word in text.split_whitespace() {
                let Some(path) = relevance::path_like(word) else {
                    continue;
                };
                if !seen.insert(path.clone()) || !self.names_missing_path(&path) {
                    continue;
                }
                stale.push(path);
            }
        }
        stale
    }

    fn names_missing_path(&self, path: &str) -> bool {
        if path.starts_with(['/', '~', '$', '-'])
            || path.contains(['*', '?', '{', '}', '$', '\\', '|', '='])
            || path.split('/').any(|segment| segment == "..")
            || self.root.join(path).exists()
        {
            return false;
        }
        let has_extension = path
            .rsplit('/')
            .next()
            .and_then(|file| file.rsplit_once('.'))
            .is_some_and(|(stem, _)| !stem.is_empty());
        match path.split_once('/') {
            Some((first, _)) => {
                // `docs/x.md` with `docs/` gone is stale; `and/or` and
                // `example.com/a.md` never named a repository path.
                let domain = first.contains('.') && !first.starts_with('.');
                !domain && (has_extension || self.root.join(first).exists())
            }
            // A bare file name may live anywhere; it is stale only when the
            // index knows files of that kind and none has this name.
            None => has_extension && self.index_lacks_file_name(path),
        }
    }

    fn index_lacks_file_name(&self, name: &str) -> bool {
        let Some((_, extension)) = name.rsplit_once('.') else {
            return false;
        };
        let suffix = format!("/{name}");
        let dotted = format!(".{extension}");
        self.db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM input_state WHERE substr(path,-length(?1))=?1) \
                 AND NOT EXISTS(SELECT 1 FROM input_state WHERE path=?2 OR substr(path,-length(?3))=?3)",
                params![dotted, name, suffix],
                |row| row.get::<_, bool>(0),
            )
            .unwrap_or(false)
    }
}

/// The best path relation between a steering's path scopes and the topic's
/// paths: the scope containing a topic path outranks one inside it.
fn path_tier(scopes: &[String], topic_paths: &[String]) -> Option<Tier> {
    let mut best = None;
    for scope in scopes {
        for path in topic_paths {
            let tier = if relevance::path_contains(scope, path) {
                Tier::Path
            } else if relevance::path_contains(path, scope) {
                Tier::PathDescendant
            } else {
                continue;
            };
            best = Some(best.map_or(tier, |current: Tier| current.min(tier)));
        }
    }
    best
}

/// Active steerings that a newer active steering appears to replace: the same
/// normalised title, or the same scope and a title sharing most meaningful
/// words. This is an inference for human review, never a lifecycle change.
fn possible_supersessions(active: &[SteeringRow]) -> BTreeMap<String, Vec<String>> {
    let shapes = active
        .iter()
        .map(|row| {
            let title = row
                .title
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .to_lowercase();
            let words = relevance::meaningful_terms(terms(&row.title))
                .into_iter()
                .map(|word| word.to_lowercase())
                .filter(|word| !word.is_empty())
                .collect::<BTreeSet<_>>();
            let scope = row.scope.iter().cloned().collect::<BTreeSet<_>>();
            (row, title, words, scope)
        })
        .collect::<Vec<_>>();
    let mut possible: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (older, title, words, scope) in &shapes {
        for (newer, newer_title, newer_words, newer_scope) in &shapes {
            if newer.sequence <= older.sequence {
                continue;
            }
            let same_title = !title.is_empty() && title == newer_title;
            let overlap = words.intersection(newer_words).count();
            let union = words.union(newer_words).count();
            let similar = scope == newer_scope
                && words.len() >= 2
                && newer_words.len() >= 2
                && overlap * 5 >= union * 3;
            if same_title || similar {
                possible
                    .entry(older.id.clone())
                    .or_default()
                    .push(newer.id.clone());
            }
        }
    }
    possible
}

pub(crate) fn steering_is_active(steering: &Value) -> bool {
    if steering["status"] != "active" {
        return false;
    }
    steering["expires_at"]
        .as_str()
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
        .is_none_or(|expires_at| expires_at > Utc::now())
}

/// The consultation form of a steering: lifecycle history is left to
/// `steering.list`, and the compact form keeps cleanup flags.
pub(crate) fn consultation_forms(steering: Value) -> (Value, Value) {
    let mut full = steering;
    if let Some(object) = full.as_object_mut() {
        for key in [
            "history",
            "revision",
            "retired_at",
            "retired_by",
            "retired_reason",
            "superseded_by",
            "status",
        ] {
            object.remove(key);
        }
    }
    let mut compact = json!({
        "id": full["id"],
        "priority": full["priority"],
        "title": full["title"],
        "instruction": trim_text(full["instruction"].as_str().unwrap_or(""), 240),
        "scope": crate::first_values(&full["scope"], 4),
    });
    if let Some(tier) = full.get("match") {
        compact["match"] = tier.clone();
    }
    if needs_cleanup(&full) {
        compact["stale_references"] = crate::first_values(&full["stale_references"], 3);
        if let Some(newer) = full.get("possibly_superseded_by") {
            compact["possibly_superseded_by"] = newer.clone();
        }
    }
    (full, compact)
}

/// Whether a returned steering or decision names missing paths or appears to
/// be replaced by a newer record.
pub(crate) fn needs_cleanup(record: &Value) -> bool {
    record["stale_references"]
        .as_array()
        .is_some_and(|stale| !stale.is_empty())
        || record.get("possibly_superseded_by").is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::{TempDir, tempdir};

    fn fixture() -> (TempDir, Service) {
        let d = tempdir().unwrap();
        fs::write(
            d.path().join("Cargo.toml"),
            "[package]\nname='demo'\nversion='0.1.0'\nedition='2024'\n",
        )
        .unwrap();
        fs::create_dir(d.path().join("src")).unwrap();
        fs::write(d.path().join("src/lib.rs"), "pub fn demo() {}\n").unwrap();
        let service = Service::open(d.path()).unwrap();
        (d, service)
    }

    fn steering(title: &str, instruction: &str, scope: &[&str]) -> RecordSteering {
        RecordSteering {
            title: title.into(),
            instruction: instruction.into(),
            scope: scope.iter().map(|entry| (*entry).to_owned()).collect(),
            priority: "normal".into(),
            status: "active".into(),
            expires_at: None,
            supersedes: vec![],
            recorded_by: String::new(),
        }
    }

    fn record(service: &Service, title: &str, instruction: &str, scope: &[&str]) -> String {
        service
            .record_steering(steering(title, instruction, scope))
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    fn listed(service: &Service, query: Option<&str>, status: Option<&str>) -> Vec<String> {
        service.steering_list(query, status, 50).unwrap()["steerings"]
            .as_array()
            .unwrap()
            .iter()
            .map(|steering| steering["id"].as_str().unwrap().to_owned())
            .collect()
    }

    fn retire(id: &str) -> RetireSteering {
        RetireSteering {
            id: id.into(),
            retired_by: "maintainer".into(),
            reason: "no longer applies".into(),
        }
    }

    #[test]
    fn retiring_keeps_history_and_leaves_the_active_list() {
        let (_d, service) = fixture();
        let id = record(&service, "Use the cache", "Read through the cache", &[]);
        let missing_reason = service
            .retire_steering(RetireSteering {
                reason: " ".into(),
                ..retire(&id)
            })
            .unwrap_err();
        assert!(missing_reason.to_string().contains("reason is required"));

        let retired = service.retire_steering(retire(&id)).unwrap();
        assert_eq!(retired["status"], "retired");
        assert_eq!(retired["retired_by"], "maintainer");
        assert_eq!(retired["retired_reason"], "no longer applies");
        assert_eq!(retired["history"][0]["action"], "retired");
        assert_eq!(retired["history"][0]["actor"], "maintainer");
        assert!(listed(&service, None, None).is_empty());
        assert_eq!(listed(&service, None, Some("retired")), [id.as_str()]);
        assert_eq!(listed(&service, None, Some("all")), [id.as_str()]);

        let again = service.retire_steering(retire(&id)).unwrap_err();
        assert!(
            again.to_string().contains("only active steerings"),
            "{again}"
        );
        let unknown = service.retire_steering(retire("STR-9999")).unwrap_err();
        assert!(
            unknown.to_string().contains("unknown steering"),
            "{unknown}"
        );
        let filter = service.steering_list(None, Some("enabled"), 5).unwrap_err();
        assert!(
            filter
                .to_string()
                .contains("unsupported steering list status")
        );
    }

    #[test]
    fn superseding_retires_targets_atomically_and_links_both_ways() {
        let (_d, service) = fixture();
        let old = record(&service, "Format with rustfmt", "Run cargo fmt", &[]);
        let other = record(&service, "Lint", "Run clippy", &[]);

        let mut unattributed = steering("Format", "Run cargo fmt --all", &[]);
        unattributed.supersedes = vec![old.clone()];
        let error = service.record_steering(unattributed).unwrap_err();
        assert!(error.to_string().contains("recorded_by is required"));

        // One unknown target rejects the whole record; nothing is retired.
        let mut partial = steering("Format", "Run cargo fmt --all", &[]);
        partial.supersedes = vec![old.clone(), "STR-0404".into()];
        partial.recorded_by = "maintainer".into();
        assert!(service.record_steering(partial).is_err());
        assert_eq!(listed(&service, None, None).len(), 2);

        let mut replacement = steering("Format the workspace", "Run cargo fmt --all", &[]);
        replacement.supersedes = vec![old.clone()];
        replacement.recorded_by = "maintainer".into();
        let new = service.record_steering(replacement).unwrap();
        let new_id = new["id"].as_str().unwrap();
        assert_eq!(new["supersedes"], json!([old]));

        let superseded = service.steering_resource(&old).unwrap();
        assert_eq!(superseded["status"], "retired");
        assert_eq!(superseded["superseded_by"], json!([new_id]));
        assert_eq!(superseded["history"][0]["action"], "superseded");
        assert_eq!(
            superseded["history"][0]["reason"],
            format!("superseded by {new_id}")
        );
        assert_eq!(listed(&service, None, None), [new_id.to_owned(), other]);

        let mut again = steering("Format again", "Run cargo fmt", &[]);
        again.supersedes = vec![old.clone()];
        again.recorded_by = "maintainer".into();
        let error = service.record_steering(again).unwrap_err();
        assert!(error.to_string().contains("cannot supersede steering"));
    }

    #[test]
    fn status_filter_runs_before_the_limit() {
        let (_d, service) = fixture();
        let governing = record(
            &service,
            "Cache policy",
            "Invalidate the cache on write",
            &["cache"],
        );
        let mut expired = steering("Cache policy", "Old cache rule", &["cache"]);
        expired.expires_at = Some("2001-01-01T00:00:00Z".into());
        service.record_steering(expired).unwrap();
        for index in 0..8 {
            let id = record(
                &service,
                &format!("Cache rule {index}"),
                "Cache everything",
                &["cache"],
            );
            service.retire_steering(retire(&id)).unwrap();
        }
        for query in [None, Some("cache")] {
            let first = service.steering_list(query, None, 1).unwrap();
            assert_eq!(first["steerings"][0]["id"], governing.as_str(), "{query:?}");
            assert_eq!(first["count"], 1);
        }
        assert_eq!(listed(&service, None, Some("all")).len(), 10);
        let consulted = service
            .governing_steerings("cache invalidation", 1)
            .unwrap();
        assert_eq!(consulted[0]["id"], governing.as_str());
    }

    #[test]
    fn path_scopes_match_ancestors_and_descendants_not_siblings() {
        let (_d, service) = fixture();
        let global = record(&service, "Be careful", "Prefer small diffs", &[]);
        let concept = record(
            &service,
            "Renderer",
            "Keep the Renderer stateless",
            &["Renderer"],
        );
        let descendant = record(
            &service,
            "UI",
            "Use the shared widgets",
            &["crates/mule_godot/src/app/ui"],
        );
        let ancestor = record(
            &service,
            "Godot crate",
            "Run the Godot app tests",
            &["crates/mule_godot/"],
        );
        let exact = record(
            &service,
            "App",
            "Keep app state in one place",
            &["./crates/mule_godot/src/app"],
        );
        record(
            &service,
            "Core app",
            "Keep core app pure",
            &["crates/mule_core/src/app"],
        );
        record(
            &service,
            "Application",
            "Application src rules",
            &["crates/mule_godot/src/application"],
        );
        record(
            &service,
            "Case",
            "Case-sensitive",
            &["Crates/mule_godot/src/app"],
        );

        let path_only = service
            .steering_list(Some("crates/mule_godot/src/app"), None, 20)
            .unwrap();
        let ids = path_only["steerings"]
            .as_array()
            .unwrap()
            .iter()
            .map(|steering| {
                (
                    steering["id"].as_str().unwrap(),
                    steering["match"].as_str().unwrap(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            [
                (exact.as_str(), "path"),
                (ancestor.as_str(), "path"),
                (descendant.as_str(), "path_descendant"),
                (global.as_str(), "global"),
            ]
        );

        let mixed = listed(
            &service,
            Some("Renderer changes in crates/mule_godot/src/app/"),
            None,
        );
        assert_eq!(mixed, [exact, ancestor, descendant, concept, global]);
    }

    #[test]
    fn stale_references_are_reported_without_failing() {
        let (d, service) = fixture();
        fs::create_dir(d.path().join("docs")).unwrap();
        fs::write(d.path().join("docs/guide.md"), "guide\n").unwrap();
        let recorded = service
            .record_steering(steering(
                "Agent build",
                "Run `scripts/cargo-agent.sh` (see docs/guide.md, docs/gone.md and src/lib.rs:3) and/or https://example.com/a.md before /usr/bin/env checks.",
                &["src/lib.rs"],
            ))
            .unwrap();
        assert_eq!(
            recorded["stale_references"],
            json!(["scripts/cargo-agent.sh", "docs/gone.md"])
        );
        assert!(
            recorded["warnings"][0]
                .as_str()
                .unwrap()
                .contains("scripts/cargo-agent.sh")
        );
        let listed = service.steering_list(None, None, 5).unwrap();
        assert_eq!(
            listed["steerings"][0]["stale_references"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert!(listed["steerings"][0]["stale_note"].is_string());

        let consultation = service.consult("Agent build scripts", 4_000).unwrap();
        assert_eq!(
            consultation["steerings"][0]["stale_references"][0],
            "scripts/cargo-agent.sh"
        );
        let next_steps = consultation["next_steps"].to_string();
        assert!(
            next_steps.contains("do not retire them yourself"),
            "{next_steps}"
        );

        let clean = service
            .record_steering(steering("Docs", "Follow docs/guide.md", &[]))
            .unwrap();
        assert_eq!(clean["stale_references"], json!([]));
        assert!(clean.get("warnings").is_none());
    }

    #[test]
    fn decisions_report_stale_references() {
        let (_d, service) = fixture();
        let decision = service
            .record_decision(crate::RecordDecision {
                title: "Build entry point".into(),
                status: "accepted".into(),
                reason: "Agents build with scripts/cargo-agent.sh".into(),
                applies_to: vec!["src/lib.rs".into()],
                consequences: vec![],
                supersedes: vec![],
                recorded_by: String::new(),
                materialize: false,
            })
            .unwrap();
        let resource = service
            .decision_resource(decision["id"].as_str().unwrap())
            .unwrap();
        assert_eq!(
            resource["stale_references"],
            json!(["scripts/cargo-agent.sh"])
        );
    }

    #[test]
    fn newer_steering_with_the_same_title_is_flagged_for_review() {
        let (_d, service) = fixture();
        let old = record(
            &service,
            "Release checklist",
            "Tag before publishing",
            &["release"],
        );
        let new = record(
            &service,
            "Release  Checklist",
            "Publish, then tag",
            &["release"],
        );
        record(&service, "Unrelated", "Something else", &["release"]);
        let older = service.steering_resource(&old).unwrap();
        assert_eq!(older["possibly_superseded_by"], json!([new]));
        // Inference never changes the lifecycle.
        assert_eq!(older["status"], "active");
        let newer = service.steering_resource(&new).unwrap();
        assert!(newer.get("possibly_superseded_by").is_none());
        let consultation = service.consult("release checklist", 4_000).unwrap();
        assert!(consultation["next_steps"].to_string().contains(&old));
    }

    #[test]
    fn existing_steering_stores_migrate_additively() {
        let (d, service) = fixture();
        service
            .db
            .execute_batch(
                "DROP TABLE steering_history; DROP TABLE steerings;
                 CREATE TABLE steerings(id TEXT PRIMARY KEY,sequence INTEGER NOT NULL,status TEXT NOT NULL,priority TEXT NOT NULL,title TEXT NOT NULL,instruction TEXT NOT NULL,scope TEXT NOT NULL,expires_at TEXT,revision TEXT NOT NULL,created_at TEXT NOT NULL);
                 INSERT INTO steerings VALUES ('STR-0001',1,'active','high','Legacy','Keep legacy rules','[]',NULL,'b3:x','2026-01-01T00:00:00Z');
                 DELETE FROM metadata WHERE key='schema_fingerprint';",
            )
            .unwrap();
        drop(service);
        let service = Service::open(d.path()).unwrap();
        let legacy = service.steering_resource("STR-0001").unwrap();
        assert_eq!(legacy["status"], "active");
        assert_eq!(legacy["supersedes"], json!([]));
        assert_eq!(legacy["history"], json!([]));
        assert_eq!(listed(&service, None, None), ["STR-0001"]);
        let mut replacement = steering("Current", "Keep current rules", &[]);
        replacement.supersedes = vec!["STR-0001".into()];
        replacement.recorded_by = "maintainer".into();
        let current = service.record_steering(replacement).unwrap();
        assert_eq!(current["id"], "STR-0002");
        assert_eq!(
            service.steering_resource("STR-0001").unwrap()["superseded_by"],
            json!(["STR-0002"])
        );
        // Migration is idempotent.
        drop(service);
        let service = Service::open(d.path()).unwrap();
        assert_eq!(listed(&service, None, Some("all")).len(), 2);
    }
}

//! Human-reviewed ownership/invariant models, separate from inferred architecture.

use crate::coordination::{Coordinator, normalize_path};
use anyhow::{Context, Result, ensure};
use chrono::Utc;
use rand::random;
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeSet;

pub(crate) fn approved_for_root(
    root: &std::path::Path,
    control: &crate::execution::ExecutionControl,
) -> Result<Vec<Value>> {
    let path = Coordinator::state_path(root, control)?.join("coordination.sqlite3");
    if !path.exists() {
        return Ok(vec![]);
    }
    let db =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    db.busy_timeout(std::time::Duration::from_secs(2))?;
    let exists: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='domain_models')",
        [],
        |row| row.get(0),
    )?;
    if !exists {
        return Ok(vec![]);
    }
    db.prepare(
        "SELECT payload FROM domain_models WHERE status='accepted' ORDER BY rowid DESC LIMIT 100",
    )?
    .query_map([], |row| row.get::<_, String>(0))?
    .map(|row| Ok(serde_json::from_str::<Value>(&row?)?))
    .collect()
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConceptOwner {
    pub concept: String,
    pub module: String,
    pub paths: Vec<String>,
    pub invariants: Vec<String>,
    /// Named APIs/modules allowed to mutate this concept.
    pub mutation_rights: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DependencyDisposition {
    Allowed,
    Forbidden,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DependencyRule {
    pub from_module: String,
    pub to_module: String,
    pub disposition: DependencyDisposition,
    pub rationale: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DomainModelRequest {
    pub title: String,
    pub rationale: String,
    pub owners: Vec<ConceptOwner>,
    #[serde(default)]
    pub dependencies: Vec<DependencyRule>,
    /// Accepting this proposal will supersede the named accepted model atomically.
    pub supersedes: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ModelReview {
    Accept,
    Reject,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DomainReviewRequest {
    pub id: String,
    pub review: ModelReview,
    pub reviewed_by: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ModelStatus {
    Proposed,
    Accepted,
    Rejected,
    Superseded,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DomainModel {
    id: String,
    title: String,
    rationale: String,
    owners: Vec<ConceptOwner>,
    dependencies: Vec<DependencyRule>,
    status: ModelStatus,
    supersedes: Option<String>,
    superseded_by: Option<String>,
    proposed_head: Option<String>,
    proposed_workspace_digest: String,
    created_at: i64,
    reviewed_by: Option<String>,
    review_reason: Option<String>,
    reviewed_at: Option<i64>,
}

impl Coordinator {
    fn domain_db(&self) -> Result<rusqlite::Connection> {
        let db = self.db()?;
        db.execute_batch("CREATE TABLE IF NOT EXISTS domain_models(id TEXT PRIMARY KEY,status TEXT NOT NULL,payload TEXT NOT NULL)")?;
        Ok(db)
    }

    pub(crate) fn domain_propose(&self, request: DomainModelRequest) -> Result<Value> {
        text(&request.title, 200)?;
        text(&request.rationale, 8000)?;
        ensure!(
            !request.owners.is_empty()
                && request.owners.len() <= 100
                && request.dependencies.len() <= 200,
            "domain model size exceeds bounds"
        );
        let mut owners = request.owners;
        let mut concepts = BTreeSet::new();
        let mut modules = BTreeSet::new();
        for owner in &mut owners {
            text(&owner.concept, 200)?;
            text(&owner.module, 200)?;
            ensure!(
                concepts.insert(owner.concept.clone()),
                "concept has more than one canonical owner"
            );
            modules.insert(owner.module.clone());
            ensure!(
                !owner.paths.is_empty()
                    && owner.paths.len() <= 100
                    && owner.invariants.len() <= 100
                    && owner.mutation_rights.len() <= 100,
                "invalid concept ownership bounds"
            );
            for path in &mut owner.paths {
                *path = normalize_path(&self.root, path)?;
            }
            for value in owner.invariants.iter().chain(&owner.mutation_rights) {
                text(value, 4000)?;
            }
        }
        for rule in &request.dependencies {
            ensure!(
                modules.contains(&rule.from_module) && modules.contains(&rule.to_module),
                "dependency rule names an undeclared module"
            );
            text(&rule.rationale, 4000)?;
        }
        if let Some(id) = &request.supersedes {
            ensure!(
                self.domain_record(id)?.status == ModelStatus::Accepted,
                "only an accepted model can be superseded"
            );
        }
        let model = DomainModel {
            id: format!("model_{:032x}", random::<u128>()),
            title: request.title,
            rationale: request.rationale,
            owners,
            dependencies: request.dependencies,
            status: ModelStatus::Proposed,
            supersedes: request.supersedes,
            superseded_by: None,
            proposed_head: self.git_optional(&["rev-parse", "--verify", "HEAD"])?,
            proposed_workspace_digest: self.workspace_fingerprint()?,
            created_at: Utc::now().timestamp(),
            reviewed_by: None,
            review_reason: None,
            reviewed_at: None,
        };
        self.domain_db()?.execute(
            "INSERT INTO domain_models(id,status,payload) VALUES (?1,'proposed',?2)",
            params![model.id, serde_json::to_string(&model)?],
        )?;
        Ok(serde_json::to_value(model)?)
    }

    fn domain_record(&self, id: &str) -> Result<DomainModel> {
        let raw: String = self
            .domain_db()?
            .query_row(
                "SELECT payload FROM domain_models WHERE id=?1",
                [id],
                |row| row.get(0),
            )
            .optional()?
            .context("unknown domain model")?;
        Ok(serde_json::from_str(&raw)?)
    }

    pub(crate) fn domain_get(&self, id: &str) -> Result<Value> {
        Ok(serde_json::to_value(self.domain_record(id)?)?)
    }

    pub(crate) fn domain_list(&self, limit: usize, offset: usize) -> Result<Value> {
        let db = self.domain_db()?;
        let total: usize =
            db.query_row("SELECT COUNT(*) FROM domain_models", [], |row| row.get(0))?;
        let models = db
            .prepare("SELECT payload FROM domain_models ORDER BY rowid DESC LIMIT ?1 OFFSET ?2")?
            .query_map(
                params![
                    limit.clamp(1, 100) as i64,
                    offset.min(i64::MAX as usize) as i64
                ],
                |row| row.get::<_, String>(0),
            )?
            .map(|row| Ok(serde_json::from_str::<DomainModel>(&row?)?))
            .collect::<Result<Vec<_>>>()?;
        let next = offset.saturating_add(models.len());
        Ok(
            json!({"models":models,"page":{"total":total,"has_more":next<total,"next_offset":(next<total).then_some(next)}}),
        )
    }

    pub(crate) fn domain_review(&self, request: DomainReviewRequest) -> Result<Value> {
        text(&request.reviewed_by, 200)?;
        text(&request.reason, 8000)?;
        let mut db = self.domain_db()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let raw: String = tx
            .query_row(
                "SELECT payload FROM domain_models WHERE id=?1",
                [&request.id],
                |row| row.get(0),
            )
            .optional()?
            .context("unknown domain model")?;
        let mut model: DomainModel = serde_json::from_str(&raw)?;
        ensure!(
            model.status == ModelStatus::Proposed,
            "only a proposed model can be reviewed; decisions are terminal"
        );
        let accepted = matches!(request.review, ModelReview::Accept);
        if accepted {
            let existing = tx
                .prepare("SELECT payload FROM domain_models WHERE status='accepted'")?
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for raw in existing {
                let existing: DomainModel = serde_json::from_str(&raw)?;
                if model.supersedes.as_deref() == Some(&existing.id) {
                    continue;
                }
                ensure!(
                    !model
                        .owners
                        .iter()
                        .any(|new| existing.owners.iter().any(|old| old.concept == new.concept)),
                    "accepted model already owns a proposed concept; explicitly supersede it"
                );
            }
        }
        if accepted && let Some(previous) = &model.supersedes {
            let raw: String = tx.query_row(
                "SELECT payload FROM domain_models WHERE id=?1",
                [previous],
                |row| row.get(0),
            )?;
            let mut prior: DomainModel = serde_json::from_str(&raw)?;
            ensure!(
                prior.status == ModelStatus::Accepted,
                "superseded model changed before review"
            );
            prior.status = ModelStatus::Superseded;
            prior.superseded_by = Some(model.id.clone());
            tx.execute(
                "UPDATE domain_models SET status='superseded',payload=?1 WHERE id=?2",
                params![serde_json::to_string(&prior)?, previous],
            )?;
        }
        model.status = if accepted {
            ModelStatus::Accepted
        } else {
            ModelStatus::Rejected
        };
        model.reviewed_by = Some(request.reviewed_by);
        model.review_reason = Some(request.reason);
        model.reviewed_at = Some(Utc::now().timestamp());
        tx.execute(
            "UPDATE domain_models SET status=?1,payload=?2 WHERE id=?3",
            params![
                if accepted { "accepted" } else { "rejected" },
                serde_json::to_string(&model)?,
                model.id
            ],
        )?;
        tx.commit()?;
        Ok(serde_json::to_value(model)?)
    }
}

fn text(value: &str, max: usize) -> Result<()> {
    ensure!(
        !value.trim().is_empty() && value.len() <= max && !value.contains('\0'),
        "invalid domain model text"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::ExecutionControl;
    fn request() -> DomainModelRequest {
        DomainModelRequest {
            title: "Ownership".into(),
            rationale: "One owner of session state".into(),
            owners: vec![ConceptOwner {
                concept: "session".into(),
                module: "coordination".into(),
                paths: vec!["src/coordination.rs".into()],
                invariants: vec!["Expiry is terminal".into()],
                mutation_rights: vec!["Coordinator".into()],
            }],
            dependencies: vec![],
            supersedes: None,
        }
    }
    #[test]
    fn only_human_review_activates_models_and_supersession_is_atomic() {
        let temp = tempfile::tempdir().unwrap();
        let coord = Coordinator::open(temp.path(), ExecutionControl::default()).unwrap();
        let proposed = coord.domain_propose(request()).unwrap();
        assert!(
            approved_for_root(&coord.root, &coord.control)
                .unwrap()
                .is_empty()
        );
        let id = proposed["id"].as_str().unwrap();
        coord
            .domain_review(DomainReviewRequest {
                id: id.into(),
                review: ModelReview::Accept,
                reviewed_by: "human".into(),
                reason: "Approved lifecycle contract".into(),
            })
            .unwrap();
        assert_eq!(
            approved_for_root(&coord.root, &coord.control)
                .unwrap()
                .len(),
            1
        );
        assert!(
            coord
                .domain_review(DomainReviewRequest {
                    id: id.into(),
                    review: ModelReview::Reject,
                    reviewed_by: "human".into(),
                    reason: "late".into()
                })
                .is_err()
        );
        let mut replacement = request();
        replacement.supersedes = Some(id.into());
        let next = coord.domain_propose(replacement).unwrap();
        coord
            .domain_review(DomainReviewRequest {
                id: next["id"].as_str().unwrap().into(),
                review: ModelReview::Accept,
                reviewed_by: "human".into(),
                reason: "Explicit replacement".into(),
            })
            .unwrap();
        assert_eq!(
            approved_for_root(&coord.root, &coord.control)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(coord.domain_get(id).unwrap()["status"], "superseded");
    }
}

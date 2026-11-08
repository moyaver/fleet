// SPDX-License-Identifier: AGPL-3.0-only
//! Failure triage (issue #84): whether a failure fingerprint is known (tracked by an issue) or
//! resolved. Every fingerprint gets a `failure_triage` row when it is first seen, so its first
//! sighting is recorded; a maintainer's change is written with an `audit_events` row in the same
//! transaction, so a state never changes without a record of who changed it.

use fleet_protocol::rest::history::{FailureTriageView, TriageState};
use sqlx::PgConnection;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{Result, Store, auth::AuditActor};

/// The audit action of every triage change; `details` says from what state to what.
const AUDIT_ACTION: &str = "failure.triage";

/// A row as the views show it.
struct Row {
    fingerprint: String,
    first_seen_at: OffsetDateTime,
    state: Option<String>,
    issue_url: Option<String>,
    updated_at: Option<OffsetDateTime>,
}

impl Row {
    fn into_view(self) -> FailureTriageView {
        FailureTriageView {
            fingerprint: self.fingerprint,
            state: self.state.as_deref().and_then(parse_state),
            issue_url: self.issue_url,
            first_seen_at: self.first_seen_at,
            updated_at: self.updated_at,
        }
    }
}

fn parse_state(s: &str) -> Option<TriageState> {
    match s {
        "known" => Some(TriageState::Known),
        "resolved" => Some(TriageState::Resolved),
        _ => None,
    }
}

fn state_name(state: TriageState) -> &'static str {
    match state {
        TriageState::Known => "known",
        TriageState::Resolved => "resolved",
    }
}

impl Store {
    /// The triage of `fingerprints` in a project. A fingerprint the project has never seen is
    /// absent from the result.
    pub async fn failure_triage(
        &self,
        project_id: Uuid,
        fingerprints: &[String],
    ) -> Result<Vec<FailureTriageView>> {
        let rows = sqlx::query_as!(
            Row,
            "SELECT fingerprint, first_seen_at, state, issue_url, updated_at
               FROM failure_triage
              WHERE project_id = $1 AND fingerprint = ANY($2)",
            project_id,
            fingerprints
        )
        .fetch_all(self.pool())
        .await?;
        Ok(rows.into_iter().map(Row::into_view).collect())
    }

    /// The triage of the failure a flow execution had, when it is a final failure.
    pub async fn execution_failure_triage(
        &self,
        project_id: Uuid,
        flow_execution_id: Uuid,
    ) -> Result<Option<FailureTriageView>> {
        let row = sqlx::query_as!(
            Row,
            "SELECT t.fingerprint, t.first_seen_at, t.state, t.issue_url, t.updated_at
               FROM failure_fingerprints ff
               JOIN failure_triage t
                 ON t.project_id = ff.project_id AND t.fingerprint = ff.fingerprint
              WHERE ff.project_id = $1 AND ff.flow_execution_id = $2",
            project_id,
            flow_execution_id
        )
        .fetch_optional(self.pool())
        .await?;
        Ok(row.map(Row::into_view))
    }

    /// Marks a fingerprint `state`, with the issue or change it points at, and audits the change.
    /// Returns the triage after the call, or `None` when the project has never seen the
    /// fingerprint. Setting what is already recorded changes nothing and writes no audit row.
    pub async fn set_failure_triage(
        &self,
        project_id: Uuid,
        fingerprint: &str,
        state: TriageState,
        issue_url: Option<&str>,
        actor: AuditActor,
    ) -> Result<Option<FailureTriageView>> {
        let mut tx = self.pool().begin().await?;
        let result = change(
            &mut tx,
            project_id,
            fingerprint,
            Some(state),
            issue_url,
            actor,
        )
        .await?;
        tx.commit().await?;
        Ok(result)
    }

    /// Clears a fingerprint back to untriaged, and audits the change. The same rules as
    /// [`set_failure_triage`](Self::set_failure_triage) apply.
    pub async fn clear_failure_triage(
        &self,
        project_id: Uuid,
        fingerprint: &str,
        actor: AuditActor,
    ) -> Result<Option<FailureTriageView>> {
        let mut tx = self.pool().begin().await?;
        let result = change(&mut tx, project_id, fingerprint, None, None, actor).await?;
        tx.commit().await?;
        Ok(result)
    }
}

/// Writes one triage change and its audit row inside `tx`. `None` when there is no such
/// fingerprint in the project (nothing is written).
async fn change(
    tx: &mut PgConnection,
    project_id: Uuid,
    fingerprint: &str,
    state: Option<TriageState>,
    issue_url: Option<&str>,
    actor: AuditActor,
) -> Result<Option<FailureTriageView>> {
    let current = sqlx::query!(
        "SELECT state, issue_url FROM failure_triage
          WHERE project_id = $1 AND fingerprint = $2
          FOR UPDATE",
        project_id,
        fingerprint
    )
    .fetch_optional(&mut *tx)
    .await?;
    let Some(current) = current else {
        return Ok(None);
    };
    let new_state = state.map(state_name);
    if current.state.as_deref() != new_state || current.issue_url.as_deref() != issue_url {
        sqlx::query!(
            "UPDATE failure_triage SET state = $3, issue_url = $4, updated_at = now()
              WHERE project_id = $1 AND fingerprint = $2",
            project_id,
            fingerprint,
            new_state,
            issue_url
        )
        .execute(&mut *tx)
        .await?;
        let (kind, id) = actor.parts();
        sqlx::query!(
            "INSERT INTO audit_events (actor_kind, actor_id, project_id, action, details)
             VALUES ($1, $2, $3, $4, $5)",
            kind,
            id,
            project_id,
            AUDIT_ACTION,
            serde_json::json!({
                "fingerprint": fingerprint,
                "from": current.state,
                "to": new_state,
                "issue_url": issue_url,
            })
        )
        .execute(&mut *tx)
        .await?;
    }
    let row = sqlx::query_as!(
        Row,
        "SELECT fingerprint, first_seen_at, state, issue_url, updated_at
           FROM failure_triage WHERE project_id = $1 AND fingerprint = $2",
        project_id,
        fingerprint
    )
    .fetch_one(&mut *tx)
    .await?;
    Ok(Some(row.into_view()))
}

#[cfg(test)]
mod tests {
    use fleet_protocol::rest::history::TriageState::{Known, Resolved};
    use serde_json::{Value, json};
    use sqlx::PgPool;

    use super::*;

    const FP: &str = "v1:0123456789abcdef";
    const ISSUE: &str = "https://github.com/o/r/issues/84";

    fn store(pool: PgPool) -> Store {
        Store {
            pool,
            origin: Uuid::now_v7(),
        }
    }

    /// What ingest does on a fingerprint's first sighting.
    async fn seen(store: &Store, project: Uuid) {
        sqlx::query!(
            "INSERT INTO failure_triage (project_id, fingerprint) VALUES ($1, $2)",
            project,
            FP
        )
        .execute(store.pool())
        .await
        .unwrap();
    }

    /// The triage audit rows, oldest first: who, and the details of each change.
    async fn audit(store: &Store) -> Vec<(String, Option<Uuid>, Value)> {
        sqlx::query!(
            "SELECT actor_kind, actor_id, details FROM audit_events
              WHERE action = 'failure.triage' ORDER BY at, id"
        )
        .fetch_all(store.pool())
        .await
        .unwrap()
        .into_iter()
        .map(|r| (r.actor_kind, r.actor_id, r.details))
        .collect()
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn a_change_is_recorded_with_its_actor_and_nothing_else_is(pool: PgPool) {
        let store = store(pool);
        let project = store.upsert_project("app", "App").await.unwrap();
        seen(&store, project).await;
        let before = store.failure_triage(project, &[FP.into()]).await.unwrap();
        let first_seen = before[0].first_seen_at;
        assert_eq!(before[0].state, None, "a seen failure starts untriaged");
        let who = Uuid::now_v7();

        let known = store
            .set_failure_triage(project, FP, Known, Some(ISSUE), AuditActor::User(who))
            .await
            .unwrap()
            .expect("the fingerprint exists");
        assert_eq!(known.state, Some(Known));
        assert_eq!(known.issue_url.as_deref(), Some(ISSUE));
        assert!(known.updated_at.is_some());
        assert_eq!(
            known.first_seen_at, first_seen,
            "changing state keeps first seen"
        );

        // The same state again changes nothing, so it writes no audit row.
        store
            .set_failure_triage(project, FP, Known, Some(ISSUE), AuditActor::User(who))
            .await
            .unwrap();
        let resolved = store
            .set_failure_triage(project, FP, Resolved, None, AuditActor::User(who))
            .await
            .unwrap()
            .unwrap();
        assert_eq!((resolved.state, resolved.issue_url), (Some(Resolved), None));

        let cleared = store
            .clear_failure_triage(project, FP, AuditActor::User(who))
            .await
            .unwrap()
            .unwrap();
        assert_eq!((cleared.state, cleared.issue_url), (None, None));

        let rows = audit(&store).await;
        assert_eq!(
            rows,
            vec![
                (
                    "user".into(),
                    Some(who),
                    json!({"fingerprint": FP, "from": null, "to": "known", "issue_url": ISSUE})
                ),
                (
                    "user".into(),
                    Some(who),
                    json!({"fingerprint": FP, "from": "known", "to": "resolved", "issue_url": null})
                ),
                (
                    "user".into(),
                    Some(who),
                    json!({"fingerprint": FP, "from": "resolved", "to": null, "issue_url": null})
                ),
            ],
            "three changes, three rows, and the repeat wrote none"
        );
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn a_fingerprint_the_project_never_saw_is_not_triaged(pool: PgPool) {
        let store = store(pool);
        let project = store.upsert_project("app", "App").await.unwrap();
        let who = AuditActor::User(Uuid::now_v7());
        assert!(
            store
                .set_failure_triage(project, FP, Known, Some(ISSUE), who)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .clear_failure_triage(project, FP, who)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            audit(&store).await.len(),
            0,
            "a refused change writes no audit row"
        );
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn a_known_failure_without_its_issue_is_refused_by_the_database(pool: PgPool) {
        let store = store(pool);
        let project = store.upsert_project("app", "App").await.unwrap();
        seen(&store, project).await;
        let refused = store
            .set_failure_triage(project, FP, Known, None, AuditActor::System)
            .await;
        assert!(refused.is_err(), "known needs its link");
        let view = store.failure_triage(project, &[FP.into()]).await.unwrap();
        assert_eq!(view[0].state, None, "a refused change changes nothing");
        assert_eq!(
            audit(&store).await.len(),
            0,
            "a refused change writes no audit row"
        );
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn triage_is_read_per_project(pool: PgPool) {
        let store = store(pool);
        let app = store.upsert_project("app", "App").await.unwrap();
        let other = store.upsert_project("other", "Other").await.unwrap();
        seen(&store, app).await;
        store
            .set_failure_triage(app, FP, Known, Some(ISSUE), AuditActor::System)
            .await
            .unwrap();
        assert_eq!(
            store.failure_triage(app, &[FP.into()]).await.unwrap()[0].state,
            Some(Known)
        );
        assert!(
            store
                .failure_triage(other, &[FP.into()])
                .await
                .unwrap()
                .is_empty(),
            "another project's failure is invisible"
        );
    }
}

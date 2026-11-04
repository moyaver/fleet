// SPDX-License-Identifier: AGPL-3.0-only
//! Liveness sweeps (research 05 §6.1, §6.2, §6.8), run by one leader at a time.
//!
//! Heartbeats never touch the database: each instance persists the last contact of its
//! connected runs once per sweep ([`Store::persist_last_seen`]). The leader, holding a session
//! advisory lock ([`Store::try_lead`]), then
//!
//! 1. detaches `active` runs nobody has heard from (a socket on a crashed instance never closed);
//! 2. abandons `detached` (and stuck `finalizing`) runs whose resume grace expired: their open
//!    flows get a `server_inferred` claim of `unknown` (the lowest rank, so late evidence still
//!    wins), unfinished suites become `unknown`, devices `lost`, and the job is re-derived,
//!    which reads `interrupted` if Maestro was running at last contact — never success (I3).

use std::collections::BTreeSet;
use std::time::Duration;

use sqlx::PgConnection;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{
    Result, Store,
    ingest::{Change, EntityRef, bump_version, derive},
};

/// The key of the leader's advisory lock.
pub const LEADER_LOCK: i64 = 0x666c_6565_7431; // "fleet1"

/// What one sweep did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Active runs marked detached.
    pub detached: usize,
    /// Runs abandoned.
    pub abandoned: usize,
    /// Live changes to publish.
    pub changes: Vec<Change>,
}

/// The leader's lock: a dedicated session holding `pg_try_advisory_lock`. Dropping it closes
/// the session, which releases the lock.
#[derive(Debug)]
pub struct Leader {
    conn: PgConnection,
}

impl Leader {
    /// Whether the session (and so the lock) is still alive.
    pub async fn alive(&mut self) -> bool {
        sqlx::query_scalar!("SELECT 1 AS one")
            .fetch_one(&mut self.conn)
            .await
            .is_ok()
    }
}

fn interval(d: Duration) -> sqlx::postgres::types::PgInterval {
    sqlx::postgres::types::PgInterval {
        months: 0,
        days: 0,
        microseconds: i64::try_from(d.as_micros()).unwrap_or(i64::MAX),
    }
}

impl Store {
    /// Becomes the leader if nobody else is.
    pub async fn try_lead(&self) -> Result<Option<Leader>> {
        let mut conn = self.pool().acquire().await?.detach();
        let got = sqlx::query_scalar!("SELECT pg_try_advisory_lock($1) AS \"got!\"", LEADER_LOCK)
            .fetch_one(&mut conn)
            .await?;
        Ok(got.then_some(Leader { conn }))
    }

    /// Records when this instance last heard from its connected runs.
    pub async fn persist_last_seen(&self, seen: &[(Uuid, OffsetDateTime)]) -> Result<()> {
        if seen.is_empty() {
            return Ok(());
        }
        let (ids, at): (Vec<Uuid>, Vec<OffsetDateTime>) = seen.iter().copied().unzip();
        sqlx::query!(
            "UPDATE agent_runs r SET last_frame_at = greatest(r.last_frame_at, u.at)
               FROM unnest($1::uuid[], $2::timestamptz[]) AS u(id, at)
              WHERE r.id = u.id",
            &ids,
            &at
        )
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// One leader sweep. `silent_after`: an active run with no contact for this long is
    /// detached. `grace`: a detached run is abandoned after this long.
    pub async fn sweep(&self, silent_after: Duration, grace: Duration) -> Result<SweepReport> {
        let mut report = SweepReport::default();
        let detached = sqlx::query!(
            "UPDATE agent_runs r
                SET state = 'detached', detached_at = r.last_frame_at, rev = r.rev + 1
               FROM jobs j
              WHERE j.id = r.job_id AND r.state = 'active' AND r.last_frame_at < now() - $1::interval
              RETURNING r.id, r.project_id, j.pipeline_attempt_id",
            interval(silent_after)
        )
        .fetch_all(self.pool())
        .await?;
        report.detached = detached.len();
        for r in detached {
            let mut tx = self.pool().begin().await?;
            if let Some(c) = bump_version(
                &mut tx,
                r.project_id,
                r.pipeline_attempt_id,
                BTreeSet::from([EntityRef::AgentRun(r.id)]),
            )
            .await?
            {
                report.changes.push(c);
            }
            tx.commit().await?;
        }
        loop {
            let mut tx = self.pool().begin().await?;
            let Some(run) = sqlx::query!(
                "SELECT r.id, r.project_id, r.job_id, r.committed_seq, j.pipeline_attempt_id
                   FROM agent_runs r JOIN jobs j ON j.id = r.job_id
                  WHERE (r.state = 'detached' AND r.detached_at < now() - $1::interval)
                     OR (r.state = 'finalizing' AND r.last_frame_at < now() - $1::interval)
                  LIMIT 1
                  FOR UPDATE OF r SKIP LOCKED",
                interval(grace)
            )
            .fetch_optional(&mut *tx)
            .await?
            else {
                break;
            };
            let entities = abandon(&mut tx, run.id, run.job_id, run.committed_seq).await?;
            if let Some(c) =
                bump_version(&mut tx, run.project_id, run.pipeline_attempt_id, entities).await?
            {
                report.changes.push(c);
            }
            tx.commit().await?;
            report.abandoned += 1;
            tracing::info!(agent_run_id = %run.id, "agent run abandoned after its resume grace");
        }
        Ok(report)
    }

    /// Deletes expired agent tokens, replay records and browser sessions.
    pub async fn cleanup_credentials(&self) -> Result<u64> {
        let tokens = sqlx::query!(
            "DELETE FROM agent_tokens t
              WHERE t.expires_at < now() - interval '1 day'
                AND NOT EXISTS (SELECT 1 FROM agent_runs r WHERE r.token_id = t.id)"
        )
        .execute(self.pool())
        .await?
        .rows_affected();
        let jti = sqlx::query!("DELETE FROM oidc_jti_seen WHERE expires_at < now()")
            .execute(self.pool())
            .await?
            .rows_affected();
        let sessions = sqlx::query!("DELETE FROM sessions WHERE expires_at < now()")
            .execute(self.pool())
            .await?
            .rows_affected();
        Ok(tokens + jti + sessions)
    }
}

async fn abandon(
    tx: &mut PgConnection,
    run: Uuid,
    job: Uuid,
    committed: i64,
) -> Result<BTreeSet<EntityRef>, sqlx::Error> {
    let mut changed = BTreeSet::from([EntityRef::AgentRun(run)]);
    sqlx::query!(
        "UPDATE agent_runs SET state = 'abandoned', end_reason = 'abandoned', rev = rev + 1
          WHERE id = $1",
        run
    )
    .execute(&mut *tx)
    .await?;
    // The flow may have finished: unknown, not interrupted (research 05 §6.5).
    let flows = sqlx::query_scalar!(
        "INSERT INTO flow_claims (flow_execution_id, source, origin, outcome, claim_order,
                                  observed_at, detail)
         SELECT id, 'server_inferred', 'sweep:' || $1::uuid::text, 'unknown', $2, now(),
                '{\"reason\": \"agent_abandoned\"}'
           FROM flow_executions WHERE agent_run_id = $1 AND state IN ('pending', 'running')
         ON CONFLICT DO NOTHING
         RETURNING flow_execution_id",
        run,
        committed.saturating_add(1)
    )
    .fetch_all(&mut *tx)
    .await?;
    changed.extend(
        derive::flows(tx, &flows)
            .await?
            .into_iter()
            .map(EntityRef::Flow),
    );
    let suites = sqlx::query_scalar!(
        "UPDATE suite_executions SET outcome = 'unknown', outcome_source = 'server_inferred',
                rev = rev + 1
          WHERE agent_run_id = $1 AND outcome IS NULL
          RETURNING id",
        run
    )
    .fetch_all(&mut *tx)
    .await?;
    changed.extend(suites.into_iter().map(EntityRef::Suite));
    let devices = sqlx::query_scalar!(
        "UPDATE device_sessions SET state = 'lost', rev = rev + 1
          WHERE agent_run_id = $1 AND state NOT IN ('released', 'lost')
          RETURNING id",
        run
    )
    .fetch_all(&mut *tx)
    .await?;
    changed.extend(devices.into_iter().map(EntityRef::Device));
    if derive::job(tx, job).await? {
        changed.insert(EntityRef::Job(job));
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use fleet_protocol::{event::EventBody, journal::SourceKind};
    use sqlx::PgPool;

    use super::*;
    use crate::ingest::{
        Transport,
        tests::{Agent, SuiteIds, publisher, suite_run},
    };

    #[sqlx::test(migrations = "../../migrations")]
    async fn an_abandoned_run_is_never_green_and_late_evidence_still_wins(pool: PgPool) {
        let store = Store { pool };
        let p = publisher(&store).await;
        let mut a = Agent::new("900", "e2e");
        let h = store
            .open_run(&p, &a.hello(None), Transport::WebSocket)
            .await
            .unwrap()
            .handle;
        // The run gets as far as one flow started, then the runner vanishes.
        let all = suite_run(&mut a, SuiteIds::new(), &[("login", true)]);
        store.apply_batch(&h, &all[..4]).await.unwrap();
        store.detach(&h, "closed").await.unwrap();

        let leader = store.try_lead().await.unwrap();
        assert!(leader.is_some());
        assert!(
            store.try_lead().await.unwrap().is_none(),
            "one leader at a time"
        );

        // Not yet: within the grace period.
        let r = store
            .sweep(Duration::from_mins(1), Duration::from_mins(15))
            .await
            .unwrap();
        assert_eq!(r.abandoned, 0);
        let r = store
            .sweep(Duration::from_mins(1), Duration::ZERO)
            .await
            .unwrap();
        assert_eq!((r.abandoned, r.changes.len()), (1, 1));
        let snap = store
            .snapshot(p.project_id, h.pipeline_attempt_id)
            .await
            .unwrap()
            .unwrap();
        let flow = &snap.flows[0];
        assert_eq!(
            (
                flow.state.as_str(),
                flow.outcome_source.as_deref(),
                flow.finality.as_str()
            ),
            ("unknown", Some("server_inferred"), "inferred")
        );
        assert_eq!(snap.jobs[0].lifecycle, "closed");
        assert_eq!(snap.jobs[0].outcome.as_deref(), Some("interrupted"));
        assert_eq!(snap.devices[0].state, "lost");

        // The agent comes back: its live result outranks the inference and the job reopens.
        let opened = store
            .open_run(&p, &a.hello(Some(5)), Transport::WebSocket)
            .await
            .unwrap();
        assert_eq!(
            opened.resume,
            fleet_protocol::frame::ResumeOutcome::Reopened
        );
        store.apply_batch(&opened.handle, &all[4..]).await.unwrap();
        let snap = store
            .snapshot(p.project_id, h.pipeline_attempt_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(snap.flows[0].state, "passed");
        assert_eq!(snap.jobs[0].reopened_count, 1);
        drop(leader);
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn silent_active_runs_are_detached(pool: PgPool) {
        let store = Store { pool };
        let p = publisher(&store).await;
        let mut a = Agent::new("901", "e2e");
        let h = store
            .open_run(&p, &a.hello(None), Transport::WebSocket)
            .await
            .unwrap()
            .handle;
        let e = a.event(
            fleet_protocol::journal::Subject::default(),
            SourceKind::AgentObserved,
            &EventBody::RunFinalizing,
        );
        store.apply_batch(&h, &[e]).await.unwrap();
        let old = OffsetDateTime::now_utc() - time::Duration::hours(1);
        sqlx::query!(
            "UPDATE agent_runs SET last_frame_at = $2, state = 'active' WHERE id = $1",
            a.id,
            old
        )
        .execute(store.pool())
        .await
        .unwrap();
        store
            .persist_last_seen(&[(a.id, old + time::Duration::minutes(1))])
            .await
            .unwrap();
        let r = store
            .sweep(Duration::from_mins(1), Duration::from_mins(15))
            .await
            .unwrap();
        assert_eq!(r.detached, 1);
    }
}

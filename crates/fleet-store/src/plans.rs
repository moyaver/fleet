// SPDX-License-Identifier: AGPL-3.0-only
//! Fleet plans (ADR-0013, issue #21): immutable manifests, linked to a pipeline attempt so the
//! planned total is known before any job starts. Agent runs link to their shard through
//! `plan.assigned`.

use std::collections::BTreeSet;

use fleet_protocol::plan::PlanManifest;
use serde::Deserialize;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    Error, Store,
    auth::Publisher,
    ingest::{Change, EntityRef},
    sha256,
};

/// The CI run a plan belongs to, as the planning job knows it. Verified OIDC claims override it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, ToSchema)]
pub struct PlanPipeline {
    /// `github_actions` or `generic`.
    #[serde(default)]
    pub provider: Option<String>,
    /// `owner/name`.
    #[serde(default)]
    pub repository: Option<String>,
    /// Workflow run ID.
    #[serde(default)]
    pub run_id: Option<String>,
    /// Re-run attempt, from 1.
    #[serde(default)]
    pub run_attempt: Option<u32>,
}

/// A stored plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredPlan {
    /// False when the identical manifest was already stored (an idempotent retry).
    pub created: bool,
    /// The attempt it is linked to.
    pub pipeline_attempt_id: Option<Uuid>,
    /// The attempt's live change (its planned total moved).
    pub change: Option<Change>,
}

/// A plan was not stored.
#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    /// A different manifest already has this plan ID: plans are immutable.
    #[error("plan {0} already exists with different contents")]
    Conflict(String),
    /// The manifest is malformed.
    #[error("invalid plan: {0}")]
    Invalid(String),
    /// The database failed.
    #[error(transparent)]
    Store(#[from] Error),
}

impl From<sqlx::Error> for PlanError {
    fn from(e: sqlx::Error) -> Self {
        Self::Store(e.into())
    }
}

fn validate(m: &PlanManifest) -> Result<i32, PlanError> {
    let id_ok = (1..=128).contains(&m.plan_id.len())
        && m.plan_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._:-".contains(c));
    if !id_ok {
        return Err(PlanError::Invalid(
            "planId must be 1–128 of A-Z a-z 0-9 . _ : -".into(),
        ));
    }
    if m.shards.is_empty() {
        return Err(PlanError::Invalid("a plan needs at least one shard".into()));
    }
    let ids: BTreeSet<u32> = m.shards.iter().map(|s| s.id).collect();
    if ids.len() != m.shards.len() {
        return Err(PlanError::Invalid("shard ids must be unique".into()));
    }
    if m.shards.iter().flat_map(|s| &s.flows).any(String::is_empty) {
        return Err(PlanError::Invalid("flow paths must not be empty".into()));
    }
    let total = i32::try_from(m.planned_total())
        .map_err(|_| PlanError::Invalid("too many flows".into()))?;
    if m.total_flows
        .is_some_and(|t| i64::from(t) != i64::from(total))
    {
        return Err(PlanError::Invalid(format!(
            "totalFlows says {:?} but the shards hold {total}",
            m.total_flows
        )));
    }
    Ok(total)
}

impl Store {
    /// Stores a plan once. Retrying with the identical manifest is a no-op.
    #[expect(
        clippy::too_many_lines,
        reason = "one straight-line transaction; the attempt's notice adds two lines"
    )]
    pub async fn store_plan(
        &self,
        publisher: &Publisher,
        manifest: &PlanManifest,
        pipeline: &PlanPipeline,
    ) -> Result<StoredPlan, PlanError> {
        let total = validate(manifest)?;
        let json = serde_json::to_value(manifest).map_err(|e| PlanError::Invalid(e.to_string()))?;
        let digest = sha256(&json.to_string());
        let claims = publisher.claims.clone().unwrap_or_default();
        let repository = claims.repository.or_else(|| pipeline.repository.clone());
        let run_id = claims.run_id.or_else(|| pipeline.run_id.clone());
        let provider = if publisher.claims.is_some() {
            "github_actions".to_owned()
        } else {
            pipeline
                .provider
                .clone()
                .unwrap_or_else(|| "generic".into())
        };
        let attempt = i32::try_from(
            claims
                .run_attempt
                .or(pipeline.run_attempt)
                .unwrap_or(1)
                .max(1),
        )
        .unwrap_or(i32::MAX);
        let project = publisher.project_id;

        let mut tx = self.pool().begin().await?;
        let attempt_id = match &run_id {
            Some(run) => Some(
                upsert_attempt(
                    &mut tx,
                    project,
                    &provider,
                    repository.as_deref(),
                    run,
                    attempt,
                )
                .await?,
            ),
            None => None,
        };
        let inserted = sqlx::query_scalar!(
            "INSERT INTO plans (project_id, plan_id, pipeline_attempt_id, manifest, manifest_sha256,
                                planned_total)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (project_id, plan_id) DO NOTHING
             RETURNING plan_id",
            project,
            manifest.plan_id,
            attempt_id,
            json,
            digest,
            total
        )
        .fetch_optional(&mut *tx)
        .await?;
        if inserted.is_none() {
            let same = sqlx::query_scalar!(
                "SELECT manifest_sha256 = $3 AS \"same!\" FROM plans
                  WHERE project_id = $1 AND plan_id = $2",
                project,
                manifest.plan_id,
                digest
            )
            .fetch_one(&mut *tx)
            .await?;
            if !same {
                return Err(PlanError::Conflict(manifest.plan_id.clone()));
            }
            tx.commit().await?;
            return Ok(StoredPlan {
                created: false,
                pipeline_attempt_id: attempt_id,
                change: None,
            });
        }
        insert_shards(&mut tx, project, manifest).await?;
        // The plan's flows are known before any of them runs (issue #85).
        let assigned: Vec<&str> = manifest
            .shards
            .iter()
            .flat_map(|s| s.flows.iter().map(String::as_str))
            .collect();
        crate::catalog::upsert_plan_flows(&mut tx, project, &assigned).await?;
        let change = match attempt_id {
            Some(a) => {
                let version = sqlx::query_scalar!(
                    "UPDATE pipeline_attempts SET version = version + 1 WHERE id = $1
                     RETURNING version",
                    a
                )
                .fetch_one(&mut *tx)
                .await?;
                let change = Change {
                    project_id: project,
                    pipeline_attempt_id: a,
                    version: u64::try_from(version).unwrap_or(0),
                    entities: Vec::<EntityRef>::new(),
                };
                crate::live::notify(&mut tx, self.origin(), &change).await?;
                Some(change)
            }
            None => None,
        };
        tx.commit().await?;
        Ok(StoredPlan {
            created: true,
            pipeline_attempt_id: attempt_id,
            change,
        })
    }
}

async fn insert_shards(
    tx: &mut sqlx::PgConnection,
    project: Uuid,
    manifest: &PlanManifest,
) -> Result<(), sqlx::Error> {
    for s in &manifest.shards {
        sqlx::query!(
            "INSERT INTO plan_shards (project_id, plan_id, shard_id, flows, estimate_ms)
             VALUES ($1, $2, $3, $4, $5)",
            project,
            manifest.plan_id,
            i32::try_from(s.id).unwrap_or(i32::MAX),
            &s.flows,
            s.estimated_seconds
                .and_then(|secs| i64::try_from(secs.saturating_mul(1000)).ok())
        )
        .execute(&mut *tx)
        .await?;
    }
    Ok(())
}

/// The pipeline run and attempt a plan belongs to, created if no agent has connected yet.
async fn upsert_attempt(
    tx: &mut sqlx::PgConnection,
    project: Uuid,
    provider: &str,
    repository: Option<&str>,
    run: &str,
    attempt: i32,
) -> Result<Uuid, sqlx::Error> {
    let run_key = crate::ingest::run_key(repository, run);
    let run_row = sqlx::query_scalar!(
        "INSERT INTO pipeline_runs (project_id, provider, provider_run_key, repository)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (project_id, provider, provider_run_key) DO UPDATE
           SET repository = coalesce(pipeline_runs.repository, EXCLUDED.repository)
         RETURNING id",
        project,
        provider,
        run_key,
        repository
    )
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query_scalar!(
        "INSERT INTO pipeline_attempts (project_id, pipeline_run_id, attempt)
         VALUES ($1, $2, $3)
         ON CONFLICT (pipeline_run_id, attempt) DO UPDATE SET attempt = EXCLUDED.attempt
         RETURNING id",
        project,
        run_row,
        attempt
    )
    .fetch_one(&mut *tx)
    .await
}

#[cfg(test)]
mod tests {
    use fleet_protocol::{
        event::{
            EventBody, FlowEnded, FlowOutcome, FlowRef, FlowStarted, ShardAssigned, ShardMode,
            SuiteStarted,
        },
        journal::{ClockSample, EventEnvelope, Provenance, SourceKind, Subject},
        plan::PlanShard,
    };
    use sqlx::PgPool;
    use time::OffsetDateTime;

    use super::*;
    use crate::ingest::{
        Transport,
        tests::{Agent, SuiteIds, publisher},
    };

    fn manifest(id: &str, flows: &[&[&str]]) -> PlanManifest {
        PlanManifest {
            config_path: None,
            config_sha256: None,
            equal_count_estimated_seconds: None,
            plan_id: id.into(),
            total_flows: None,
            shards: flows
                .iter()
                .enumerate()
                .map(|(i, f)| PlanShard {
                    id: u32::try_from(i).unwrap(),
                    estimated_seconds: Some(30),
                    flows: f.iter().map(|s| (*s).to_owned()).collect(),
                })
                .collect(),
        }
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn planned_totals_exist_before_any_job_and_plans_are_immutable(pool: PgPool) {
        let store = Store {
            pool,
            origin: uuid::Uuid::now_v7(),
        };
        let p = publisher(&store).await;
        let m = manifest("plan-1", &[&["a.yaml", "b.yaml"], &["c.yaml"]]);
        let pipeline = PlanPipeline {
            repository: Some("o/r".into()),
            run_id: Some("500".into()),
            ..PlanPipeline::default()
        };
        let stored = store.store_plan(&p, &m, &pipeline).await.unwrap();
        assert!(stored.created && stored.change.is_some());
        let attempt = stored.pipeline_attempt_id.unwrap();
        let snap = store
            .snapshot(p.project_id, attempt)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            snap.attempt.planned_total,
            Some(3),
            "before any agent connected"
        );
        assert_eq!(snap.jobs, vec![]);
        assert_eq!(snap.plans[0].shards.len(), 2);

        assert!(
            !store.store_plan(&p, &m, &pipeline).await.unwrap().created,
            "idempotent"
        );
        let changed = manifest("plan-1", &[&["a.yaml"]]);
        assert!(matches!(
            store.store_plan(&p, &changed, &pipeline).await,
            Err(PlanError::Conflict(_))
        ));
        assert!(matches!(
            store
                .store_plan(&p, &manifest("bad id!", &[&["a"]]), &pipeline)
                .await,
            Err(PlanError::Invalid(_))
        ));

        // A shard's agent run joins the same attempt and links to its shard.
        let mut a = Agent::new("500", "shard-1");
        let h = store
            .open_run(&p, &a.hello(None), Transport::WebSocket)
            .await
            .unwrap()
            .handle;
        assert_eq!(h.pipeline_attempt_id, attempt);
        let e = a.event(
            fleet_protocol::journal::Subject::default(),
            fleet_protocol::journal::SourceKind::Declared,
            &fleet_protocol::event::EventBody::PlanAssigned(fleet_protocol::event::PlanAssigned {
                plan_id: "plan-1".into(),
                shard_id: 1,
                flows: vec!["c.yaml".into()],
            }),
        );
        store.apply_batch(&h, &[e]).await.unwrap();
        let snap = store
            .snapshot(p.project_id, attempt)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(snap.plans[0].shards[1].agent_runs, vec![a.id]);
        assert_eq!(snap.plans[0].shards[0].agent_runs, Vec::<Uuid>::new());
    }
    /// An event at a fixed wall-clock time, so shard spans are exact.
    fn at(
        a: &mut Agent,
        subject: Subject,
        body: &EventBody,
        wall: OffsetDateTime,
    ) -> EventEnvelope {
        a.seq += 1;
        EventEnvelope::new(
            a.seq,
            ClockSample { wall, mono_ms: 0 },
            subject,
            Provenance::new(SourceKind::LiveConsole),
            body,
        )
    }

    /// One shard's agent run: its suite, its device, the plan link, and each flow as
    /// `(name, start, end)` in seconds after `base`; an end of `None` leaves the flow running.
    fn shard_events(
        a: &mut Agent,
        ids: SuiteIds,
        shard: u32,
        serial: &str,
        base: OffsetDateTime,
        flows: &[(&str, i64, Option<i64>)],
    ) -> Vec<EventEnvelope> {
        let at_s = |s: i64| base + time::Duration::seconds(s);
        let mut out = vec![
            at(
                a,
                ids.suite(),
                &EventBody::SuiteStarted(SuiteStarted {
                    shard_mode: ShardMode::None,
                    shards: 1,
                }),
                base,
            ),
            at(
                a,
                ids.shard(),
                &EventBody::ShardAssigned(ShardAssigned {
                    shard_number: 1,
                    device_serial: serial.into(),
                    planned: flows
                        .iter()
                        .map(|(n, _, _)| FlowRef {
                            name: (*n).into(),
                            path: Some(format!("{n}.yaml")),
                        })
                        .collect(),
                }),
                base,
            ),
            at(
                a,
                Subject::default(),
                &EventBody::PlanAssigned(fleet_protocol::event::PlanAssigned {
                    plan_id: "plan-2".into(),
                    shard_id: shard,
                    flows: flows.iter().map(|(n, _, _)| format!("{n}.yaml")).collect(),
                }),
                base,
            ),
        ];
        for (ordinal, (name, start, end)) in flows.iter().enumerate() {
            let flow = Uuid::now_v7();
            out.push(at(
                a,
                ids.flow(flow),
                &EventBody::FlowStarted(FlowStarted {
                    flow: FlowRef {
                        name: (*name).into(),
                        path: Some(format!("{name}.yaml")),
                    },
                    shard_number: 1,
                    ordinal: u32::try_from(ordinal).unwrap(),
                }),
                at_s(*start),
            ));
            if let Some(end) = end {
                out.push(at(
                    a,
                    ids.flow(flow),
                    &EventBody::FlowEnded(FlowEnded {
                        outcome: FlowOutcome::Passed,
                        duration_ms: None,
                        failure: None,
                        flow: None,
                    }),
                    at_s(*end),
                ));
            }
        }
        out
    }

    /// Each shard's actual time is the span from its first flow's start to its last flow's end,
    /// and stays unknown while a flow it started is still running.
    #[sqlx::test(migrations = "../../migrations")]
    async fn each_shards_actual_time_is_the_span_of_its_flows(pool: PgPool) {
        let store = Store {
            pool,
            origin: uuid::Uuid::now_v7(),
        };
        let p = publisher(&store).await;
        let mut m = manifest("plan-2", &[&["a.yaml", "b.yaml"], &["c.yaml"]]);
        m.equal_count_estimated_seconds = Some(90);
        let pipeline = PlanPipeline {
            repository: Some("o/r".into()),
            run_id: Some("600".into()),
            ..PlanPipeline::default()
        };
        let attempt = store
            .store_plan(&p, &m, &pipeline)
            .await
            .unwrap()
            .pipeline_attempt_id
            .unwrap();
        let base = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();

        // Shard 0 finishes at +50 s; shard 1's only flow is still running.
        let mut a = Agent::new("600", "shard-0");
        let mut b = Agent::new("600", "shard-1");
        let (ids_a, ids_b) = (SuiteIds::new(), SuiteIds::new());
        let ha = store
            .open_run(&p, &a.hello(None), Transport::WebSocket)
            .await
            .unwrap()
            .handle;
        let hb = store
            .open_run(&p, &b.hello(None), Transport::WebSocket)
            .await
            .unwrap()
            .handle;
        let batch_a = shard_events(
            &mut a,
            ids_a,
            0,
            "emulator-5554",
            base,
            &[("a", 0, Some(30)), ("b", 30, Some(50))],
        );
        store.apply_batch(&ha, &batch_a).await.unwrap();
        let batch_b = shard_events(&mut b, ids_b, 1, "emulator-5556", base, &[("c", 5, None)]);
        store.apply_batch(&hb, &batch_b).await.unwrap();

        let snap = store
            .snapshot(p.project_id, attempt)
            .await
            .unwrap()
            .unwrap();
        let shards = &snap.plans[0].shards;
        assert_eq!(shards[0].actual_ms, Some(50_000), "+0 s to +50 s");
        assert_eq!(shards[1].actual_ms, None, "a flow is still running");
        assert_eq!(
            snap.plans[0].equal_count_estimate_ms,
            Some(90_000),
            "the manifest's equal-count baseline, in ms"
        );

        // Shard 1's flow ends at +95 s: its span is +5 s to +95 s.
        let c_end = {
            let flow = store
                .snapshot(p.project_id, attempt)
                .await
                .unwrap()
                .unwrap()
                .flows
                .iter()
                .find(|f| f.name == "c")
                .unwrap()
                .id;
            at(
                &mut b,
                ids_b.flow(flow),
                &EventBody::FlowEnded(FlowEnded {
                    outcome: FlowOutcome::Passed,
                    duration_ms: None,
                    failure: None,
                    flow: None,
                }),
                base + time::Duration::seconds(95),
            )
        };
        store.apply_batch(&hb, &[c_end]).await.unwrap();
        let snap = store
            .snapshot(p.project_id, attempt)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(snap.plans[0].shards[1].actual_ms, Some(90_000));
        assert_eq!(snap.plans[0].shards[0].actual_ms, Some(50_000));
    }
}

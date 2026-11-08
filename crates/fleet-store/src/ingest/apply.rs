// SPDX-License-Identifier: AGPL-3.0-only
//! Applying one journal event to the derived tables (research 05 §6, §9.3).
//!
//! Every write is an idempotent upsert keyed by an agent-generated ID, guarded by ownership:
//! `ON CONFLICT … DO UPDATE … WHERE owner = this run`, so a row owned by another run is never
//! touched and the event is rejected as `foreign_entity` instead (§3.11). Outcomes are never
//! written here: claims go to `flow_claims` and [`super::derive`] resolves them.

use std::collections::BTreeSet;

use fleet_core::claims::Claim;
use fleet_protocol::{
    event::{
        AdbState, AgentWarning, AppCrashed, DeviceStateObserved, EventBody, FlowEnded, FlowOutcome,
        FlowRef, FlowReported, JournalGap, MaestroBuild, PlanAssigned, ProcessExited, ReportParsed,
        RunEnded, RunSignal, RunStarted, ShardAssigned, StepEnded, StepStarted, StepTiming,
        SuiteStarted,
    },
    journal::{EventEnvelope, SourceKind},
};
use sqlx::PgConnection;
use time::OffsetDateTime;
use uuid::Uuid;

use super::{EntityRef, RunHandle, analytics, derive, to_i64, valid_agent_id};

/// What became of one event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Effect {
    /// Projected into the derived tables.
    Applied,
    /// Stored in the journal only: a type, version or event this build doesn't project.
    Unsupported,
    /// Refused for good: (stored code, detail).
    Rejected(&'static str, String),
}

impl Effect {
    pub(crate) fn status(&self) -> &'static str {
        match self {
            Self::Applied => "applied",
            Self::Unsupported => "unsupported",
            Self::Rejected(..) => "rejected",
        }
    }
}

fn reject(code: &'static str, message: impl Into<String>) -> Effect {
    Effect::Rejected(code, message.into())
}

/// Where a step sits in its flow's nesting (issue #38). `None` where the event doesn't say, which
/// keeps what an earlier event stored.
#[derive(Debug, Default)]
struct Nesting {
    depth: Option<i32>,
    parent: Option<i32>,
    attempt: Option<i32>,
}

/// The snake-case wire name of a serde enum value.
pub(crate) fn wire<T: serde::Serialize>(v: T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// The `agent.warning` codes a flow keeps for its page: the step alignment warnings (issue #148).
const STEP_WARNINGS: [&str; 2] = ["steps_not_aligned", "step_status_disagrees"];

/// The `agent.warning` code a run keeps for its own files: a commands file whose flow the log never
/// started (issue #148). It carries no flow, so it is kept on the agent run.
const RUN_FILE_WARNING: &str = "commands_flow_unnamed";

/// What one batch touched, so derivation and broadcasts cover exactly that.
#[derive(Debug, Clone)]
pub(crate) struct Ctx<'a> {
    run: &'a RunHandle,
    now: OffsetDateTime,
    /// Flows with new claims or a start: re-resolve.
    pub(crate) resolve: BTreeSet<Uuid>,
    /// Suites whose facts changed: re-derive.
    pub(crate) suites: BTreeSet<Uuid>,
    /// Suites that closed in this batch: their flows' finality may change.
    pub(crate) closed: BTreeSet<Uuid>,
    /// Devices whose busy/ready state may change.
    pub(crate) devices: BTreeSet<Uuid>,
    /// Entities whose stored row changed.
    pub(crate) changed: BTreeSet<EntityRef>,
}

impl<'a> Ctx<'a> {
    pub(crate) fn new(run: &'a RunHandle, now: OffsetDateTime) -> Self {
        Self {
            run,
            now,
            resolve: BTreeSet::new(),
            suites: BTreeSet::new(),
            closed: BTreeSet::new(),
            devices: BTreeSet::new(),
            changed: BTreeSet::new(),
        }
    }

    /// Applies one event. A database error aborts the transaction; the caller retries the batch
    /// under savepoints when it was the event's fault.
    pub(crate) async fn apply(
        &mut self,
        tx: &mut PgConnection,
        e: &EventEnvelope,
    ) -> Result<Effect, sqlx::Error> {
        if matches!(
            e.src.kind,
            SourceKind::ServerInferred | SourceKind::Unrecognized
        ) {
            return Ok(reject(
                "invalid",
                format!("agents may not send {} evidence", wire(e.src.kind)),
            ));
        }
        let s = &e.subject;
        for id in [
            s.suite_execution_id,
            s.shard_id,
            s.device_session_id,
            s.flow_execution_id,
        ]
        .into_iter()
        .flatten()
        {
            if !valid_agent_id(id, self.now) {
                return Ok(reject("invalid", format!("{id} is not a current UUIDv7")));
            }
        }
        let body = match e.decode() {
            Ok(Some(body)) => body,
            Ok(None) => return Ok(Effect::Unsupported),
            Err(err) => return Ok(reject("invalid", format!("bad {} payload: {err}", e.kind))),
        };
        if matches!(
            body,
            EventBody::FlowStarted(_) | EventBody::FlowEnded(_) | EventBody::FlowReported(_)
        ) {
            for (id, query) in [
                (
                    s.device_session_id,
                    "SELECT agent_run_id FROM device_sessions WHERE id = $1",
                ),
                (s.shard_id, "SELECT agent_run_id FROM shards WHERE id = $1"),
            ] {
                if let Some(id) = id {
                    let owner = sqlx::query_scalar::<_, Uuid>(query)
                        .bind(id)
                        .fetch_optional(&mut *tx)
                        .await?;
                    if owner != Some(self.run.agent_run_id) {
                        return Ok(foreign(id));
                    }
                }
            }
        }
        match body {
            EventBody::RunStarted(b) => self.run_started(tx, &b).await,
            EventBody::SuiteStarted(b) => self.suite_started(tx, e, &b).await,
            EventBody::ShardAssigned(b) => self.shard_assigned(tx, e, &b).await,
            EventBody::FlowStarted(b) => {
                let flow = FlowRef {
                    name: b.flow.name.clone(),
                    path: b.flow.path.clone(),
                };
                self.upsert_flow(tx, e, &flow, Some(b.shard_number), Some(b.ordinal), true)
                    .await
            }
            EventBody::StepStarted(b) => self.step_started(tx, e, &b).await,
            EventBody::StepEnded(b) => self.step_ended(tx, e, &b).await,
            EventBody::FlowEnded(b) => self.flow_ended(tx, e, &b).await,
            EventBody::FlowReported(b) => self.flow_reported(tx, e, &b).await,
            EventBody::AppCrashed(b) => self.app_crashed(tx, e, &b).await,
            EventBody::ProcessExited(b) => self.process_exited(tx, e, &b).await,
            EventBody::ReportParsed(b) => self.report_parsed(tx, e, &b).await,
            EventBody::RunSignal(b) => self.run_signal(tx, &b).await,
            EventBody::RunFinalizing => self.run_finalizing(tx).await,
            EventBody::RunEnded(b) => self.run_ended(tx, e, &b).await,
            EventBody::PlanAssigned(b) => self.plan_assigned(tx, &b).await,
            EventBody::DeviceStateObserved(b) => self.device_state(tx, e, &b).await,
            EventBody::JournalGap(b) => self.journal_gap(tx, &b).await,
            EventBody::MaestroDetected(b) => self.maestro_detected(tx, &b).await,
            EventBody::AgentWarning(b) => self.agent_warning(tx, e, &b).await,
            // Kept in the journal; projected by later milestones (artifacts: M5).
            EventBody::ArtifactRegistered(_)
            | EventBody::ArtifactUploaded(_)
            | EventBody::ArtifactRejected(_)
            | EventBody::ParserUnrecognized(_) => Ok(Effect::Applied),
        }
    }

    /// Re-derives what this batch touched; returns every changed entity.
    pub(crate) async fn derive(
        mut self,
        tx: &mut PgConnection,
    ) -> Result<BTreeSet<EntityRef>, sqlx::Error> {
        if !self.closed.is_empty() {
            let closed: Vec<Uuid> = self.closed.iter().copied().collect();
            let flows = sqlx::query_scalar!(
                "SELECT id FROM flow_executions WHERE suite_execution_id = ANY($1)",
                &closed
            )
            .fetch_all(&mut *tx)
            .await?;
            self.resolve.extend(flows);
        }
        let resolve: Vec<Uuid> = self.resolve.iter().copied().collect();
        for id in derive::flows(tx, &resolve).await? {
            self.changed.insert(EntityRef::Flow(id));
        }
        // Whatever this batch touched may have become a final pass or failure.
        analytics::finalize_flows(tx, &resolve).await?;
        if !resolve.is_empty() {
            let suites = sqlx::query_scalar!(
                "SELECT DISTINCT suite_execution_id FROM flow_executions WHERE id = ANY($1)",
                &resolve
            )
            .fetch_all(&mut *tx)
            .await?;
            self.suites.extend(suites);
        }
        let suites: Vec<Uuid> = self.suites.iter().copied().collect();
        for id in derive::suites(tx, &suites).await? {
            self.changed.insert(EntityRef::Suite(id));
        }
        let devices: Vec<Uuid> = self.devices.iter().copied().collect();
        for id in derive::devices(tx, &devices).await? {
            self.changed.insert(EntityRef::Device(id));
        }
        if derive::job(tx, self.run.job_id).await? {
            self.changed.insert(EntityRef::Job(self.run.job_id));
        }
        Ok(self.changed)
    }

    fn run_changed(&mut self) {
        self.changed
            .insert(EntityRef::AgentRun(self.run.agent_run_id));
    }

    async fn run_started(
        &mut self,
        tx: &mut PgConnection,
        b: &RunStarted,
    ) -> Result<Effect, sqlx::Error> {
        sqlx::query!(
            "UPDATE agent_runs SET argv = $2, agent_version = $3, evidence = $4, rev = rev + 1
             WHERE id = $1",
            self.run.agent_run_id,
            serde_json::to_value(&b.argv).unwrap_or_default(),
            b.agent_version,
            b.evidence
                .as_ref()
                .and_then(|e| serde_json::to_value(e).ok())
        )
        .execute(&mut *tx)
        .await?;
        self.run_changed();
        Ok(Effect::Applied)
    }

    async fn suite_started(
        &mut self,
        tx: &mut PgConnection,
        e: &EventEnvelope,
        b: &SuiteStarted,
    ) -> Result<Effect, sqlx::Error> {
        let Some(suite) = e.subject.suite_execution_id else {
            return Ok(reject(
                "invalid",
                "suite.started needs a suite_execution_id",
            ));
        };
        let owned = sqlx::query_scalar!(
            "INSERT INTO suite_executions (id, project_id, agent_run_id, job_id, shard_mode,
                                           shard_count, started_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (id) DO UPDATE SET rev = suite_executions.rev
               WHERE suite_executions.agent_run_id = EXCLUDED.agent_run_id
             RETURNING id",
            suite,
            self.run.project_id,
            self.run.agent_run_id,
            self.run.job_id,
            wire(b.shard_mode),
            i32::try_from(b.shards).unwrap_or(i32::MAX),
            e.at.wall
        )
        .fetch_optional(&mut *tx)
        .await?;
        if owned.is_none() {
            return Ok(foreign(suite));
        }
        self.suites.insert(suite);
        self.changed.insert(EntityRef::Suite(suite));
        Ok(Effect::Applied)
    }

    async fn shard_assigned(
        &mut self,
        tx: &mut PgConnection,
        e: &EventEnvelope,
        b: &ShardAssigned,
    ) -> Result<Effect, sqlx::Error> {
        let (Some(suite), Some(shard)) = (e.subject.suite_execution_id, e.subject.shard_id) else {
            return Ok(reject(
                "invalid",
                "shard.assigned needs a suite_execution_id and a shard_id",
            ));
        };
        let device = e.subject.device_session_id;
        if let Some(device) = device {
            let owned = sqlx::query_scalar!(
                "INSERT INTO device_sessions (id, project_id, agent_run_id, serial, state,
                                              first_seen_at, last_seen_at)
                 VALUES ($1, $2, $3, $4, 'ready', $5, $5)
                 ON CONFLICT (id) DO UPDATE
                   SET serial = EXCLUDED.serial, last_seen_at = EXCLUDED.last_seen_at,
                       rev = device_sessions.rev + 1
                   WHERE device_sessions.agent_run_id = EXCLUDED.agent_run_id
                 RETURNING id",
                device,
                self.run.project_id,
                self.run.agent_run_id,
                b.device_serial,
                e.at.wall
            )
            .fetch_optional(&mut *tx)
            .await?;
            if owned.is_none() {
                return Ok(foreign(device));
            }
            self.changed.insert(EntityRef::Device(device));
        }
        let planned = serde_json::to_value(&b.planned).unwrap_or_default();
        let owned = sqlx::query_scalar!(
            "INSERT INTO shards (id, project_id, agent_run_id, suite_execution_id, shard_index,
                                 device_session_id, planned, planned_count)
             SELECT $1, s.project_id, s.agent_run_id, s.id, $3, $4, $5, $6
               FROM suite_executions s WHERE s.id = $2 AND s.agent_run_id = $7
             ON CONFLICT (id) DO UPDATE
               SET device_session_id = coalesce(EXCLUDED.device_session_id, shards.device_session_id),
                   planned = EXCLUDED.planned, planned_count = EXCLUDED.planned_count
               WHERE shards.agent_run_id = EXCLUDED.agent_run_id
             RETURNING id",
            shard,
            suite,
            i32::try_from(b.shard_number).unwrap_or(i32::MAX),
            device,
            planned,
            i32::try_from(b.planned.len()).unwrap_or(i32::MAX),
            self.run.agent_run_id
        )
        .fetch_optional(&mut *tx)
        .await?;
        if owned.is_none() {
            return Ok(foreign(shard));
        }
        // The plan is known live, so progress is determinate (research 08 §4).
        sqlx::query!(
            "UPDATE suite_executions
                SET planned_count = (SELECT sum(planned_count)::int FROM shards
                                      WHERE suite_execution_id = $1),
                    plan_source = $2,
                    lifecycle = CASE WHEN lifecycle = 'starting' THEN 'running' ELSE lifecycle END,
                    rev = rev + 1
              WHERE id = $1",
            suite,
            wire(e.src.kind)
        )
        .execute(&mut *tx)
        .await?;
        self.changed.insert(EntityRef::Suite(suite));
        Ok(Effect::Applied)
    }

    /// Creates or updates a flow execution. `started` marks a live `flow.started`; claims about
    /// a flow that never started create it without starting it.
    async fn upsert_flow(
        &mut self,
        tx: &mut PgConnection,
        e: &EventEnvelope,
        flow: &FlowRef,
        shard_number: Option<u32>,
        ordinal: Option<u32>,
        started: bool,
    ) -> Result<Effect, sqlx::Error> {
        let (Some(suite), Some(id)) = (e.subject.suite_execution_id, e.subject.flow_execution_id)
        else {
            return Ok(reject(
                "invalid",
                format!(
                    "{} needs a suite_execution_id and a flow_execution_id",
                    e.kind
                ),
            ));
        };
        let owned = sqlx::query_scalar!(
            "INSERT INTO flow_executions (id, project_id, agent_run_id, suite_execution_id,
                                          shard_id, device_session_id, flow_name, flow_path,
                                          shard_number, ordinal, started, state, started_at,
                                          pipeline_attempt_id, job_id)
             SELECT $1, s.project_id, s.agent_run_id, s.id, $3, $4, $5, $6, $7, $8, $9,
                    CASE WHEN $9 THEN 'running' ELSE 'pending' END,
                    CASE WHEN $9 THEN $10::timestamptz END, j.pipeline_attempt_id, s.job_id
               FROM suite_executions s JOIN jobs j ON j.id = s.job_id
              WHERE s.id = $2 AND s.agent_run_id = $11
             ON CONFLICT (id) DO UPDATE
               SET started = flow_executions.started OR EXCLUDED.started,
                   started_at = coalesce(flow_executions.started_at, EXCLUDED.started_at),
                   flow_name = CASE WHEN EXCLUDED.started THEN EXCLUDED.flow_name
                                    ELSE flow_executions.flow_name END,
                   flow_path = coalesce(flow_executions.flow_path, EXCLUDED.flow_path),
                   shard_id = coalesce(flow_executions.shard_id, EXCLUDED.shard_id),
                   device_session_id = coalesce(flow_executions.device_session_id,
                                                EXCLUDED.device_session_id),
                   shard_number = coalesce(flow_executions.shard_number, EXCLUDED.shard_number),
                   ordinal = coalesce(flow_executions.ordinal, EXCLUDED.ordinal),
                   rev = flow_executions.rev + 1
               WHERE flow_executions.agent_run_id = EXCLUDED.agent_run_id
             RETURNING id",
            id,
            suite,
            e.subject.shard_id,
            e.subject.device_session_id,
            flow.name,
            flow.path,
            shard_number.and_then(|n| i32::try_from(n).ok()),
            ordinal.and_then(|n| i32::try_from(n).ok()),
            started,
            e.at.wall,
            self.run.agent_run_id
        )
        .fetch_optional(&mut *tx)
        .await?;
        if owned.is_none() {
            return Ok(foreign(id));
        }
        if started {
            sqlx::query!(
                "UPDATE suite_executions SET lifecycle = 'running', rev = rev + 1
                  WHERE id = $1 AND lifecycle = 'starting'",
                suite
            )
            .execute(&mut *tx)
            .await?;
            self.suites.insert(suite);
        }
        self.resolve.insert(id);
        self.changed.insert(EntityRef::Flow(id));
        if let Some(d) = e.subject.device_session_id {
            self.devices.insert(d);
        }
        Ok(Effect::Applied)
    }

    async fn owned_flow(
        &mut self,
        tx: &mut PgConnection,
        e: &EventEnvelope,
    ) -> Result<Result<Uuid, Effect>, sqlx::Error> {
        let Some(id) = e.subject.flow_execution_id else {
            return Ok(Err(reject(
                "invalid",
                format!("{} needs a flow_execution_id", e.kind),
            )));
        };
        let owner =
            sqlx::query_scalar!("SELECT agent_run_id FROM flow_executions WHERE id = $1", id)
                .fetch_optional(&mut *tx)
                .await?;
        Ok(match owner {
            Some(o) if o == self.run.agent_run_id => Ok(id),
            Some(_) => Err(foreign(id)),
            None => Err(reject(
                "unknown_entity",
                format!("flow execution {id} was never declared"),
            )),
        })
    }

    /// Keeps the step alignment warnings a flow's final `commands.json` raised, so its page says how
    /// far its steps are verified (issue #148). The flow's verdict never depends on them (I3). A
    /// commands file whose flow was never named is kept on its agent run instead. Other warnings
    /// stay in the journal.
    async fn agent_warning(
        &mut self,
        tx: &mut PgConnection,
        e: &EventEnvelope,
        b: &AgentWarning,
    ) -> Result<Effect, sqlx::Error> {
        if b.code == RUN_FILE_WARNING && e.subject.flow_execution_id.is_none() {
            let rows = sqlx::query!(
                "UPDATE agent_runs
                    SET file_warnings = array_append(file_warnings, $2), rev = rev + 1
                  WHERE id = $1 AND NOT ($2 = ANY(file_warnings))",
                self.run.agent_run_id,
                b.code
            )
            .execute(&mut *tx)
            .await?
            .rows_affected();
            if rows > 0 {
                self.changed
                    .insert(EntityRef::AgentRun(self.run.agent_run_id));
            }
            return Ok(Effect::Applied);
        }
        if !STEP_WARNINGS.contains(&b.code.as_str()) || e.subject.flow_execution_id.is_none() {
            return Ok(Effect::Applied);
        }
        let flow = match self.owned_flow(tx, e).await? {
            Ok(f) => f,
            Err(effect) => return Ok(effect),
        };
        let rows = sqlx::query!(
            "UPDATE flow_executions
                SET step_warnings = array_append(step_warnings, $2),
                    rev = rev + 1
              WHERE id = $1 AND agent_run_id = $3 AND NOT ($2 = ANY(step_warnings))",
            flow,
            b.code,
            self.run.agent_run_id
        )
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if rows > 0 {
            self.changed.insert(EntityRef::Flow(flow));
        }
        Ok(Effect::Applied)
    }

    /// Records the app's crash on its flow. Evidence only (I3): no claim, and the flow's state,
    /// outcome and failure are Maestro's. The first report of a crash is kept.
    async fn app_crashed(
        &mut self,
        tx: &mut PgConnection,
        e: &EventEnvelope,
        b: &AppCrashed,
    ) -> Result<Effect, sqlx::Error> {
        let flow = match self.owned_flow(tx, e).await? {
            Ok(f) => f,
            Err(effect) => return Ok(effect),
        };
        sqlx::query!(
            "UPDATE flow_executions
                SET crash_package = coalesce(crash_package, $2),
                    crash_exception = coalesce(crash_exception, $3),
                    rev = rev + 1
              WHERE id = $1 AND agent_run_id = $4",
            flow,
            b.package,
            b.exception,
            self.run.agent_run_id
        )
        .execute(&mut *tx)
        .await?;
        self.changed.insert(EntityRef::Flow(flow));
        Ok(Effect::Applied)
    }

    async fn step_started(
        &mut self,
        tx: &mut PgConnection,
        e: &EventEnvelope,
        b: &StepStarted,
    ) -> Result<Effect, sqlx::Error> {
        // A top-level step has no depth or attempt, and an attempt counts from 1: zero is no value
        // an agent can mean.
        if b.depth == Some(0) || b.attempt == Some(0) {
            return Ok(reject(
                "invalid",
                "step.started has a zero depth or attempt",
            ));
        }
        let nesting = Nesting {
            depth: b.depth.map(|d| i32::try_from(d).unwrap_or(i32::MAX)),
            parent: b
                .parent
                .as_ref()
                .map(|p| i32::try_from(p.index).unwrap_or(i32::MAX)),
            attempt: b.attempt.map(|a| i32::try_from(a).unwrap_or(i32::MAX)),
        };
        self.step(tx, e, Some(&b.description), "running", nesting)
            .await
    }

    async fn step_ended(
        &mut self,
        tx: &mut PgConnection,
        e: &EventEnvelope,
        b: &StepEnded,
    ) -> Result<Effect, sqlx::Error> {
        if e.src.kind == SourceKind::ArtifactFile {
            return self.step_settled(tx, e, b).await;
        }
        self.step(tx, e, None, &wire(b.status), Nesting::default())
            .await
    }

    /// Settles a step a final `commands.json` covers (issue #148): the file's status, and its start
    /// and length when it gives them. Only a row the log already wrote is settled, so a file never
    /// creates a step, and a step the file does not cover keeps what the log said (I3).
    async fn step_settled(
        &mut self,
        tx: &mut PgConnection,
        e: &EventEnvelope,
        b: &StepEnded,
    ) -> Result<Effect, sqlx::Error> {
        let Some(step) = &e.subject.step else {
            return Ok(reject("invalid", format!("{} needs a step", e.kind)));
        };
        let flow = match self.owned_flow(tx, e).await? {
            Ok(f) => f,
            Err(effect) => return Ok(effect),
        };
        let span = b.timing.as_ref().and_then(settled_span);
        let (started, ended) = span.unzip();
        let rows = sqlx::query!(
            "UPDATE step_executions
                SET status = $4,
                    source = $5,
                    started_at = coalesce($6::timestamptz, started_at),
                    ended_at = coalesce($7::timestamptz, ended_at)
              WHERE run_day = $1 AND flow_execution_id = $2 AND step_index = $3",
            self.run.run_day,
            flow,
            i32::try_from(step.index).unwrap_or(i32::MAX),
            wire(b.status),
            wire(e.src.kind),
            started,
            ended
        )
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if rows > 0 {
            sqlx::query!(
                "UPDATE flow_executions SET rev = rev + 1 WHERE id = $1",
                flow
            )
            .execute(&mut *tx)
            .await?;
            self.changed.insert(EntityRef::Flow(flow));
        }
        Ok(Effect::Applied)
    }

    async fn step(
        &mut self,
        tx: &mut PgConnection,
        e: &EventEnvelope,
        label: Option<&str>,
        status: &str,
        nesting: Nesting,
    ) -> Result<Effect, sqlx::Error> {
        let Some(step) = &e.subject.step else {
            return Ok(reject("invalid", format!("{} needs a step", e.kind)));
        };
        let flow = match self.owned_flow(tx, e).await? {
            Ok(f) => f,
            Err(effect) => return Ok(effect),
        };
        let running = status == "running";
        sqlx::query!(
            "INSERT INTO step_executions (run_day, flow_execution_id, step_index, label, status,
                                          source, started_at, ended_at, depth, parent_step_index,
                                          attempt)
             VALUES ($1, $2, $3, $4, $5, $6,
                     CASE WHEN $7 THEN $8::timestamptz END, CASE WHEN NOT $7 THEN $8::timestamptz END,
                     $9, $10, $11)
             ON CONFLICT (run_day, flow_execution_id, step_index) DO UPDATE
               SET label = coalesce(EXCLUDED.label, step_executions.label),
                   status = CASE WHEN $7 OR step_executions.source = 'artifact_file'
                                 THEN step_executions.status ELSE EXCLUDED.status END,
                   source = CASE WHEN step_executions.source = 'artifact_file'
                                 THEN step_executions.source ELSE EXCLUDED.source END,
                   started_at = CASE WHEN step_executions.source = 'artifact_file'
                                     THEN step_executions.started_at
                                     ELSE coalesce(step_executions.started_at, EXCLUDED.started_at) END,
                   ended_at = CASE WHEN step_executions.source = 'artifact_file'
                                   THEN step_executions.ended_at
                                   ELSE coalesce(EXCLUDED.ended_at, step_executions.ended_at) END,
                   depth = coalesce(EXCLUDED.depth, step_executions.depth),
                   parent_step_index = coalesce(EXCLUDED.parent_step_index, step_executions.parent_step_index),
                   attempt = coalesce(EXCLUDED.attempt, step_executions.attempt)",
            self.run.run_day,
            flow,
            i32::try_from(step.index).unwrap_or(i32::MAX),
            label,
            status,
            wire(e.src.kind),
            running,
            e.at.wall,
            nesting.depth,
            nesting.parent,
            nesting.attempt
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query!(
            "UPDATE flow_executions SET rev = rev + 1 WHERE id = $1",
            flow
        )
        .execute(&mut *tx)
        .await?;
        self.changed.insert(EntityRef::Flow(flow));
        Ok(Effect::Applied)
    }

    async fn flow_ended(
        &mut self,
        tx: &mut PgConnection,
        e: &EventEnvelope,
        b: &FlowEnded,
    ) -> Result<Effect, sqlx::Error> {
        if let Err(refused) = Claim::new(e.src.kind, b.outcome, e.seq) {
            return Ok(reject("invalid", refused.to_string()));
        }
        let flow = match &b.flow {
            Some(f) => match self.upsert_flow(tx, e, f, None, None, false).await? {
                Effect::Applied => e.subject.flow_execution_id,
                other => return Ok(other),
            },
            None => match self.owned_flow(tx, e).await? {
                Ok(id) => Some(id),
                Err(effect) => return Ok(effect),
            },
        };
        let Some(flow) = flow else {
            return Ok(reject("invalid", "flow.ended needs a flow_execution_id"));
        };
        let message = b.failure.as_ref().map(|f| f.message.as_str());
        self.claim(tx, e, flow, b.outcome, message, b.duration_ms, None)
            .await
    }

    async fn flow_reported(
        &mut self,
        tx: &mut PgConnection,
        e: &EventEnvelope,
        b: &FlowReported,
    ) -> Result<Effect, sqlx::Error> {
        if let Err(refused) = Claim::new(e.src.kind, b.outcome, e.seq) {
            return Ok(reject("invalid", refused.to_string()));
        }
        let flow_ref = FlowRef {
            name: b.junit.name.clone(),
            path: b.junit.file.clone(),
        };
        let shard = b.testsuite_index.saturating_add(1);
        match self
            .upsert_flow(tx, e, &flow_ref, Some(shard), None, false)
            .await?
        {
            Effect::Applied => {}
            other => return Ok(other),
        }
        let Some(flow) = e.subject.flow_execution_id else {
            return Ok(reject("invalid", "flow.reported needs a flow_execution_id"));
        };
        let relative = |p: &&str| {
            !p.contains(['\\', ':'])
                && std::path::Path::new(p)
                    .components()
                    .all(|c| matches!(c, std::path::Component::Normal(_)))
                && !p.is_empty()
        };
        let identity = b
            .repo_path
            .as_deref()
            .filter(relative)
            .or_else(|| b.junit.file.as_deref().filter(relative));
        if let Some(file) = identity {
            let def = sqlx::query_scalar!(
                "INSERT INTO flow_definitions (project_id, repo_path, last_name) VALUES ($1, $2, $3)
                 ON CONFLICT (project_id, repo_path) DO UPDATE SET last_name = EXCLUDED.last_name
                 RETURNING id",
                self.run.project_id,
                file,
                b.junit.name
            )
            .fetch_one(&mut *tx)
            .await?;
            sqlx::query!(
                "UPDATE flow_executions SET flow_definition_id = $2 WHERE id = $1",
                flow,
                def
            )
            .execute(&mut *tx)
            .await?;
        }
        let detail = serde_json::json!({
            "testsuite_index": b.testsuite_index,
            "testcase_index": b.testcase_index,
            "matched": b.matched,
            "junit": b.junit,
        });
        self.claim(
            tx,
            e,
            flow,
            b.outcome,
            b.junit.failure.as_deref(),
            b.junit.time_ms,
            Some(detail),
        )
        .await
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "one claim: which flow, what, and the evidence behind it"
    )]
    async fn claim(
        &mut self,
        tx: &mut PgConnection,
        e: &EventEnvelope,
        flow: Uuid,
        outcome: FlowOutcome,
        message: Option<&str>,
        duration_ms: Option<u64>,
        detail: Option<serde_json::Value>,
    ) -> Result<Effect, sqlx::Error> {
        // Invariant I3: the lattice's rules decide who may claim what, before the database's
        // CHECK constraint gets a say.
        let claim = match Claim::new(e.src.kind, outcome, e.seq) {
            Ok(c) => c,
            Err(refused) => return Ok(reject("invalid", refused.to_string())),
        };
        let mut detail = detail.unwrap_or_else(|| serde_json::json!({}));
        if let (Some(parser), Some(obj)) = (&e.src.parser, detail.as_object_mut()) {
            obj.insert("parser".into(), parser.clone().into());
        }
        sqlx::query!(
            "INSERT INTO flow_claims (flow_execution_id, source, origin, outcome, claim_order,
                                      observed_at, detail)
             VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT DO NOTHING",
            flow,
            wire(claim.source()),
            format!("{}:{}", self.run.agent_run_id, e.seq),
            wire(claim.outcome()),
            to_i64(e.seq),
            e.at.wall,
            detail
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query!(
            "UPDATE flow_executions
                SET failure_message = coalesce(failure_message, $2),
                    duration_ms = coalesce(duration_ms, $3),
                    ended_at = coalesce(ended_at, $4),
                    rev = rev + 1
              WHERE id = $1",
            flow,
            message,
            duration_ms.map(to_i64),
            e.at.wall
        )
        .execute(&mut *tx)
        .await?;
        self.resolve.insert(flow);
        self.changed.insert(EntityRef::Flow(flow));
        if let Some(d) = e.subject.device_session_id {
            self.devices.insert(d);
        }
        Ok(Effect::Applied)
    }

    fn owned_suite_update(&mut self, e: &EventEnvelope, updated: Option<Uuid>) -> Effect {
        match (e.subject.suite_execution_id, updated) {
            (Some(s), Some(_)) => {
                self.suites.insert(s);
                self.changed.insert(EntityRef::Suite(s));
                Effect::Applied
            }
            (Some(s), None) => reject(
                "unknown_entity",
                format!("suite {s} is unknown or not owned by this run"),
            ),
            (None, _) => reject("invalid", format!("{} needs a suite_execution_id", e.kind)),
        }
    }

    async fn process_exited(
        &mut self,
        tx: &mut PgConnection,
        e: &EventEnvelope,
        b: &ProcessExited,
    ) -> Result<Effect, sqlx::Error> {
        let updated = sqlx::query_scalar!(
            "UPDATE suite_executions
                SET exit_code = $3, exit_signal = $4, exit_class = $5, ended_at = $6,
                    lifecycle = CASE WHEN lifecycle = 'closed' THEN lifecycle ELSE 'reporting' END,
                    rev = rev + 1
              WHERE id = $1 AND agent_run_id = $2
              RETURNING id",
            e.subject.suite_execution_id,
            self.run.agent_run_id,
            b.code,
            b.signal,
            wire(b.class),
            e.at.wall
        )
        .fetch_optional(&mut *tx)
        .await?;
        let effect = self.owned_suite_update(e, updated);
        if effect != Effect::Applied {
            return Ok(effect);
        }
        sqlx::query!(
            "UPDATE agent_runs SET maestro_exit_code = $2, maestro_exit_signal = $3, rev = rev + 1
              WHERE id = $1",
            self.run.agent_run_id,
            b.code,
            b.signal
        )
        .execute(&mut *tx)
        .await?;
        self.run_changed();
        Ok(effect)
    }

    async fn report_parsed(
        &mut self,
        tx: &mut PgConnection,
        e: &EventEnvelope,
        b: &ReportParsed,
    ) -> Result<Effect, sqlx::Error> {
        let updated = sqlx::query_scalar!(
            "UPDATE suite_executions
                SET report_status = $3, lifecycle = 'closed', ended_at = coalesce(ended_at, $4),
                    rev = rev + 1
              WHERE id = $1 AND agent_run_id = $2
              RETURNING id",
            e.subject.suite_execution_id,
            self.run.agent_run_id,
            wire(b.status),
            e.at.wall
        )
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(s) = updated {
            self.closed.insert(s);
        }
        Ok(self.owned_suite_update(e, updated))
    }

    async fn run_signal(
        &mut self,
        tx: &mut PgConnection,
        b: &RunSignal,
    ) -> Result<Effect, sqlx::Error> {
        sqlx::query!(
            "UPDATE agent_runs SET signal_received = $2, rev = rev + 1 WHERE id = $1",
            self.run.agent_run_id,
            b.signal
        )
        .execute(&mut *tx)
        .await?;
        self.run_changed();
        Ok(Effect::Applied)
    }

    async fn run_finalizing(&mut self, tx: &mut PgConnection) -> Result<Effect, sqlx::Error> {
        sqlx::query!(
            "UPDATE agent_runs SET state = 'finalizing', rev = rev + 1
              WHERE id = $1 AND state IN ('active', 'detached', 'abandoned')",
            self.run.agent_run_id
        )
        .execute(&mut *tx)
        .await?;
        self.run_changed();
        Ok(Effect::Applied)
    }

    async fn run_ended(
        &mut self,
        tx: &mut PgConnection,
        e: &EventEnvelope,
        b: &RunEnded,
    ) -> Result<Effect, sqlx::Error> {
        sqlx::query!(
            "UPDATE agent_runs
                SET state = 'ended', end_seq = $2, ended_at = now(),
                    end_reason = CASE WHEN $3 THEN 'interrupted' ELSE 'normal' END,
                    rev = rev + 1
              WHERE id = $1",
            self.run.agent_run_id,
            to_i64(e.seq),
            b.interrupted
        )
        .execute(&mut *tx)
        .await?;
        let released = sqlx::query_scalar!(
            "UPDATE device_sessions SET state = 'released', rev = rev + 1
              WHERE agent_run_id = $1 AND state <> 'released'
              RETURNING id",
            self.run.agent_run_id
        )
        .fetch_all(&mut *tx)
        .await?;
        self.changed
            .extend(released.into_iter().map(EntityRef::Device));
        self.run_changed();
        Ok(Effect::Applied)
    }

    /// Records the Maestro build, and re-derives the run's flows: their condition fingerprint
    /// reads the version, and it may arrive after the flows have ended.
    async fn maestro_detected(
        &mut self,
        tx: &mut PgConnection,
        b: &MaestroBuild,
    ) -> Result<Effect, sqlx::Error> {
        sqlx::query!(
            "UPDATE agent_runs SET maestro_version = $2, maestro_profile_supported = $3, rev = rev + 1
             WHERE id = $1",
            self.run.agent_run_id,
            b.version,
            b.profile_supported
        )
        .execute(&mut *tx)
        .await?;
        let flows = sqlx::query_scalar!(
            "SELECT id FROM flow_executions WHERE agent_run_id = $1",
            self.run.agent_run_id
        )
        .fetch_all(&mut *tx)
        .await?;
        self.resolve.extend(flows);
        self.run_changed();
        Ok(Effect::Applied)
    }

    async fn plan_assigned(
        &mut self,
        tx: &mut PgConnection,
        b: &PlanAssigned,
    ) -> Result<Effect, sqlx::Error> {
        sqlx::query!(
            "UPDATE agent_runs SET plan_id = $2, plan_shard_id = $3, rev = rev + 1 WHERE id = $1",
            self.run.agent_run_id,
            b.plan_id,
            i32::try_from(b.shard_id).unwrap_or(i32::MAX)
        )
        .execute(&mut *tx)
        .await?;
        self.run_changed();
        Ok(Effect::Applied)
    }

    async fn device_state(
        &mut self,
        tx: &mut PgConnection,
        e: &EventEnvelope,
        b: &DeviceStateObserved,
    ) -> Result<Effect, sqlx::Error> {
        let offline = matches!(b.state, AdbState::Offline | AdbState::Absent);
        let online = b.state == AdbState::Device;
        let updated = sqlx::query_scalar!(
            "UPDATE device_sessions
                SET state = CASE WHEN state IN ('released', 'lost') THEN state
                                 WHEN $3 THEN 'offline'
                                 WHEN $4 AND state IN ('offline', 'discovered', 'booting') THEN 'ready'
                                 ELSE state END,
                    last_seen_at = $5, rev = rev + 1
              WHERE agent_run_id = $1 AND (id = $2 OR ($2::uuid IS NULL AND serial = $6))
              RETURNING id",
            self.run.agent_run_id,
            e.subject.device_session_id,
            offline,
            online,
            e.at.wall,
            b.serial
        )
        .fetch_all(&mut *tx)
        .await?;
        self.changed
            .extend(updated.into_iter().map(EntityRef::Device));
        Ok(Effect::Applied)
    }

    async fn journal_gap(
        &mut self,
        tx: &mut PgConnection,
        b: &JournalGap,
    ) -> Result<Effect, sqlx::Error> {
        if b.from_seq > b.to_seq || b.from_seq == 0 {
            return Ok(reject(
                "invalid",
                format!("bad gap {}..{}", b.from_seq, b.to_seq),
            ));
        }
        sqlx::query!(
            "INSERT INTO ingest_gaps (agent_run_id, stream, from_seq, to_seq, reason)
             VALUES ($1, 'journal', $2, $3, $4) ON CONFLICT DO NOTHING",
            self.run.agent_run_id,
            to_i64(b.from_seq),
            to_i64(b.to_seq),
            format!("journal.gap: {}", b.reason)
        )
        .execute(&mut *tx)
        .await?;
        self.run_changed();
        Ok(Effect::Applied)
    }
}

fn foreign(id: Uuid) -> Effect {
    reject(
        "foreign_entity",
        format!("{id} belongs to another agent run"),
    )
}

/// A settled step's start and end, from the file's start and length. `None` when the times do not
/// fit a timestamp.
fn settled_span(t: &StepTiming) -> Option<(OffsetDateTime, OffsetDateTime)> {
    let started =
        OffsetDateTime::from_unix_timestamp_nanos(i128::from(t.started_ms) * 1_000_000).ok()?;
    let length = time::Duration::milliseconds(i64::try_from(t.duration_ms).ok()?);
    Some((started, started.checked_add(length)?))
}

// SPDX-License-Identifier: AGPL-3.0-only
//! Agent runs and the ingest transaction (research 05 §3.4–§3.7, §9.3, §9.4).
//!
//! [`Store::open_run`] handles `hello`: it upserts the CI hierarchy by natural key, inserts or
//! resumes the agent run, applies the resume rules and bumps the fencing epoch.
//! [`Store::apply_batch`] handles one `events` frame in one transaction:
//!
//! 1. lock the run row and fence the epoch;
//! 2. skip duplicates (`seq ≤ committed_seq`) and require `committed_seq + 1` next, or a
//!    `journal.gap` that covers the hole exactly;
//! 3. apply each event in `seq` order with ownership checks on every agent-generated ID;
//! 4. re-derive flow outcomes from their claims (fleet-core's lattice), then suites and the job;
//! 5. write the raw journal, bump revisions and the attempt's live version;
//! 6. advance the cursor, guarded by `epoch` and the previous cursor.
//!
//! The caller acks only after this returns. An event that breaks a constraint is rejected on its
//! own (the batch is retried event by event under savepoints), so a bad event never wedges
//! ingestion.

mod analytics;
mod apply;
pub(crate) mod derive;
#[cfg(test)]
pub(crate) mod tests;

use std::{borrow::Cow, collections::BTreeSet, sync::LazyLock};

use fleet_core::redact::Redactor;
use fleet_protocol::{
    event::EventBody,
    frame::{CiProvider, ErrorCode, Hello, ResumeOutcome},
    journal::EventEnvelope,
};
use serde::{Deserialize, Serialize};
use sqlx::{Connection as _, PgConnection};
use time::{Date, OffsetDateTime};
use uuid::Uuid;

use crate::{
    Error, Result, Store,
    auth::{Credential, Publisher},
};

/// How an agent reached the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// The `fleet.v1+json` WebSocket.
    WebSocket,
    /// The HTTPS batch fallback.
    Batch,
}

/// An accepted agent run on one connection: what every later call needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunHandle {
    /// The agent run.
    pub agent_run_id: Uuid,
    /// Its project.
    pub project_id: Uuid,
    /// Its job.
    pub job_id: Uuid,
    /// The job's pipeline attempt (the live-view scope).
    pub pipeline_attempt_id: Uuid,
    /// The worker it runs on.
    pub worker_id: Uuid,
    /// Partition key of its journal.
    pub run_day: Date,
    /// This connection's fencing epoch.
    pub epoch: u32,
    /// This connection.
    pub connection_id: Uuid,
}

/// The answer to `hello`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opened {
    /// The accepted run.
    pub handle: RunHandle,
    /// Highest committed `seq`: the agent drops everything up to it and resends the rest.
    pub committed_through: u64,
    /// New, resumed or reopened.
    pub resume: ResumeOutcome,
    /// What changed, for the live hub.
    pub change: Option<Change>,
}

/// `hello` was refused.
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    /// The server refuses the run; `fatal` means don't retry with the same run or credential.
    #[error("{code:?}: {message}")]
    Refused {
        /// Why.
        code: ErrorCode,
        /// Detail.
        message: String,
        /// Don't retry.
        fatal: bool,
    },
    /// The database failed.
    #[error(transparent)]
    Store(#[from] Error),
}

impl From<sqlx::Error> for OpenError {
    fn from(e: sqlx::Error) -> Self {
        Self::Store(e.into())
    }
}

/// One event refused for good.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    /// Which event.
    pub seq: u64,
    /// Why, on the wire.
    pub code: ErrorCode,
    /// Detail, starting with the stored reason (`foreign_entity: …`).
    pub message: String,
}

/// A committed batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    /// Everything up to here is committed (the cumulative ack).
    pub through: u64,
    /// Events already committed before this batch.
    pub duplicates: u32,
    /// Events refused for good; the cursor moved past them.
    pub rejected: Vec<Rejection>,
    /// Set when the batch skipped a `seq` after `through`: the agent resends from here.
    pub expected: Option<u64>,
    /// What changed, for the live hub.
    pub change: Option<Change>,
}

/// A batch was not applied.
#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    /// A newer connection owns the run (research 05 §3.7).
    #[error("superseded by a newer connection")]
    Superseded,
    /// The batch doesn't start at `committed_seq + 1`.
    #[error("sequence gap: expected {expected}")]
    SeqGap {
        /// The `seq` the server expects next.
        expected: u64,
    },
    /// The database failed.
    #[error(transparent)]
    Store(#[from] Error),
}

impl From<sqlx::Error> for ApplyError {
    fn from(e: sqlx::Error) -> Self {
        Self::Store(e.into())
    }
}

/// A committed change to one pipeline attempt's live state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// The project.
    pub project_id: Uuid,
    /// The attempt whose version moved.
    pub pipeline_attempt_id: Uuid,
    /// The attempt's new live version.
    pub version: u64,
    /// The entities that changed.
    pub entities: Vec<EntityRef>,
}

/// An entity whose stored state changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum EntityRef {
    /// A job.
    Job(Uuid),
    /// An agent run.
    AgentRun(Uuid),
    /// A suite execution.
    Suite(Uuid),
    /// A flow execution.
    Flow(Uuid),
    /// A device session.
    Device(Uuid),
}

/// The CI identity a `hello` resolves to; verified claims win over agent-reported values.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Identity {
    provider: &'static str,
    run_key: String,
    repository: Option<String>,
    sha: Option<String>,
    git_ref: Option<String>,
    attempt: i32,
    job_key: String,
    job_name: Option<String>,
}

fn identity(p: &Publisher, h: &Hello) -> Identity {
    let ci = &h.ci;
    let claims = p.claims.clone().unwrap_or_default();
    let repository = claims.repository.or_else(|| ci.repository.clone());
    let run_id = claims.run_id.or_else(|| ci.run_id.clone());
    let attempt = claims.run_attempt.or(ci.run_attempt).unwrap_or(1).max(1);
    let job_key = claims.check_run_id.unwrap_or_else(|| ci.job_key.clone());
    let run_key = match &run_id {
        Some(id) => run_key(repository.as_deref(), id),
        // No CI run to group by: the agent run is its own pipeline.
        None => format!("agent-run:{}", h.agent_run_id),
    };
    Identity {
        provider: match ci.provider {
            CiProvider::GithubActions => "github_actions",
            CiProvider::Generic => "generic",
        },
        run_key,
        repository,
        sha: claims.sha.or_else(|| ci.sha.clone()),
        git_ref: claims.git_ref.or_else(|| ci.git_ref.clone()),
        attempt: i32::try_from(attempt).unwrap_or(i32::MAX),
        job_key,
        job_name: ci.job_name.clone(),
    }
}

/// The natural key of a CI run within a provider (research 05 §3.1).
pub(crate) fn run_key(repository: Option<&str>, run_id: &str) -> String {
    format!("{}:{run_id}", repository.unwrap_or("-"))
}

/// Agent-generated IDs must be UUIDv7 within ±7 days of the server clock (research 05 §5.5).
pub(crate) fn valid_agent_id(id: Uuid, now: OffsetDateTime) -> bool {
    let Some(ts) = id.get_timestamp() else {
        return false;
    };
    if id.get_version_num() != 7 {
        return false;
    }
    let (secs, _) = ts.to_unix();
    let delta = i64::try_from(secs)
        .unwrap_or(i64::MAX)
        .saturating_sub(now.unix_timestamp());
    delta.abs() <= 7 * 24 * 3600
}

fn refused(code: ErrorCode, message: impl Into<String>, fatal: bool) -> OpenError {
    OpenError::Refused {
        code,
        message: message.into(),
        fatal,
    }
}

fn to_u64(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

fn to_i64(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

impl Store {
    /// Accepts or refuses a `hello` (research 05 §3.4 resume rules, §3.7 fencing).
    pub async fn open_run(
        &self,
        publisher: &Publisher,
        hello: &Hello,
        transport: Transport,
    ) -> Result<Opened, OpenError> {
        let now = OffsetDateTime::now_utc();
        if !valid_agent_id(hello.agent_run_id, now) {
            return Err(refused(
                ErrorCode::Invalid,
                "agent_run_id must be a current UUIDv7",
                true,
            ));
        }
        let id = identity(publisher, hello);
        if let Some(claims) = &publisher.claims
            && claims.check_run_id.is_some()
            && claims.check_run_id.as_deref() != Some(hello.ci.job_key.as_str())
        {
            tracing::debug!(
                agent_run_id = %hello.agent_run_id,
                "agent-reported job key differs from the verified check_run_id; using the claim"
            );
        }
        let mut tx = self.pool().begin().await?;
        let existing = sqlx::query!(
            "SELECT project_id, job_id, worker_id, run_day, state, epoch, committed_seq
               FROM agent_runs WHERE id = $1 FOR UPDATE",
            hello.agent_run_id
        )
        .fetch_optional(&mut *tx)
        .await?;

        let next_seq = hello.resume.next_seq.max(1);
        let oldest = hello.resume.oldest_unacked_seq.unwrap_or(next_seq);
        let (job_id, worker_id, run_day, committed, resume, new_run) = if let Some(run) = existing {
            let committed = to_u64(run.committed_seq);
            check_resume(publisher, run.project_id, committed, next_seq)?;
            let resume = if run.state == "abandoned" {
                ResumeOutcome::Reopened
            } else {
                ResumeOutcome::Resumed
            };
            let worker = run.worker_id.ok_or_else(|| {
                OpenError::Store(Error::Corrupt("agent run without a worker".into()))
            })?;
            (run.job_id, worker, run.run_day, committed, resume, false)
        } else {
            let (job_id, worker_id) = upsert_hierarchy(&mut tx, publisher, hello, &id).await?;
            let run_day = run_day_of(hello, now);
            insert_run(&mut tx, publisher, hello, job_id, worker_id, run_day).await?;
            (job_id, worker_id, run_day, 0, ResumeOutcome::New, true)
        };

        // The agent no longer holds what the server never committed: record the loss explicitly
        // and move the cursor on, so a gap never wedges ingestion (I5).
        let mut committed = committed;
        if committed + 1 < oldest {
            let reason = if new_run {
                "unknown_run_resumed"
            } else {
                "agent_discarded"
            };
            sqlx::query!(
                "INSERT INTO ingest_gaps (agent_run_id, stream, from_seq, to_seq, reason)
                 VALUES ($1, 'journal', $2, $3, $4) ON CONFLICT DO NOTHING",
                hello.agent_run_id,
                to_i64(committed + 1),
                to_i64(oldest - 1),
                reason
            )
            .execute(&mut *tx)
            .await?;
            committed = oldest - 1;
        }

        let (epoch, connection_id, attempt_id) = fence(
            &mut tx,
            hello.agent_run_id,
            committed,
            transport,
            worker_id,
            job_id,
        )
        .await?;

        let mut entities = BTreeSet::from([EntityRef::AgentRun(hello.agent_run_id)]);
        if derive::job(&mut tx, job_id).await? {
            entities.insert(EntityRef::Job(job_id));
        }
        let change = self
            .bump_version(&mut tx, publisher.project_id, attempt_id, entities)
            .await?;
        tx.commit().await?;
        Ok(Opened {
            handle: RunHandle {
                agent_run_id: hello.agent_run_id,
                project_id: publisher.project_id,
                job_id,
                pipeline_attempt_id: attempt_id,
                worker_id,
                run_day,
                epoch: u32::try_from(epoch).unwrap_or(0),
                connection_id,
            },
            committed_through: committed,
            resume,
            change,
        })
    }

    /// The server is restarting: the connection ends but the run stays `active` (it shows
    /// `stale` until it reconnects, research 03 §5.4); the leader detaches it if it never does.
    pub async fn connection_closed(&self, run: &RunHandle, reason: &str) -> Result<()> {
        sqlx::query!(
            "UPDATE connections SET closed_at = now(), close_reason = $2
              WHERE id = $1 AND closed_at IS NULL",
            run.connection_id,
            reason
        )
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// The connection closed without `bye` after `run.ended`: the run is detached until it
    /// resumes or its grace expires. A newer connection's state is left alone.
    pub async fn detach(&self, run: &RunHandle, reason: &str) -> Result<()> {
        let mut tx = self.pool().begin().await?;
        detach_run(&mut tx, run, reason).await?;
        tx.commit().await?;
        Ok(())
    }

    /// [`Self::detach`] for a socket that closed on this instance. When it detaches an active
    /// run, every instance's view of that run's liveness changes: the attempt's version is
    /// bumped and the change returned, for [`crate::Store`]'s caller to publish. Otherwise
    /// nothing changed and nothing is returned.
    pub async fn detach_closed_socket(
        &self,
        run: &RunHandle,
        reason: &str,
    ) -> Result<Option<Change>> {
        let mut tx = self.pool().begin().await?;
        let change = if detach_run(&mut tx, run, reason).await? {
            self.bump_version(
                &mut tx,
                run.project_id,
                run.pipeline_attempt_id,
                BTreeSet::from([EntityRef::AgentRun(run.agent_run_id)]),
            )
            .await?
        } else {
            None
        };
        tx.commit().await?;
        Ok(change)
    }

    /// Applies one `events` frame (research 05 §9.4). See the module docs.
    pub async fn apply_batch(
        &self,
        run: &RunHandle,
        events: &[EventEnvelope],
    ) -> Result<Applied, ApplyError> {
        let mut sorted: Vec<&EventEnvelope> = events.iter().collect();
        sorted.sort_by_key(|e| e.seq);
        sorted.dedup_by_key(|e| e.seq);
        let redacted: Vec<Option<EventEnvelope>> =
            sorted.iter().map(|e| redact_log_text(e)).collect();
        let sorted: Vec<&EventEnvelope> = sorted
            .iter()
            .zip(&redacted)
            .map(|(e, r)| r.as_ref().unwrap_or(e))
            .collect();
        match self.apply_sorted(run, &sorted, false).await {
            Err(ApplyError::Store(Error::Database(e))) if is_data_error(e.as_ref()) => {
                tracing::warn!(agent_run_id = %run.agent_run_id, error = %e,
                    "an event broke a constraint; retrying the batch event by event");
                self.apply_sorted(run, &sorted, true).await
            }
            other => other,
        }
    }

    async fn apply_sorted(
        &self,
        run: &RunHandle,
        events: &[&EventEnvelope],
        savepoints: bool,
    ) -> Result<Applied, ApplyError> {
        let now = OffsetDateTime::now_utc();
        let mut tx = self.pool().begin().await?;
        let cursor = sqlx::query!(
            "SELECT epoch, committed_seq, state, end_seq FROM agent_runs WHERE id = $1 FOR UPDATE",
            run.agent_run_id
        )
        .fetch_one(&mut *tx)
        .await?;
        if u32::try_from(cursor.epoch).ok() != Some(run.epoch) {
            return Err(ApplyError::Superseded);
        }
        let prev = to_u64(cursor.committed_seq);
        let duplicates =
            u32::try_from(events.iter().filter(|e| e.seq <= prev).count()).unwrap_or(u32::MAX);
        let fresh: Vec<&EventEnvelope> = events.iter().copied().filter(|e| e.seq > prev).collect();
        let Some(first) = fresh.first() else {
            tx.commit().await?;
            return Ok(Applied {
                through: prev,
                duplicates,
                rejected: Vec::new(),
                expected: None,
                change: None,
            });
        };
        if !extends(first, prev) {
            return Err(ApplyError::SeqGap { expected: prev + 1 });
        }
        // The contiguous prefix; anything after a hole waits for a resend. A `journal.gap`
        // that covers a hole exactly keeps the prefix going.
        let mut through = prev;
        let contiguous: Vec<&EventEnvelope> = fresh
            .iter()
            .copied()
            .take_while(|e| {
                let next = extends(e, through);
                if next {
                    through = e.seq;
                }
                next
            })
            .collect();
        let expected = (contiguous.len() < fresh.len()).then_some(through + 1);
        let mut end_seq = cursor.end_seq.map(to_u64);

        let mut ctx = apply::Ctx::new(run, now);
        let mut journal = Vec::with_capacity(contiguous.len());
        for e in &contiguous {
            let effect = if contains_nul(&serde_json::to_value(e).unwrap_or_default()) {
                apply::Effect::Rejected("invalid", "event contains a NUL character".into())
            } else if let Some(end) = end_seq.filter(|end| e.seq > *end) {
                apply::Effect::Rejected("after_end", format!("the run ended at seq {end}"))
            } else if savepoints || e.kind == "shard.assigned" {
                apply_under_savepoint(&mut tx, &mut ctx, e, savepoints).await?
            } else {
                ctx.apply(&mut tx, e).await?
            };
            if effect == apply::Effect::Applied
                && matches!(e.decode(), Ok(Some(EventBody::RunEnded(_))))
            {
                end_seq = Some(e.seq);
            }
            journal.push((*e, effect));
        }

        let rejected = record_rejections(&mut tx, run, &journal).await?;
        write_journal(&mut tx, run, &journal).await?;
        let entities = ctx.derive(&mut tx).await?;

        let advanced = sqlx::query!(
            "UPDATE agent_runs SET committed_seq = $4, last_frame_at = now()
              WHERE id = $1 AND epoch = $2 AND committed_seq = $3",
            run.agent_run_id,
            cursor.epoch,
            cursor.committed_seq,
            to_i64(through)
        )
        .execute(&mut *tx)
        .await?;
        if advanced.rows_affected() != 1 {
            return Err(ApplyError::Superseded);
        }
        let change = self
            .bump_version(&mut tx, run.project_id, run.pipeline_attempt_id, entities)
            .await?;
        tx.commit().await?;
        Ok(Applied {
            through,
            duplicates,
            rejected,
            expected,
            change,
        })
    }
}

// Restore both the transaction and the accumulated projection changes when an event is rejected.
async fn apply_under_savepoint(
    tx: &mut PgConnection,
    ctx: &mut apply::Ctx<'_>,
    event: &EventEnvelope,
    reject_data_errors: bool,
) -> Result<apply::Effect, sqlx::Error> {
    let before = ctx.clone();
    let mut sp = tx.begin().await?;
    let effect = match ctx.apply(&mut sp, event).await {
        Ok(effect @ apply::Effect::Rejected(..)) => effect,
        Ok(effect) => {
            sp.commit().await?;
            return Ok(effect);
        }
        Err(err) if reject_data_errors && is_data_error(&err) => {
            apply::Effect::Rejected("invalid", format!("violates a constraint: {err}"))
        }
        Err(err) => return Err(err),
    };
    sp.rollback().await?;
    *ctx = before;
    Ok(effect)
}

/// Whether `e` continues a journal committed through `through`: it is the next `seq`, or it is a
/// `journal.gap` sequenced at its own `to_seq` whose `from_seq` is the next `seq`, so it stands in
/// for the events the agent dropped (the hole `from_seq..to_seq`, its own slot last). Anything
/// else after a hole is a sequence gap and waits for a resend.
fn extends(e: &EventEnvelope, through: u64) -> bool {
    e.seq == through + 1
        || (e.kind == "journal.gap"
            && matches!(
                e.decode(),
                Ok(Some(EventBody::JournalGap(g)))
                    if g.to_seq == e.seq && g.from_seq == through + 1 && g.from_seq <= g.to_seq
            ))
}

/// Constraint, foreign-key and bad-data errors: the event is at fault, not the database.
fn is_data_error(e: &(dyn std::error::Error + 'static)) -> bool {
    let Some(sqlx::Error::Database(db)) = e.downcast_ref::<sqlx::Error>() else {
        return false;
    };
    db.code()
        .is_some_and(|c| c.starts_with("22") || c.starts_with("23"))
}

/// Detaches the run if this connection still holds it, and records that the connection closed.
/// Returns whether the run was active and is now detached.
async fn detach_run(tx: &mut PgConnection, run: &RunHandle, reason: &str) -> Result<bool> {
    let flipped = sqlx::query!(
        "UPDATE agent_runs SET state = 'detached', detached_at = now(), rev = rev + 1
          WHERE id = $1 AND epoch = $2 AND state = 'active'",
        run.agent_run_id,
        i32::try_from(run.epoch).unwrap_or(i32::MAX)
    )
    .execute(&mut *tx)
    .await?
    .rows_affected()
        > 0;
    sqlx::query!(
        "UPDATE connections SET closed_at = now(), close_reason = $2
          WHERE id = $1 AND closed_at IS NULL",
        run.connection_id,
        reason
    )
    .execute(&mut *tx)
    .await?;
    Ok(flipped)
}

/// Takes the run over for this connection: bumps the epoch (fencing older sockets, §3.7) and
/// records the connection.
async fn fence(
    tx: &mut PgConnection,
    agent_run_id: Uuid,
    committed: u64,
    transport: Transport,
    worker_id: Uuid,
    job_id: Uuid,
) -> Result<(i32, Uuid, Uuid), sqlx::Error> {
    let epoch: i32 = sqlx::query_scalar!(
        "UPDATE agent_runs
                SET epoch = epoch + 1,
                    committed_seq = $2,
                    state = CASE WHEN state = 'ended' THEN state
                                 WHEN state = 'finalizing' THEN state
                                 ELSE 'active' END,
                    late_resume = late_resume OR state = 'abandoned',
                    detached_at = NULL,
                    last_frame_at = now(),
                    rev = rev + 1
              WHERE id = $1
              RETURNING epoch",
        agent_run_id,
        to_i64(committed)
    )
    .fetch_one(&mut *tx)
    .await?;
    let transport = match transport {
        Transport::WebSocket => "websocket",
        Transport::Batch => "batch",
    };
    let connection_id = sqlx::query_scalar!(
        "INSERT INTO connections (agent_run_id, epoch, transport) VALUES ($1, $2, $3)
             RETURNING id",
        agent_run_id,
        epoch,
        transport
    )
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query!(
        "UPDATE workers SET last_seen_at = now() WHERE id = $1",
        worker_id
    )
    .execute(&mut *tx)
    .await?;
    let attempt_id =
        sqlx::query_scalar!("SELECT pipeline_attempt_id FROM jobs WHERE id = $1", job_id)
            .fetch_one(&mut *tx)
            .await?;

    Ok((epoch, connection_id, attempt_id))
}

/// The resume rules that refuse a run (research 05 §3.4).
fn check_resume(
    publisher: &Publisher,
    owner: Uuid,
    committed: u64,
    next_seq: u64,
) -> Result<(), OpenError> {
    if owner != publisher.project_id {
        return Err(refused(
            ErrorCode::Unauthorized,
            "the agent run belongs to another project",
            true,
        ));
    }
    if committed >= next_seq {
        return Err(refused(
            ErrorCode::SeqRegression,
            format!(
                "the server committed through {committed} but the agent's next seq is {next_seq}"
            ),
            true,
        ));
    }
    Ok(())
}

/// The day a run belongs to (its `run_day` partition key): the day it started, as the agent
/// recorded it, or the day the server first heard from it. A published run keeps the day of the
/// run it describes, not the day it was published (issue #83).
fn run_day_of(hello: &Hello, now: OffsetDateTime) -> Date {
    hello
        .started_at
        .unwrap_or(now)
        .to_offset(time::UtcOffset::UTC)
        .date()
}

async fn insert_run(
    tx: &mut PgConnection,
    publisher: &Publisher,
    hello: &Hello,
    job_id: Uuid,
    worker_id: Uuid,
    run_day: Date,
) -> Result<(), sqlx::Error> {
    let (token_id, api_key_id) = match publisher.credential {
        Credential::AgentToken(t) => (Some(t), None),
        Credential::ApiKey(k) => (None, Some(k)),
    };
    // A run from an earlier day than the partitions the hourly job keeps (a published run) needs
    // its day's partitions first; the function takes an advisory lock, so creators do not race.
    sqlx::query(
        "SELECT fleet_ensure_partitions('agent_events'::regclass, $1, $1),
                fleet_ensure_partitions('step_executions'::regclass, $1, $1)",
    )
    .bind(run_day)
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        "INSERT INTO agent_runs (id, project_id, job_id, worker_id, token_id, api_key_id,
                                 previous_agent_run_id, trust_level, run_day,
                                 agent_version, protocol_version, origin)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
        hello.agent_run_id,
        publisher.project_id,
        job_id,
        worker_id,
        token_id,
        api_key_id,
        hello.previous_agent_run_id,
        publisher.trust.as_str(),
        run_day,
        hello.agent.version,
        i16::try_from(hello.protocol.max).unwrap_or(i16::MAX),
        hello.origin.as_str()
    )
    .execute(&mut *tx)
    .await?;
    Ok(())
}

async fn upsert_hierarchy(
    tx: &mut PgConnection,
    publisher: &Publisher,
    hello: &Hello,
    id: &Identity,
) -> Result<(Uuid, Uuid), sqlx::Error> {
    let project = publisher.project_id;
    let run_id = sqlx::query_scalar!(
        "INSERT INTO pipeline_runs (project_id, provider, provider_run_key, repository, commit_sha, git_ref)
         VALUES ($1, $2, $3, $4, $5, $6)
         ON CONFLICT (project_id, provider, provider_run_key) DO UPDATE
           SET commit_sha = coalesce(pipeline_runs.commit_sha, EXCLUDED.commit_sha)
         RETURNING id",
        project,
        id.provider,
        id.run_key,
        id.repository,
        id.sha,
        id.git_ref
    )
    .fetch_one(&mut *tx)
    .await?;
    // DO NOTHING, not DO UPDATE: locking the attempt row here, before the job row, would invert
    // apply_sorted's order (job row in derive, attempt row in bump_version) and deadlock.
    let inserted = sqlx::query_scalar!(
        "INSERT INTO pipeline_attempts (project_id, pipeline_run_id, attempt) VALUES ($1, $2, $3)
         ON CONFLICT (pipeline_run_id, attempt) DO NOTHING
         RETURNING id",
        project,
        run_id,
        id.attempt
    )
    .fetch_optional(&mut *tx)
    .await?;
    let attempt_id = match inserted {
        Some(id) => id,
        None => {
            sqlx::query_scalar!(
                "SELECT id FROM pipeline_attempts WHERE pipeline_run_id = $1 AND attempt = $2",
                run_id,
                id.attempt
            )
            .fetch_one(&mut *tx)
            .await?
        }
    };
    let job_id = sqlx::query_scalar!(
        "INSERT INTO jobs (project_id, pipeline_attempt_id, job_key, name) VALUES ($1, $2, $3, $4)
         ON CONFLICT (pipeline_attempt_id, job_key) DO UPDATE
           SET name = coalesce(jobs.name, EXCLUDED.name)
         RETURNING id",
        project,
        attempt_id,
        id.job_key,
        id.job_name
    )
    .fetch_one(&mut *tx)
    .await?;
    let runner = hello.ci.runner.as_deref().unwrap_or("unknown");
    let identity_key = format!("{runner}|{}|{}", hello.agent.os, hello.agent.arch);
    let worker_id = sqlx::query_scalar!(
        "INSERT INTO workers (project_id, identity_key, runner_name, os, arch)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (project_id, identity_key) DO UPDATE SET last_seen_at = now()
         RETURNING id",
        project,
        identity_key,
        hello.ci.runner,
        hello.agent.os,
        hello.agent.arch
    )
    .fetch_one(&mut *tx)
    .await?;
    Ok((job_id, worker_id))
}

fn contains_nul(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::String(s) => s.contains('\0'),
        serde_json::Value::Array(values) => values.iter().any(contains_nul),
        serde_json::Value::Object(values) => values
            .iter()
            .any(|(k, v)| k.contains('\0') || contains_nul(v)),
        _ => false,
    }
}

/// Token shapes only: the server has no known-secret set, so this is the second line of
/// redaction behind the agent's (research 06 T6).
static TOKEN_SHAPES: LazyLock<Redactor> = LazyLock::new(|| Redactor::new(std::iter::empty()));

/// Event types whose text is Maestro console or log output.
const LOG_TEXT_EVENTS: &[&str] = &[
    "step.started",
    "step.ended",
    "flow.reported",
    "flow.ended",
    "app.crashed",
];

/// The event with token-shaped text replaced, when it has any: a log line that carries a token
/// is stored redacted even when the agent sent it unredacted. Only [`LOG_TEXT_EVENTS`] are
/// looked at, so other events are passed on untouched.
fn redact_log_text(e: &EventEnvelope) -> Option<EventEnvelope> {
    if !LOG_TEXT_EVENTS.contains(&e.kind.as_str()) {
        return None;
    }
    let mut data = e.data.clone();
    if !redact_json(&mut data) {
        return None;
    }
    Some(EventEnvelope { data, ..e.clone() })
}

/// Redacts every string in `value`; `true` when one changed.
fn redact_json(value: &mut serde_json::Value) -> bool {
    match value {
        serde_json::Value::String(s) => {
            if let Cow::Owned(redacted) = TOKEN_SHAPES.redact(s) {
                *s = redacted;
                true
            } else {
                false
            }
        }
        serde_json::Value::Array(values) => {
            values.iter_mut().fold(false, |c, v| redact_json(v) | c)
        }
        serde_json::Value::Object(map) => map.values_mut().fold(false, |c, v| redact_json(v) | c),
        _ => false,
    }
}

fn safe_json(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::String(s) => serde_json::Value::String(s.replace('\0', "\\u0000")),
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.into_iter().map(safe_json).collect())
        }
        serde_json::Value::Object(values) => serde_json::Value::Object(
            values
                .into_iter()
                .map(|(k, v)| (k.replace('\0', "\\u0000"), safe_json(v)))
                .collect(),
        ),
        other => other,
    }
}

/// Stores refused events in `ingest_rejections` (and audits ID reuse across runs).
async fn record_rejections(
    tx: &mut PgConnection,
    run: &RunHandle,
    journal: &[(&EventEnvelope, apply::Effect)],
) -> Result<Vec<Rejection>, sqlx::Error> {
    let mut rejected = Vec::new();
    for (e, effect) in journal {
        let apply::Effect::Rejected(code, message) = effect else {
            continue;
        };
        sqlx::query!(
            "INSERT INTO ingest_rejections (agent_run_id, seq, code, message, raw)
             VALUES ($1, $2, $3, $4, $5) ON CONFLICT DO NOTHING",
            run.agent_run_id,
            to_i64(e.seq),
            code,
            message,
            safe_json(serde_json::to_value(e).unwrap_or_default())
        )
        .execute(&mut *tx)
        .await?;
        if *code == "foreign_entity" {
            // Reusing another run's IDs is either a bug or an attack: worth an audit row.
            sqlx::query!(
                "INSERT INTO audit_events (actor_kind, project_id, action, target_kind,
                                           target_id, details)
                 VALUES ('system', $1, 'ingest.foreign_entity', 'agent_run', $2, $3)",
                run.project_id,
                run.agent_run_id,
                serde_json::json!({ "seq": e.seq, "message": message })
            )
            .execute(&mut *tx)
            .await?;
        }
        rejected.push(Rejection {
            seq: e.seq,
            code: ErrorCode::Invalid,
            message: format!("{code}: {message}"),
        });
    }
    Ok(rejected)
}

async fn write_journal(
    tx: &mut PgConnection,
    run: &RunHandle,
    journal: &[(&EventEnvelope, apply::Effect)],
) -> Result<(), sqlx::Error> {
    if journal.is_empty() {
        return Ok(());
    }
    let mut seq = Vec::with_capacity(journal.len());
    let mut kind = Vec::with_capacity(journal.len());
    let mut v = Vec::with_capacity(journal.len());
    let mut src_kind = Vec::with_capacity(journal.len());
    let mut src_parser = Vec::with_capacity(journal.len());
    let mut occurred = Vec::with_capacity(journal.len());
    let mut mono = Vec::with_capacity(journal.len());
    let mut subject = Vec::with_capacity(journal.len());
    let mut flow = Vec::with_capacity(journal.len());
    let mut status = Vec::with_capacity(journal.len());
    let mut data = Vec::with_capacity(journal.len());
    for (e, effect) in journal {
        seq.push(to_i64(e.seq));
        kind.push(e.kind.replace('\0', "\\u0000"));
        v.push(i16::try_from(e.v).unwrap_or(i16::MAX));
        src_kind.push(
            serde_json::to_value(e.src.kind)
                .ok()
                .and_then(|k| k.as_str().map(str::to_owned))
                .unwrap_or_else(|| "unrecognized".into()),
        );
        src_parser.push(e.src.parser.as_ref().map(|s| s.replace('\0', "\\u0000")));
        occurred.push(e.at.wall);
        mono.push(to_i64(e.at.mono_ms));
        subject.push(safe_json(
            serde_json::to_value(&e.subject).unwrap_or_default(),
        ));
        flow.push(e.subject.flow_execution_id);
        status.push(effect.status().to_owned());
        data.push(safe_json(e.data.clone()));
    }
    sqlx::query!(
        "INSERT INTO agent_events (run_day, agent_run_id, seq, type, v, src_kind, src_parser,
                                   occurred_at, mono_ms, subject, flow_execution_id,
                                   apply_status, data)
         SELECT $1, $2, u.seq, u.type, u.v, u.src_kind, u.src_parser, u.occurred_at, u.mono_ms,
                u.subject, u.flow, u.status, u.data
           FROM unnest($3::bigint[], $4::text[], $5::smallint[], $6::text[], $7::text[],
                       $8::timestamptz[], $9::bigint[], $10::jsonb[], $11::uuid[], $12::text[],
                       $13::jsonb[])
             AS u(seq, type, v, src_kind, src_parser, occurred_at, mono_ms, subject, flow,
                  status, data)
         ON CONFLICT DO NOTHING",
        run.run_day,
        run.agent_run_id,
        &seq,
        &kind,
        &v,
        &src_kind,
        &src_parser as &[Option<String>],
        &occurred,
        &mono,
        &subject,
        &flow as &[Option<Uuid>],
        &status,
        &data
    )
    .execute(&mut *tx)
    .await?;
    Ok(())
}

impl Store {
    /// Bumps the attempt's live version when anything changed; the hub orders deltas by it. The
    /// version's notice goes out with the same transaction, for the other instances' hubs.
    pub(crate) async fn bump_version(
        &self,
        tx: &mut PgConnection,
        project_id: Uuid,
        attempt_id: Uuid,
        entities: BTreeSet<EntityRef>,
    ) -> Result<Option<Change>, sqlx::Error> {
        if entities.is_empty() {
            return Ok(None);
        }
        let version = sqlx::query_scalar!(
            "UPDATE pipeline_attempts SET version = version + 1 WHERE id = $1 RETURNING version",
            attempt_id
        )
        .fetch_one(&mut *tx)
        .await?;
        let change = Change {
            project_id,
            pipeline_attempt_id: attempt_id,
            version: to_u64(version),
            entities: entities.into_iter().collect(),
        };
        crate::live::notify(tx, self.origin(), &change).await?;
        Ok(Some(change))
    }
}

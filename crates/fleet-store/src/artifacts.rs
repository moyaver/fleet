// SPDX-License-Identifier: AGPL-3.0-only
//! Artifact rows and their upload state machine (PLAN §7.6; research 05 §12.1).
//!
//! ```text
//! request_upload ──► declared ──claim_content──► uploading ──content_received──► (spooled)
//!        │              ▲  └──── grant lapses ──── │  abort_content (stream failed) ─┘
//!        │              └────────── re-request ────┘
//!        ├──► available (the project already has these bytes for this class: `Exists`)
//!        └──► rejected  (remembered: the same bytes are refused the same way)
//! (spooled) ──mark_available──► available       (spooled) ──mark_rejected──► rejected
//! ```
//!
//! This module holds the rows and the rules that need the database (idempotency, per-run quotas,
//! per-project deduplication, single-use grants). Verifying bytes and writing the blob store is
//! the server's job; it reports back with [`Store::mark_available`] or [`Store::mark_rejected`].
//! Digests cross this API as lowercase hex.

use std::time::Duration;

use fleet_protocol::artifact::ArtifactClass;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{Result, Store, auth::Credential};

/// The retention class an artifact starts in, before its flow's outcome is known (research 05
/// §12.3). Evidence of a failed flow is promoted to `failure_evidence` when it finalizes (or at
/// once, if the flow had already failed when the file arrived).
#[must_use]
pub fn default_retention(class: ArtifactClass) -> &'static str {
    match class {
        ArtifactClass::Junit | ArtifactClass::HtmlReport => "report",
        ArtifactClass::RunnerLog | ArtifactClass::DeviceLogcat => "debug_log",
        ArtifactClass::UserScreenshot | ArtifactClass::UserRecording => "user_media",
        ArtifactClass::FailureScreenshot
        | ArtifactClass::FailureHierarchy
        | ArtifactClass::Commands => "success_evidence",
    }
}

impl Credential {
    /// The ID of the API key or agent token.
    #[must_use]
    pub fn id(self) -> Uuid {
        match self {
            Self::ApiKey(id) | Self::AgentToken(id) => id,
        }
    }
}

/// What an agent asks to upload.
#[derive(Debug, Clone)]
pub struct UploadAsk<'a> {
    /// The caller's project.
    pub project_id: Uuid,
    /// The caller's credential: the grant is bound to it.
    pub credential: Credential,
    /// The run that produced the file.
    pub agent_run_id: Uuid,
    /// The flow it is evidence for.
    pub flow_execution_id: Option<Uuid>,
    /// The flow step that produced it, when known (see [`ArtifactRow::step`]).
    pub step: Option<u32>,
    /// Its class.
    pub class: ArtifactClass,
    /// A sanitised label relative to the output directory.
    pub path: &'a str,
    /// The label's last segment.
    pub name: &'a str,
    /// The declared `Content-Type`.
    pub content_type: &'a str,
    /// Size in bytes.
    pub size: u64,
    /// SHA-256, 64 lowercase hex digits.
    pub sha256: &'a str,
}

/// What one run may store, and what its project may declare in a day (research 06 T12).
#[derive(Debug, Clone, Copy)]
pub struct Quota {
    /// Bytes across all of the run's artifacts.
    pub max_bytes: u64,
    /// Number of artifacts.
    pub max_files: u32,
    /// Bytes across the project's artifacts declared in the last 24 hours, in all its runs.
    pub project_max_bytes: u64,
    /// Number of the project's artifacts declared in the last 24 hours, in all its runs.
    pub project_max_files: u32,
}

/// The answer to [`Store::request_upload`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Requested {
    /// Send the bytes before `expires_at`.
    Grant {
        /// The artifact.
        artifact_id: Uuid,
        /// When the grant lapses.
        expires_at: OffsetDateTime,
    },
    /// Nothing to send: the artifact is available.
    Exists {
        /// The artifact.
        artifact_id: Uuid,
    },
    /// These bytes were refused before; they are refused again.
    Rejected {
        /// The artifact row that records it.
        artifact_id: Uuid,
        /// The refusal's code.
        reason: String,
    },
    /// The run doesn't exist in the caller's project (yet).
    UnknownRun,
    /// The flow doesn't exist in the run (yet).
    UnknownFlow,
    /// The run is over its quota.
    OverQuota,
    /// The project is over its quota for the last 24 hours.
    OverProjectQuota,
}

/// The result of claiming a grant for its bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Claim {
    /// Go ahead and receive exactly `size` bytes.
    Claimed {
        /// The declared size.
        size: u64,
    },
    /// No such artifact for this caller.
    NotFound,
    /// The grant lapsed.
    Expired,
    /// The bytes were already received (the grant is single-use), or the artifact is final.
    AlreadyUsed,
}

/// What `complete` finds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pending {
    /// Bytes are in the spool: verify and store them.
    Ready(PendingArtifact),
    /// Already finished with this outcome (a repeated `complete`).
    Done(Finished),
    /// No such artifact for this caller.
    NotFound,
    /// The bytes haven't (all) arrived.
    NotReceived,
}

/// The facts verification needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingArtifact {
    /// The artifact.
    pub id: Uuid,
    /// Its class.
    pub class: ArtifactClass,
    /// The declared size.
    pub size: u64,
    /// The declared SHA-256.
    pub sha256: String,
}

/// A finished artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Finished {
    /// Verified and stored.
    Available,
    /// Refused, with the code.
    Rejected(String),
}

/// An artifact as the dashboard lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactRow {
    /// ID.
    pub id: Uuid,
    /// The flow it is evidence for; `None` for a file of the whole run (report, log).
    pub flow_execution_id: Option<Uuid>,
    /// The flow step that produced it, as Maestro's `artifacts[]` names it; `None` when no step
    /// did, and for artifacts uploaded before step links were stored.
    pub step: Option<u32>,
    /// Class (the wire name).
    pub class: String,
    /// Last path segment.
    pub name: String,
    /// Path label.
    pub path: String,
    /// The verified content type; for a rejected artifact, the declared one.
    pub content_type: String,
    /// Size in bytes.
    pub size_bytes: i64,
    /// SHA-256, lowercase hex.
    pub sha256: String,
    /// `available` or `rejected`.
    pub state: String,
    /// Why it was refused.
    pub reject_reason: Option<String>,
    /// Its retention class.
    pub retention_class: String,
    /// When the request arrived.
    pub created_at: OffsetDateTime,
}

/// What the usercontent origin needs to serve an artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Servable {
    /// SHA-256, lowercase hex: with the project, the blob's key.
    pub sha256: String,
    /// Size in bytes.
    pub size_bytes: i64,
    /// Class.
    pub class: ArtifactClass,
    /// The verified content type.
    pub content_type: String,
    /// File name for `Content-Disposition`.
    pub name: String,
}

fn secs(d: Duration) -> f64 {
    d.as_secs_f64()
}

impl Store {
    /// Records an upload request and decides what the agent does next. Idempotent on
    /// `(agent_run_id, class, sha256)`; see the module docs for the cases.
    pub async fn request_upload(
        &self,
        ask: &UploadAsk<'_>,
        grant_ttl: Duration,
        quota: Quota,
    ) -> Result<Requested> {
        let mut tx = self.pool().begin().await?;
        // One request per run at a time: the quota check and the insert must not interleave.
        sqlx::query!(
            "SELECT pg_advisory_xact_lock(hashtextextended($1::text, 0))",
            ask.agent_run_id.to_string()
        )
        .execute(&mut *tx)
        .await?;
        let scope = match scope_of(&mut tx, ask).await? {
            Ok(scope) => scope,
            Err(refused) => return Ok(refused),
        };
        let existing = sqlx::query!(
            "SELECT id, state, reject_reason FROM artifacts
              WHERE agent_run_id = $1 AND class = $2 AND sha256 = decode($3, 'hex')",
            ask.agent_run_id,
            ask.class.as_str(),
            ask.sha256
        )
        .fetch_optional(&mut *tx)
        .await?;
        let outcome = match existing {
            Some(row) => match row.state.as_str() {
                "available" => Requested::Exists {
                    artifact_id: row.id,
                },
                "rejected" => Requested::Rejected {
                    artifact_id: row.id,
                    reason: row.reject_reason.unwrap_or_default(),
                },
                // A retry after a crash, a lapsed grant or a failed transfer: start over.
                _ => regrant(&mut tx, ask, row.id, grant_ttl).await?,
            },
            None => declare(&mut tx, ask, scope, grant_ttl, quota).await?,
        };
        tx.commit().await?;
        Ok(outcome)
    }

    /// Claims a grant for its bytes: only the credential it was issued to, once, before it
    /// lapses. Follow with [`content_received`](Self::content_received), or
    /// [`abort_content`](Self::abort_content) if the transfer failed.
    pub async fn claim_content(
        &self,
        project_id: Uuid,
        credential: Credential,
        artifact_id: Uuid,
    ) -> Result<Claim> {
        let claimed = sqlx::query_scalar!(
            "UPDATE artifacts SET state = 'uploading', received_at = NULL
              WHERE id = $1 AND project_id = $2 AND credential_id = $3 AND state = 'declared'
                AND grant_expires_at > now()
              RETURNING size_bytes",
            artifact_id,
            project_id,
            credential.id()
        )
        .fetch_optional(self.pool())
        .await?;
        if let Some(size) = claimed {
            return Ok(Claim::Claimed {
                size: u64::try_from(size).unwrap_or(0),
            });
        }
        let row = sqlx::query!(
            "SELECT state FROM artifacts
              WHERE id = $1 AND project_id = $2 AND credential_id = $3",
            artifact_id,
            project_id,
            credential.id()
        )
        .fetch_optional(self.pool())
        .await?;
        Ok(match row {
            None => Claim::NotFound,
            Some(r) if r.state == "declared" => Claim::Expired,
            Some(_) => Claim::AlreadyUsed,
        })
    }

    /// The claimed bytes arrived whole and sit in the spool.
    pub async fn content_received(&self, artifact_id: Uuid) -> Result<()> {
        sqlx::query!(
            "UPDATE artifacts SET received_at = now() WHERE id = $1 AND state = 'uploading'",
            artifact_id
        )
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// The transfer failed: the grant is usable again until it lapses.
    pub async fn abort_content(&self, artifact_id: Uuid) -> Result<()> {
        sqlx::query!(
            "UPDATE artifacts SET state = 'declared'
              WHERE id = $1 AND state = 'uploading' AND received_at IS NULL",
            artifact_id
        )
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// What `complete` should do for this artifact.
    pub async fn pending_complete(
        &self,
        project_id: Uuid,
        credential: Credential,
        artifact_id: Uuid,
    ) -> Result<Pending> {
        let Some(r) = sqlx::query!(
            "SELECT state, class, size_bytes, encode(sha256, 'hex') AS \"sha256!\", reject_reason,
                    received_at IS NOT NULL AS \"received!\"
               FROM artifacts
              WHERE id = $1 AND project_id = $2 AND credential_id = $3",
            artifact_id,
            project_id,
            credential.id()
        )
        .fetch_optional(self.pool())
        .await?
        else {
            return Ok(Pending::NotFound);
        };
        Ok(match (r.state.as_str(), r.received) {
            ("available", _) => Pending::Done(Finished::Available),
            ("rejected", _) => {
                Pending::Done(Finished::Rejected(r.reject_reason.unwrap_or_default()))
            }
            ("uploading", true) => match ArtifactClass::parse(&r.class) {
                Some(class) => Pending::Ready(PendingArtifact {
                    id: artifact_id,
                    class,
                    size: u64::try_from(r.size_bytes).unwrap_or(0),
                    sha256: r.sha256,
                }),
                None => {
                    return Err(crate::Error::Corrupt(format!(
                        "artifact {artifact_id} has class {:?}",
                        r.class
                    )));
                }
            },
            _ => Pending::NotReceived,
        })
    }

    /// The bytes are verified and in the blob store: record the blob, make the artifact
    /// available and set its retention. Idempotent.
    pub async fn mark_available(
        &self,
        project_id: Uuid,
        artifact_id: Uuid,
        served_type: &str,
    ) -> Result<()> {
        let mut tx = self.pool().begin().await?;
        let updated = sqlx::query_scalar!(
            "UPDATE artifacts a
                SET state = 'available', served_type = $3, available_at = now(),
                    grant_expires_at = NULL
              WHERE a.id = $1 AND a.project_id = $2 AND a.state = 'uploading'
              RETURNING a.id",
            artifact_id,
            project_id,
            served_type
        )
        .fetch_optional(&mut *tx)
        .await?;
        if updated.is_some() {
            sqlx::query!(
                "INSERT INTO blobs (project_id, sha256, size_bytes)
                 SELECT project_id, sha256, size_bytes FROM artifacts WHERE id = $1
                 ON CONFLICT (project_id, sha256) DO UPDATE
                   SET unreferenced_since = NULL, verified_at = now()",
                artifact_id
            )
            .execute(&mut *tx)
            .await?;
            set_retention(&mut tx, artifact_id).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Verification refused the bytes: record why. Nothing else is kept.
    pub async fn mark_rejected(&self, artifact_id: Uuid, reason: &str) -> Result<()> {
        sqlx::query!(
            "UPDATE artifacts SET state = 'rejected', reject_reason = $2, grant_expires_at = NULL
              WHERE id = $1 AND state IN ('declared', 'uploading')",
            artifact_id,
            reason
        )
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// A flow's artifacts, oldest first: the available ones and the refused ones, including the
    /// files of the flow's whole agent run (report, log). `None` when the flow doesn't exist in
    /// the project.
    pub async fn flow_artifacts(
        &self,
        project_id: Uuid,
        flow_execution_id: Uuid,
    ) -> Result<Option<Vec<ArtifactRow>>> {
        let exists = sqlx::query_scalar!(
            "SELECT true AS \"found!\" FROM flow_executions WHERE id = $1 AND project_id = $2",
            flow_execution_id,
            project_id
        )
        .fetch_optional(self.pool())
        .await?;
        if exists.is_none() {
            return Ok(None);
        }
        Ok(Some(
            sqlx::query!(
                "SELECT id, flow_execution_id, step_index, class, name, path,
                    coalesce(served_type, content_type) AS \"content_type!\",
                    size_bytes, encode(sha256, 'hex') AS \"sha256!\", state, reject_reason,
                    retention_class, created_at
               FROM artifacts
              WHERE project_id = $1
                AND (flow_execution_id = $2
                     OR (flow_execution_id IS NULL AND agent_run_id =
                         (SELECT agent_run_id FROM flow_executions WHERE id = $2)))
                AND state IN ('available', 'rejected')
              ORDER BY created_at, id",
                project_id,
                flow_execution_id
            )
            .fetch_all(self.pool())
            .await?
            .into_iter()
            .map(|r| ArtifactRow {
                id: r.id,
                flow_execution_id: r.flow_execution_id,
                step: r.step_index.and_then(|s| u32::try_from(s).ok()),
                class: r.class,
                name: r.name,
                path: r.path,
                content_type: r.content_type,
                size_bytes: r.size_bytes,
                sha256: r.sha256,
                state: r.state,
                reject_reason: r.reject_reason,
                retention_class: r.retention_class,
                created_at: r.created_at,
            })
            .collect(),
        ))
    }

    /// An available artifact of `project_id`, for the usercontent origin.
    pub async fn servable(&self, project_id: Uuid, artifact_id: Uuid) -> Result<Option<Servable>> {
        let Some(r) = sqlx::query!(
            "SELECT class, name, served_type AS \"served_type!\", size_bytes,
                    encode(sha256, 'hex') AS \"sha256!\"
               FROM artifacts
              WHERE id = $1 AND project_id = $2 AND state = 'available'",
            artifact_id,
            project_id
        )
        .fetch_optional(self.pool())
        .await?
        else {
            return Ok(None);
        };
        let class = ArtifactClass::parse(&r.class).ok_or_else(|| {
            crate::Error::Corrupt(format!("artifact {artifact_id} has class {:?}", r.class))
        })?;
        Ok(Some(Servable {
            sha256: r.sha256,
            size_bytes: r.size_bytes,
            class,
            content_type: r.served_type,
            name: r.name,
        }))
    }

    /// Spool files to keep: artifacts whose grant is open or whose bytes await `complete`.
    pub async fn open_uploads(&self) -> Result<Vec<Uuid>> {
        Ok(sqlx::query_scalar!(
            "SELECT id FROM artifacts
              WHERE state = 'uploading'
                 OR (state = 'declared' AND grant_expires_at > now())"
        )
        .fetch_all(self.pool())
        .await?)
    }
}

/// Fixes an available artifact's retention: evidence of a flow that has already failed starts
/// as `failure_evidence`, and so does the run's `maestro.log` once any of its flows failed (later
/// failures are promoted when the flow finalizes, [`promote_failed_evidence`]); `expires_at`
/// follows the class's TTL.
async fn set_retention(tx: &mut sqlx::PgConnection, artifact_id: Uuid) -> Result<()> {
    sqlx::query!(
        "UPDATE artifacts a SET retention_class = 'failure_evidence'
          WHERE a.id = $1
            AND ((a.retention_class = 'success_evidence'
                  AND EXISTS (SELECT 1 FROM flow_executions f
                               WHERE f.id = a.flow_execution_id AND f.state = 'failed'))
              OR (a.retention_class = 'debug_log'
                  AND EXISTS (SELECT 1 FROM flow_executions f
                               WHERE f.agent_run_id = a.agent_run_id AND f.state = 'failed')))",
        artifact_id
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        "UPDATE artifacts a SET expires_at = a.available_at + rc.ttl
           FROM retention_classes rc
          WHERE a.id = $1 AND rc.project_id = a.project_id AND rc.class = a.retention_class",
        artifact_id
    )
    .execute(tx)
    .await?;
    Ok(())
}

/// Promotes the evidence of flows that finalized as `failed` (research 05 §12.3), in the same
/// transaction that set their state: their screenshots and commands, and the run's Maestro log,
/// move from the short default retention to `failure_evidence`. A promotion never shortens an
/// artifact's life.
pub(crate) async fn promote_failed_evidence(
    tx: &mut sqlx::PgConnection,
    flows: &[Uuid],
) -> Result<(), sqlx::Error> {
    if flows.is_empty() {
        return Ok(());
    }
    sqlx::query!(
        "UPDATE artifacts a
            SET retention_class = 'failure_evidence',
                expires_at = CASE WHEN rc.ttl IS NULL OR a.expires_at IS NULL THEN NULL
                                  ELSE greatest(a.expires_at, a.available_at + rc.ttl) END
           FROM flow_executions f, retention_classes rc
          WHERE f.id = ANY($1) AND f.state = 'failed' AND a.state = 'available'
            AND rc.project_id = a.project_id AND rc.class = 'failure_evidence'
            AND ((a.flow_execution_id = f.id AND a.retention_class = 'success_evidence')
              OR (a.agent_run_id = f.agent_run_id AND a.retention_class = 'debug_log'))",
        flows
    )
    .execute(tx)
    .await?;
    Ok(())
}

/// The run's job and the flow's suite, or the answer that refuses the request.
struct Scope {
    job_id: Uuid,
    suite_execution_id: Option<Uuid>,
}

async fn scope_of(
    tx: &mut sqlx::PgConnection,
    ask: &UploadAsk<'_>,
) -> Result<std::result::Result<Scope, Requested>> {
    let Some(run) = sqlx::query!(
        "SELECT job_id FROM agent_runs WHERE id = $1 AND project_id = $2",
        ask.agent_run_id,
        ask.project_id
    )
    .fetch_optional(&mut *tx)
    .await?
    else {
        return Ok(Err(Requested::UnknownRun));
    };
    let suite_execution_id = match ask.flow_execution_id {
        Some(flow) => {
            let found = sqlx::query_scalar!(
                "SELECT suite_execution_id FROM flow_executions
                  WHERE id = $1 AND agent_run_id = $2",
                flow,
                ask.agent_run_id
            )
            .fetch_optional(&mut *tx)
            .await?;
            match found {
                Some(suite) => Some(suite),
                None => return Ok(Err(Requested::UnknownFlow)),
            }
        }
        None => None,
    };
    Ok(Ok(Scope {
        job_id: run.job_id,
        suite_execution_id,
    }))
}

/// Gives an unfinished artifact a fresh grant for the asking credential.
async fn regrant(
    tx: &mut sqlx::PgConnection,
    ask: &UploadAsk<'_>,
    artifact_id: Uuid,
    grant_ttl: Duration,
) -> Result<Requested> {
    let expires_at = sqlx::query_scalar!(
        "UPDATE artifacts
            SET state = 'declared', received_at = NULL, credential_id = $2,
                grant_expires_at = now() + make_interval(secs => $3)
          WHERE id = $1 RETURNING grant_expires_at AS \"at!\"",
        artifact_id,
        ask.credential.id(),
        secs(grant_ttl)
    )
    .fetch_one(tx)
    .await?;
    Ok(Requested::Grant {
        artifact_id,
        expires_at,
    })
}

/// The quota refusal for this upload, if it would take its run or its project over quota.
async fn over_quota(
    tx: &mut sqlx::PgConnection,
    ask: &UploadAsk<'_>,
    quota: Quota,
) -> Result<Option<Requested>> {
    let used = sqlx::query!(
        "SELECT count(*) AS \"files!\", coalesce(sum(size_bytes), 0)::bigint AS \"bytes!\"
           FROM artifacts
          WHERE agent_run_id = $1 AND state IN ('declared', 'uploading', 'available')",
        ask.agent_run_id
    )
    .fetch_one(&mut *tx)
    .await?;
    let over_files = used.files >= i64::from(quota.max_files);
    let over_bytes = u64::try_from(used.bytes)
        .unwrap_or(u64::MAX)
        .saturating_add(ask.size)
        > quota.max_bytes;
    if over_files || over_bytes {
        return Ok(Some(Requested::OverQuota));
    }
    // The same check over the project's last 24 hours, so one project cannot fill storage
    // by many runs, each within its own run quota.
    let day = sqlx::query!(
        "SELECT count(*) AS \"files!\", coalesce(sum(size_bytes), 0)::bigint AS \"bytes!\"
           FROM artifacts
          WHERE project_id = $1 AND created_at > now() - interval '24 hours'
            AND state IN ('declared', 'uploading', 'available')",
        ask.project_id
    )
    .fetch_one(&mut *tx)
    .await?;
    let over_project_files = day.files >= i64::from(quota.project_max_files);
    let over_project_bytes = u64::try_from(day.bytes)
        .unwrap_or(u64::MAX)
        .saturating_add(ask.size)
        > quota.project_max_bytes;
    if over_project_files || over_project_bytes {
        return Ok(Some(Requested::OverProjectQuota));
    }
    Ok(None)
}

/// A request for a file this run hasn't asked about: check the quota, then either reuse the
/// project's verified copy (deduplication) or declare it and grant an upload.
async fn declare(
    tx: &mut sqlx::PgConnection,
    ask: &UploadAsk<'_>,
    scope: Scope,
    grant_ttl: Duration,
    quota: Quota,
) -> Result<Requested> {
    let class = ask.class.as_str();
    if let Some(refused) = over_quota(tx, ask, quota).await? {
        return Ok(refused);
    }

    // The project already holds these bytes under this class (verified for it): no upload.
    let known = sqlx::query!(
        "SELECT a.served_type AS \"served_type!\"
           FROM artifacts a
           JOIN blobs b ON b.project_id = a.project_id AND b.sha256 = a.sha256
          WHERE a.project_id = $1 AND a.class = $2 AND a.sha256 = decode($3, 'hex')
            AND a.state = 'available' AND b.state = 'present'
          LIMIT 1",
        ask.project_id,
        class,
        ask.sha256
    )
    .fetch_optional(&mut *tx)
    .await?;
    let (state, served) = match &known {
        Some(k) => ("available", Some(k.served_type.as_str())),
        None => ("declared", None),
    };
    // A step index past `i32` is no real step: it is stored as none rather than refused.
    let step = ask.step.and_then(|s| i32::try_from(s).ok());
    let inserted = sqlx::query!(
        "INSERT INTO artifacts
             (project_id, agent_run_id, job_id, suite_execution_id, flow_execution_id, step_index,
              class, content_type, served_type, size_bytes, sha256, path, name, retention_class,
              state, credential_id, grant_expires_at, available_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, decode($11, 'hex'), $12, $13, $14, $15,
                 $16, CASE WHEN $15 = 'declared' THEN now() + make_interval(secs => $17) END,
                 CASE WHEN $15 = 'available' THEN now() END)
         RETURNING id, grant_expires_at",
        ask.project_id,
        ask.agent_run_id,
        scope.job_id,
        scope.suite_execution_id,
        ask.flow_execution_id,
        step,
        class,
        ask.content_type,
        served,
        i64::try_from(ask.size).unwrap_or(i64::MAX),
        ask.sha256,
        ask.path,
        ask.name,
        default_retention(ask.class),
        state,
        ask.credential.id(),
        secs(grant_ttl)
    )
    .fetch_one(&mut *tx)
    .await?;
    if known.is_none() {
        return Ok(Requested::Grant {
            artifact_id: inserted.id,
            expires_at: inserted
                .grant_expires_at
                .unwrap_or_else(OffsetDateTime::now_utc),
        });
    }
    sqlx::query!(
        "UPDATE blobs SET unreferenced_since = NULL
          WHERE project_id = $1 AND sha256 = decode($2, 'hex')",
        ask.project_id,
        ask.sha256
    )
    .execute(&mut *tx)
    .await?;
    set_retention(tx, inserted.id).await?;
    Ok(Requested::Exists {
        artifact_id: inserted.id,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use sha2::{Digest, Sha256};
    use sqlx::PgPool;

    use super::*;
    use crate::{
        auth::Publisher,
        ingest::{
            RunHandle, Transport,
            tests::{Agent, SuiteIds, publisher, suite_run},
        },
    };

    /// The SHA-256 of `content`, lowercase hex.
    fn hex_of(content: &str) -> String {
        use std::fmt::Write as _;
        Sha256::digest(content.as_bytes())
            .iter()
            .fold(String::new(), |mut hex, b| {
                write!(hex, "{b:02x}").unwrap();
                hex
            })
    }

    /// Uploads a failure screenshot of `flow` at `step` and returns its artifact.
    pub(crate) async fn upload_shot(
        store: &Store,
        publisher: &Publisher,
        handle: &RunHandle,
        flow: Uuid,
        step: Option<u32>,
        content: &str,
    ) -> Uuid {
        let sha = hex_of(content);
        let ask = UploadAsk {
            project_id: publisher.project_id,
            credential: publisher.credential,
            agent_run_id: handle.agent_run_id,
            flow_execution_id: Some(flow),
            class: ArtifactClass::FailureScreenshot,
            path: "login/screenshots/shot.png",
            name: "shot.png",
            content_type: "image/png",
            size: content.len() as u64,
            sha256: &sha,
            step,
        };
        let quota = Quota {
            max_bytes: 1 << 30,
            max_files: 100,
            project_max_bytes: 1 << 40,
            project_max_files: 1_000_000,
        };
        let Requested::Grant {
            artifact_id: id, ..
        } = store
            .request_upload(&ask, Duration::from_mins(15), quota)
            .await
            .unwrap()
        else {
            panic!("a new screenshot should be granted");
        };
        let (project, credential) = (publisher.project_id, publisher.credential);
        assert_eq!(
            store.claim_content(project, credential, id).await.unwrap(),
            Claim::Claimed {
                size: content.len() as u64
            }
        );
        store.content_received(id).await.unwrap();
        store
            .mark_available(project, id, "image/png")
            .await
            .unwrap();
        id
    }

    /// The step an artifact was requested with is stored and listed with it; an artifact requested
    /// without one has none.
    #[sqlx::test(migrations = "../../migrations")]
    async fn an_artifact_requested_with_a_step_is_listed_with_it(pool: PgPool) {
        let store = Store {
            pool,
            origin: uuid::Uuid::now_v7(),
        };
        let publisher = publisher(&store).await;
        let mut agent = Agent::new("1", "e2e");
        let handle = store
            .open_run(&publisher, &agent.hello(None), Transport::WebSocket)
            .await
            .unwrap()
            .handle;
        let events = suite_run(&mut agent, SuiteIds::new(), &[("login", false)]);
        store.apply_batch(&handle, &events[..4]).await.unwrap();
        let flow = sqlx::query_scalar!(
            "SELECT id FROM flow_executions WHERE agent_run_id = $1",
            handle.agent_run_id
        )
        .fetch_one(store.pool())
        .await
        .unwrap();

        let at_step = upload_shot(&store, &publisher, &handle, flow, Some(2), "at step").await;
        let unlinked = upload_shot(&store, &publisher, &handle, flow, None, "unlinked").await;

        let rows = store
            .flow_artifacts(publisher.project_id, flow)
            .await
            .unwrap()
            .unwrap();
        let step_of = |id: Uuid| rows.iter().find(|r| r.id == id).map(|r| r.step);
        assert_eq!(step_of(at_step), Some(Some(2)));
        assert_eq!(step_of(unlinked), Some(None));
    }

    /// Maestro's own screenshots and recordings (issue #133, migration 0033) are storable classes,
    /// and each keeps the `user_media` retention class, which every project has from its creation.
    #[sqlx::test(migrations = "../../migrations")]
    async fn the_user_media_classes_are_storable_under_their_own_retention(pool: PgPool) {
        let store = Store {
            pool,
            origin: uuid::Uuid::now_v7(),
        };
        let publisher = publisher(&store).await;
        let mut agent = Agent::new("1", "e2e");
        let handle = store
            .open_run(&publisher, &agent.hello(None), Transport::WebSocket)
            .await
            .unwrap()
            .handle;
        let events = suite_run(&mut agent, SuiteIds::new(), &[("login", false)]);
        store.apply_batch(&handle, &events[..4]).await.unwrap();
        let flow = sqlx::query_scalar!(
            "SELECT id FROM flow_executions WHERE agent_run_id = $1",
            handle.agent_run_id
        )
        .fetch_one(store.pool())
        .await
        .unwrap();
        let quota = Quota {
            max_bytes: 1 << 30,
            max_files: 100,
            project_max_bytes: 1 << 40,
            project_max_files: 1_000_000,
        };
        let media = [
            (
                ArtifactClass::UserScreenshot,
                "login/takeScreenshot/fleet-main.png",
                "image/png",
                "png bytes",
            ),
            (
                ArtifactClass::UserRecording,
                "login/startRecording/fleet-rec.mp4",
                "video/mp4",
                "mp4 bytes",
            ),
        ];
        for (class, path, content_type, content) in media {
            let sha = hex_of(content);
            let ask = UploadAsk {
                project_id: publisher.project_id,
                credential: publisher.credential,
                agent_run_id: handle.agent_run_id,
                flow_execution_id: Some(flow),
                class,
                path,
                name: path.rsplit('/').next().unwrap_or(path),
                content_type,
                size: content.len() as u64,
                sha256: &sha,
                step: Some(4),
            };
            let granted = store
                .request_upload(&ask, Duration::from_mins(15), quota)
                .await;
            let Ok(Requested::Grant { artifact_id, .. }) = granted else {
                panic!("{class:?} was refused: {granted:?}");
            };
            let retention: String =
                sqlx::query_scalar("SELECT retention_class FROM artifacts WHERE id = $1")
                    .bind(artifact_id)
                    .fetch_one(store.pool())
                    .await
                    .unwrap();
            assert_eq!(retention, "user_media", "{class:?}");
        }
        assert_eq!(
            default_retention(ArtifactClass::UserScreenshot),
            "user_media"
        );
        assert_eq!(
            default_retention(ArtifactClass::UserRecording),
            "user_media"
        );
        // Every project has the class's retention window, from its creation (the seeding trigger).
        let seeded: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM retention_classes WHERE class = 'user_media' AND project_id = $1",
        )
        .bind(publisher.project_id)
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert_eq!(seeded, 1);
    }

    /// After every migration, the class CHECK allows both `failure_hierarchy` (0015) and
    /// `device_logcat` (0016): a later migration that rewrites the list without one of them makes
    /// that class's upload fail.
    #[sqlx::test(migrations = "../../migrations")]
    async fn failure_hierarchy_and_device_logcat_are_both_storable(pool: PgPool) {
        let store = Store {
            pool,
            origin: uuid::Uuid::now_v7(),
        };
        let publisher = publisher(&store).await;
        let mut agent = Agent::new("1", "e2e");
        let handle = store
            .open_run(&publisher, &agent.hello(None), Transport::WebSocket)
            .await
            .unwrap()
            .handle;
        let events = suite_run(&mut agent, SuiteIds::new(), &[("login", false)]);
        store.apply_batch(&handle, &events[..4]).await.unwrap();
        let flow = sqlx::query_scalar!(
            "SELECT id FROM flow_executions WHERE agent_run_id = $1",
            handle.agent_run_id
        )
        .fetch_one(store.pool())
        .await
        .unwrap();
        let quota = Quota {
            max_bytes: 1 << 30,
            max_files: 100,
            project_max_bytes: 1 << 40,
            project_max_files: 1_000_000,
        };
        for (class, name, content_type, content) in [
            (
                ArtifactClass::FailureHierarchy,
                "step-001-tapOn-Pay.json",
                "application/json",
                "{}",
            ),
            (
                ArtifactClass::DeviceLogcat,
                "device-logcat.txt",
                "text/plain",
                "I/Test: line\n",
            ),
        ] {
            let sha = hex_of(content);
            let ask = UploadAsk {
                project_id: publisher.project_id,
                credential: publisher.credential,
                agent_run_id: handle.agent_run_id,
                flow_execution_id: Some(flow),
                class,
                path: name,
                name,
                content_type,
                size: content.len() as u64,
                sha256: &sha,
                step: Some(1),
            };
            let stored = store
                .request_upload(&ask, Duration::from_mins(15), quota)
                .await;
            assert!(
                matches!(stored, Ok(Requested::Grant { .. })),
                "{class:?} was refused: {stored:?}"
            );
        }
    }
}

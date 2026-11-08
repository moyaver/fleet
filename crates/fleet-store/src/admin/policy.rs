// SPDX-License-Identifier: AGPL-3.0-only
//! Screen G's per-project policy (issue #139): the retention window of each class, the artifact
//! overrides (whether a class is accepted, its largest file, the daily quota) and the metric
//! sampling intervals.
//!
//! The configuration sets the server-wide values. A project's override here replaces them for that
//! project; a `NULL` override means "the configured value". The server checks that an artifact
//! override does not exceed a server-wide cap before it calls in. Every change runs in one
//! transaction with its audit event, as in [`super::write`]: a refused change writes nothing.

use serde_json::{Value, json};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use super::write::{Change, Refusal, WriteResult, record, refused};
use crate::{Result, Store, auth::AuditActor};

/// The artifact classes a project may override, as the configuration names them.
pub const ARTIFACT_CLASSES: [&str; 7] = [
    "failure_screenshot",
    "failure_hierarchy",
    "commands",
    "junit",
    "maestro_log",
    "html_report",
    "device_logcat",
];

/// The retention classes (`retention_classes`).
pub const RETENTION_CLASSES: [&str; 6] = [
    "report",
    "failure_evidence",
    "success_evidence",
    "debug_log",
    "video",
    "bundle",
];

/// Longest retention window a person may set, in days (about a century).
pub const MAX_RETENTION_DAYS: u32 = 36_500;

/// Shortest and longest metric sampling interval, in milliseconds. The agent clamps to the same
/// range.
pub const SAMPLE_INTERVAL_MS: std::ops::RangeInclusive<i32> = 1_000..=300_000;

/// One overridden artifact class of a project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassOverride {
    /// The artifact class.
    pub class: String,
    /// Whether the project accepts it.
    pub accepted: bool,
    /// The project's largest file for it, in bytes; `None` is the configured cap.
    pub max_bytes: Option<i64>,
}

/// A project's overrides. A `None` (or a class not listed) means the configured value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectOverrides {
    /// The project.
    pub project: Uuid,
    /// Its slug.
    pub slug: String,
    /// The classes it overrides.
    pub classes: Vec<ClassOverride>,
    /// Most bytes it may declare in a day.
    pub bytes_per_day: Option<i64>,
    /// Most files it may declare in a day.
    pub files_per_day: Option<i32>,
    /// Metric sampling interval while the host is calm, in milliseconds.
    pub sample_interval_ms: Option<i32>,
    /// Metric sampling interval under host pressure, in milliseconds.
    pub pressure_sample_interval_ms: Option<i32>,
}

impl ProjectOverrides {
    /// The override for `class`, if the project has one.
    #[must_use]
    pub fn class(&self, class: &str) -> Option<&ClassOverride> {
        self.classes.iter().find(|c| c.class == class)
    }
}

/// The state of one artifact class: a missing row is accepted with the configured cap.
fn class_state(row: Option<(bool, Option<i64>)>) -> (bool, Option<i64>) {
    row.unwrap_or((true, None))
}

fn class_json(class: &str, (accepted, max_bytes): (bool, Option<i64>)) -> Value {
    json!({ "class": class, "accepted": accepted, "max_bytes": max_bytes })
}

/// A retention window as the audit trail shows it: whole days, or `null` for ever.
fn ttl_days_json(seconds: Option<i64>) -> Value {
    seconds.map_or(Value::Null, |s| json!(s / 86_400))
}

/// Locks a project row for a change and returns its slug.
async fn lock_project(tx: &mut Transaction<'_, Postgres>, project: Uuid) -> WriteResult<String> {
    let slug = sqlx::query_scalar!(
        "SELECT slug FROM projects WHERE id = $1 FOR UPDATE",
        project
    )
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| refused(Refusal::NotFound("project")))?;
    Ok(slug)
}

fn check_class(class: &str) -> WriteResult<()> {
    if ARTIFACT_CLASSES.contains(&class) {
        Ok(())
    } else {
        Err(refused(Refusal::Invalid(format!(
            "{class} is not an artifact class"
        ))))
    }
}

fn check_bytes(value: Option<i64>, what: &str) -> WriteResult<()> {
    match value {
        Some(v) if v <= 0 => Err(refused(Refusal::Invalid(format!(
            "{what} must be above 0 bytes, or empty for the configured value"
        )))),
        _ => Ok(()),
    }
}

fn check_interval(value: Option<i32>, what: &str) -> WriteResult<()> {
    match value {
        Some(v) if !SAMPLE_INTERVAL_MS.contains(&v) => Err(refused(Refusal::Invalid(format!(
            "{what} must be between 1 and 300 seconds"
        )))),
        _ => Ok(()),
    }
}

impl Store {
    /// Every project's overrides, by slug.
    pub async fn admin_project_overrides(&self) -> Result<Vec<ProjectOverrides>> {
        let projects = sqlx::query!(
            "SELECT id, slug, artifact_bytes_per_day, artifact_files_per_day,
                    sample_interval_ms, pressure_sample_interval_ms
               FROM projects ORDER BY slug"
        )
        .fetch_all(self.pool())
        .await?;
        let classes = sqlx::query!(
            "SELECT project_id, class, accepted, max_bytes FROM project_artifact_classes
              ORDER BY class"
        )
        .fetch_all(self.pool())
        .await?;
        Ok(projects
            .into_iter()
            .map(|p| ProjectOverrides {
                project: p.id,
                slug: p.slug,
                classes: classes
                    .iter()
                    .filter(|c| c.project_id == p.id)
                    .map(|c| ClassOverride {
                        class: c.class.clone(),
                        accepted: c.accepted,
                        max_bytes: c.max_bytes,
                    })
                    .collect(),
                bytes_per_day: p.artifact_bytes_per_day,
                files_per_day: p.artifact_files_per_day,
                sample_interval_ms: p.sample_interval_ms,
                pressure_sample_interval_ms: p.pressure_sample_interval_ms,
            })
            .collect())
    }

    /// One project's overrides, read where uploads are granted and where the agent's limits are
    /// set. A project that does not exist has none.
    pub async fn project_overrides(&self, project: Uuid) -> Result<ProjectOverrides> {
        let p = sqlx::query!(
            "SELECT slug, artifact_bytes_per_day, artifact_files_per_day,
                    sample_interval_ms, pressure_sample_interval_ms
               FROM projects WHERE id = $1",
            project
        )
        .fetch_optional(self.pool())
        .await?;
        let Some(p) = p else {
            return Ok(ProjectOverrides {
                project,
                slug: String::new(),
                classes: Vec::new(),
                bytes_per_day: None,
                files_per_day: None,
                sample_interval_ms: None,
                pressure_sample_interval_ms: None,
            });
        };
        let classes = sqlx::query!(
            "SELECT class, accepted, max_bytes FROM project_artifact_classes
              WHERE project_id = $1 ORDER BY class",
            project
        )
        .fetch_all(self.pool())
        .await?;
        Ok(ProjectOverrides {
            project,
            slug: p.slug,
            classes: classes
                .into_iter()
                .map(|c| ClassOverride {
                    class: c.class,
                    accepted: c.accepted,
                    max_bytes: c.max_bytes,
                })
                .collect(),
            bytes_per_day: p.artifact_bytes_per_day,
            files_per_day: p.artifact_files_per_day,
            sample_interval_ms: p.sample_interval_ms,
            pressure_sample_interval_ms: p.pressure_sample_interval_ms,
        })
    }

    /// Sets a retention window: whole days, or `None` to keep the class for ever. It applies to
    /// artifacts that become available afterwards (their `expires_at` is fixed then).
    pub async fn set_retention(
        &self,
        actor: AuditActor,
        project: Uuid,
        class: &str,
        ttl_days: Option<u32>,
    ) -> WriteResult<()> {
        if !RETENTION_CLASSES.contains(&class) {
            return Err(refused(Refusal::Invalid(format!(
                "{class} is not a retention class"
            ))));
        }
        if ttl_days.is_some_and(|d| d > MAX_RETENTION_DAYS) {
            return Err(refused(Refusal::Invalid(format!(
                "a retention window is at most {MAX_RETENTION_DAYS} days, or 0 to keep for ever"
            ))));
        }
        let mut tx = self.pool().begin().await?;
        lock_project(&mut tx, project).await?;
        let old = sqlx::query!(
            "SELECT EXTRACT(EPOCH FROM ttl)::bigint AS ttl_seconds
               FROM retention_classes WHERE project_id = $1 AND class = $2 FOR UPDATE",
            project,
            class
        )
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| refused(Refusal::NotFound("retention class")))?;
        let new_seconds = ttl_days.map(|d| i64::from(d) * 86_400);
        if old.ttl_seconds == new_seconds {
            return Err(refused(Refusal::Conflict(
                "the class already keeps artifacts for that long".into(),
            )));
        }
        sqlx::query!(
            "UPDATE retention_classes SET ttl = make_interval(days => $3)
              WHERE project_id = $1 AND class = $2",
            project,
            class,
            ttl_days.map(|d| i32::try_from(d).unwrap_or(i32::MAX))
        )
        .execute(&mut *tx)
        .await?;
        record(
            &mut tx,
            Change {
                actor,
                project: Some(project),
                action: "retention.set",
                target_kind: "retention_class",
                target_id: None,
            },
            Some(json!({ "class": class, "ttl_days": ttl_days_json(old.ttl_seconds) })),
            Some(json!({ "class": class, "ttl_days": ttl_days_json(new_seconds) })),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Sets whether a project accepts one artifact class and its largest file. A class that is
    /// accepted with no cap is the configured one, so its override is removed.
    pub async fn set_artifact_class(
        &self,
        actor: AuditActor,
        project: Uuid,
        class: &str,
        accepted: bool,
        max_bytes: Option<i64>,
    ) -> WriteResult<()> {
        check_class(class)?;
        check_bytes(max_bytes, "the largest file")?;
        let mut tx = self.pool().begin().await?;
        lock_project(&mut tx, project).await?;
        let old = sqlx::query!(
            "SELECT accepted, max_bytes FROM project_artifact_classes
              WHERE project_id = $1 AND class = $2 FOR UPDATE",
            project,
            class
        )
        .fetch_optional(&mut *tx)
        .await?
        .map(|r| (r.accepted, r.max_bytes));
        let before = class_state(old);
        let after = (accepted, max_bytes);
        if before == after {
            return Err(refused(Refusal::Conflict(format!(
                "{class} already has that policy"
            ))));
        }
        if after == (true, None) {
            sqlx::query!(
                "DELETE FROM project_artifact_classes WHERE project_id = $1 AND class = $2",
                project,
                class
            )
            .execute(&mut *tx)
            .await?;
        } else {
            sqlx::query!(
                "INSERT INTO project_artifact_classes (project_id, class, accepted, max_bytes)
                 VALUES ($1, $2, $3, $4)
                 ON CONFLICT (project_id, class)
                 DO UPDATE SET accepted = EXCLUDED.accepted, max_bytes = EXCLUDED.max_bytes",
                project,
                class,
                accepted,
                max_bytes
            )
            .execute(&mut *tx)
            .await?;
        }
        record(
            &mut tx,
            Change {
                actor,
                project: Some(project),
                action: "artifact_class.set",
                target_kind: "artifact_class",
                target_id: None,
            },
            Some(class_json(class, before)),
            Some(class_json(class, after)),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Sets a project's daily artifact quota: bytes and files declared in 24 hours. `None` is the
    /// configured value.
    pub async fn set_artifact_quota(
        &self,
        actor: AuditActor,
        project: Uuid,
        bytes_per_day: Option<i64>,
        files_per_day: Option<i32>,
    ) -> WriteResult<()> {
        check_bytes(bytes_per_day, "the daily bytes")?;
        if files_per_day.is_some_and(|f| f <= 0) {
            return Err(refused(Refusal::Invalid(
                "the daily files must be above 0, or empty for the configured value".into(),
            )));
        }
        let mut tx = self.pool().begin().await?;
        lock_project(&mut tx, project).await?;
        let old = sqlx::query!(
            "SELECT artifact_bytes_per_day, artifact_files_per_day FROM projects
              WHERE id = $1 FOR UPDATE",
            project
        )
        .fetch_one(&mut *tx)
        .await?;
        if (old.artifact_bytes_per_day, old.artifact_files_per_day)
            == (bytes_per_day, files_per_day)
        {
            return Err(refused(Refusal::Conflict(
                "the daily quota already has those values".into(),
            )));
        }
        sqlx::query!(
            "UPDATE projects SET artifact_bytes_per_day = $2, artifact_files_per_day = $3
              WHERE id = $1",
            project,
            bytes_per_day,
            files_per_day
        )
        .execute(&mut *tx)
        .await?;
        record(
            &mut tx,
            Change {
                actor,
                project: Some(project),
                action: "artifact_quota.set",
                target_kind: "project",
                target_id: Some(project),
            },
            Some(json!({ "bytes_per_day": old.artifact_bytes_per_day,
                         "files_per_day": old.artifact_files_per_day })),
            Some(json!({ "bytes_per_day": bytes_per_day, "files_per_day": files_per_day })),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Sets a project's metric sampling intervals, in milliseconds (1 s to 300 s). `None` is the
    /// configured value. Agents pick them up when their next welcome arrives.
    pub async fn set_metric_policy(
        &self,
        actor: AuditActor,
        project: Uuid,
        sample_interval_ms: Option<i32>,
        pressure_sample_interval_ms: Option<i32>,
    ) -> WriteResult<()> {
        check_interval(sample_interval_ms, "the sampling interval")?;
        check_interval(pressure_sample_interval_ms, "the interval under pressure")?;
        let mut tx = self.pool().begin().await?;
        lock_project(&mut tx, project).await?;
        let old = sqlx::query!(
            "SELECT sample_interval_ms, pressure_sample_interval_ms FROM projects
              WHERE id = $1 FOR UPDATE",
            project
        )
        .fetch_one(&mut *tx)
        .await?;
        if (old.sample_interval_ms, old.pressure_sample_interval_ms)
            == (sample_interval_ms, pressure_sample_interval_ms)
        {
            return Err(refused(Refusal::Conflict(
                "the metric collection already has those intervals".into(),
            )));
        }
        sqlx::query!(
            "UPDATE projects SET sample_interval_ms = $2, pressure_sample_interval_ms = $3
              WHERE id = $1",
            project,
            sample_interval_ms,
            pressure_sample_interval_ms
        )
        .execute(&mut *tx)
        .await?;
        record(
            &mut tx,
            Change {
                actor,
                project: Some(project),
                action: "metric_policy.set",
                target_kind: "project",
                target_id: Some(project),
            },
            Some(json!({ "sample_interval_ms": old.sample_interval_ms,
                         "pressure_sample_interval_ms": old.pressure_sample_interval_ms })),
            Some(json!({ "sample_interval_ms": sample_interval_ms,
                         "pressure_sample_interval_ms": pressure_sample_interval_ms })),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }
}

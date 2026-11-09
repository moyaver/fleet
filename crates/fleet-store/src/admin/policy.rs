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
pub const ARTIFACT_CLASSES: [&str; 9] = [
    "failure_screenshot",
    "failure_hierarchy",
    "commands",
    "junit",
    "runner_log",
    "html_report",
    "device_logcat",
    "user_screenshot",
    "user_recording",
];

/// The retention classes (`retention_classes`).
pub const RETENTION_CLASSES: [&str; 7] = [
    "report",
    "failure_evidence",
    "success_evidence",
    "debug_log",
    "video",
    "bundle",
    "user_media",
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

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::*;
    use crate::admin::write::WriteError;
    use crate::{
        artifacts::tests::upload_shot,
        ingest::{
            Transport,
            tests::{Agent, SuiteIds, publisher, suite_run},
        },
    };
    use time::{Duration, OffsetDateTime};

    fn store(pool: PgPool) -> Store {
        Store {
            pool,
            origin: Uuid::now_v7(),
        }
    }

    /// Every audit event of `action`, oldest first: its `(before, after)` as the trail holds them.
    async fn trail(store: &Store, action: &str) -> Vec<(Value, Value)> {
        sqlx::query_as::<_, (Value, Value)>(
            "SELECT details -> 'before', details -> 'after' FROM audit_events
              WHERE action = $1 ORDER BY at, id",
        )
        .bind(action)
        .fetch_all(store.pool())
        .await
        .unwrap()
    }

    /// Every audit event, whatever its action.
    async fn audit_count(store: &Store) -> i64 {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM audit_events")
            .fetch_one(store.pool())
            .await
            .unwrap()
    }

    /// A retention window in seconds, `None` for ever.
    async fn ttl_seconds(store: &Store, project: Uuid, class: &str) -> Option<i64> {
        sqlx::query_scalar::<_, Option<i64>>(
            "SELECT EXTRACT(EPOCH FROM ttl)::bigint FROM retention_classes
              WHERE project_id = $1 AND class = $2",
        )
        .bind(project)
        .bind(class)
        .fetch_one(store.pool())
        .await
        .unwrap()
    }

    fn is_refused<T: std::fmt::Debug>(res: WriteResult<T>) -> Refusal {
        match res {
            Err(WriteError::Refused(r)) => r,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    async fn dashboard_project(store: &Store, slug: &str) -> Uuid {
        store
            .create_project(AuditActor::System, slug, "Name")
            .await
            .unwrap()
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn a_retention_change_is_audited_with_the_old_and_new_whole_days(pool: PgPool) {
        let store = store(pool);
        let app = dashboard_project(&store, "app").await;
        // The seeded window of failure evidence is 90 days.
        store
            .set_retention(AuditActor::System, app, "failure_evidence", Some(3))
            .await
            .unwrap();
        assert_eq!(
            ttl_seconds(&store, app, "failure_evidence").await,
            Some(3 * 86_400)
        );
        let events = trail(&store, "retention.set").await;
        assert_eq!(events.len(), 1, "one event per change");
        assert_eq!(events[0].0["class"], "failure_evidence");
        assert_eq!(events[0].0["ttl_days"], 90);
        assert_eq!(events[0].1["ttl_days"], 3);

        // Keeping for ever is NULL, and the trail shows it as null.
        store
            .set_retention(AuditActor::System, app, "failure_evidence", None)
            .await
            .unwrap();
        assert_eq!(ttl_seconds(&store, app, "failure_evidence").await, None);
        let events = trail(&store, "retention.set").await;
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].0["ttl_days"], 3);
        assert!(events[1].1["ttl_days"].is_null());
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn a_refused_retention_change_writes_nothing_and_the_longest_window_is_allowed(
        pool: PgPool,
    ) {
        let store = store(pool);
        let app = dashboard_project(&store, "app").await;
        let before = audit_count(&store).await;

        let r = is_refused(
            store
                .set_retention(AuditActor::System, app, "nope", Some(3))
                .await,
        );
        assert!(matches!(r, Refusal::Invalid(_)), "{r:?}");
        let r = is_refused(
            store
                .set_retention(AuditActor::System, app, "report", Some(36_501))
                .await,
        );
        assert!(matches!(r, Refusal::Invalid(_)), "{r:?}");
        assert_eq!(
            audit_count(&store).await,
            before,
            "a refusal writes no event"
        );
        assert_eq!(
            ttl_seconds(&store, app, "report").await,
            Some(365 * 86_400),
            "a refusal changes no row"
        );

        // The same window again is refused: nothing to change, nothing audited.
        let r = is_refused(
            store
                .set_retention(AuditActor::System, app, "failure_evidence", Some(90))
                .await,
        );
        assert!(matches!(r, Refusal::Conflict(_)), "{r:?}");
        assert_eq!(audit_count(&store).await, before);

        // The longest window a person may set is accepted.
        store
            .set_retention(AuditActor::System, app, "report", Some(36_500))
            .await
            .unwrap();
        assert_eq!(
            ttl_seconds(&store, app, "report").await,
            Some(36_500 * 86_400)
        );
        assert_eq!(audit_count(&store).await, before + 1);
    }

    /// Maestro's own media (issue #133) has a per-project switch like any class, and its own
    /// `user_media` retention window (migration 0033).
    #[sqlx::test(migrations = "../../migrations")]
    async fn the_user_media_classes_and_their_retention_are_set_per_project(pool: PgPool) {
        let store = store(pool);
        let app = dashboard_project(&store, "app").await;
        store
            .set_artifact_class(AuditActor::System, app, "user_recording", false, None)
            .await
            .unwrap();
        assert_eq!(
            store
                .project_overrides(app)
                .await
                .unwrap()
                .class("user_recording"),
            Some(&ClassOverride {
                class: "user_recording".into(),
                accepted: false,
                max_bytes: None,
            })
        );
        store
            .set_artifact_class(
                AuditActor::System,
                app,
                "user_screenshot",
                true,
                Some(1_024),
            )
            .await
            .unwrap();
        assert!(
            store
                .set_artifact_class(AuditActor::System, app, "user_media", true, None)
                .await
                .is_err(),
            "a retention class is not an artifact class"
        );
        store
            .set_retention(AuditActor::System, app, "user_media", Some(3))
            .await
            .unwrap();
        assert_eq!(
            ttl_seconds(&store, app, "user_media").await,
            Some(3 * 86_400)
        );
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn an_artifact_class_policy_is_audited_and_its_row_removed_when_it_is_the_configured_one(
        pool: PgPool,
    ) {
        let store = store(pool);
        let app = dashboard_project(&store, "app").await;

        // Turned off: a row is written, and the trail holds the accepted state before.
        store
            .set_artifact_class(AuditActor::System, app, "commands", false, None)
            .await
            .unwrap();
        assert_eq!(
            store
                .project_overrides(app)
                .await
                .unwrap()
                .class("commands"),
            Some(&ClassOverride {
                class: "commands".into(),
                accepted: false,
                max_bytes: None,
            })
        );
        let events = trail(&store, "artifact_class.set").await;
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].0,
            json!({"class": "commands", "accepted": true, "max_bytes": null})
        );
        assert_eq!(
            events[0].1,
            json!({"class": "commands", "accepted": false, "max_bytes": null})
        );

        // Accepted with a cap: the cap is stored and audited.
        store
            .set_artifact_class(AuditActor::System, app, "junit", true, Some(1_024))
            .await
            .unwrap();
        let events = trail(&store, "artifact_class.set").await;
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[1].1,
            json!({"class": "junit", "accepted": true, "max_bytes": 1024})
        );

        // The same policy again is refused, and writes nothing.
        let before = audit_count(&store).await;
        let r = is_refused(
            store
                .set_artifact_class(AuditActor::System, app, "junit", true, Some(1_024))
                .await,
        );
        assert!(matches!(r, Refusal::Conflict(_)), "{r:?}");
        assert_eq!(audit_count(&store).await, before);

        // A cap of 0 or less, and a class that is not one, are refused.
        for bad in [0, -5] {
            let r = is_refused(
                store
                    .set_artifact_class(AuditActor::System, app, "junit", true, Some(bad))
                    .await,
            );
            assert!(matches!(r, Refusal::Invalid(_)), "{bad}: {r:?}");
        }
        let r = is_refused(
            store
                .set_artifact_class(AuditActor::System, app, "video", true, None)
                .await,
        );
        assert!(matches!(r, Refusal::Invalid(_)), "{r:?}");
        assert_eq!(audit_count(&store).await, before, "refusals write nothing");

        // Accepted with no cap is the configured policy: the row goes.
        store
            .set_artifact_class(AuditActor::System, app, "commands", true, None)
            .await
            .unwrap();
        assert!(
            store
                .project_overrides(app)
                .await
                .unwrap()
                .class("commands")
                .is_none()
        );
        let rows = sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM project_artifact_classes WHERE project_id = $1 AND class = 'commands'",
        )
        .bind(app)
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert_eq!(rows, 0);
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn the_daily_quota_is_audited_and_refused_when_it_is_not_a_positive_number(pool: PgPool) {
        let store = store(pool);
        let app = dashboard_project(&store, "app").await;
        store
            .set_artifact_quota(AuditActor::System, app, Some(1_000_000), Some(50))
            .await
            .unwrap();
        let events = trail(&store, "artifact_quota.set").await;
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].0,
            json!({"bytes_per_day": null, "files_per_day": null})
        );
        assert_eq!(
            events[0].1,
            json!({"bytes_per_day": 1_000_000, "files_per_day": 50})
        );
        let o = store.project_overrides(app).await.unwrap();
        assert_eq!(
            (o.bytes_per_day, o.files_per_day),
            (Some(1_000_000), Some(50))
        );

        let before = audit_count(&store).await;
        for (bytes, files) in [(Some(0), None), (None, Some(0)), (Some(-1), None)] {
            let r = is_refused(
                store
                    .set_artifact_quota(AuditActor::System, app, bytes, files)
                    .await,
            );
            assert!(
                matches!(r, Refusal::Invalid(_)),
                "{bytes:?} {files:?}: {r:?}"
            );
        }
        assert_eq!(audit_count(&store).await, before, "refusals write nothing");
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn the_metric_intervals_are_audited_and_must_be_between_1_and_300_seconds(pool: PgPool) {
        let store = store(pool);
        let app = dashboard_project(&store, "app").await;
        store
            .set_metric_policy(AuditActor::System, app, Some(2_000), Some(30_000))
            .await
            .unwrap();
        let events = trail(&store, "metric_policy.set").await;
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].0,
            json!({"sample_interval_ms": null, "pressure_sample_interval_ms": null})
        );
        assert_eq!(
            events[0].1,
            json!({"sample_interval_ms": 2_000, "pressure_sample_interval_ms": 30_000})
        );
        let o = store.project_overrides(app).await.unwrap();
        assert_eq!(
            (o.sample_interval_ms, o.pressure_sample_interval_ms),
            (Some(2_000), Some(30_000))
        );

        let before = audit_count(&store).await;
        for bad in [999, 300_001] {
            let r = is_refused(
                store
                    .set_metric_policy(AuditActor::System, app, Some(bad), None)
                    .await,
            );
            assert!(matches!(r, Refusal::Invalid(_)), "{bad} ms: {r:?}");
            let r = is_refused(
                store
                    .set_metric_policy(AuditActor::System, app, None, Some(bad))
                    .await,
            );
            assert!(
                matches!(r, Refusal::Invalid(_)),
                "{bad} ms under pressure: {r:?}"
            );
        }
        assert_eq!(audit_count(&store).await, before, "refusals write nothing");
        let o = store.project_overrides(app).await.unwrap();
        assert_eq!(
            (o.sample_interval_ms, o.pressure_sample_interval_ms),
            (Some(2_000), Some(30_000)),
            "a refused interval changes no row"
        );

        // The same intervals again are refused; the bounds themselves are allowed.
        let r = is_refused(
            store
                .set_metric_policy(AuditActor::System, app, Some(2_000), Some(30_000))
                .await,
        );
        assert!(matches!(r, Refusal::Conflict(_)), "{r:?}");
        store
            .set_metric_policy(AuditActor::System, app, Some(1_000), Some(300_000))
            .await
            .unwrap();
    }

    /// A file's `(available_at, expires_at)`, and its retention class.
    async fn expiry(store: &Store, artifact: Uuid) -> (OffsetDateTime, OffsetDateTime, String) {
        let (available, expires, class) =
            sqlx::query_as::<_, (Option<OffsetDateTime>, Option<OffsetDateTime>, String)>(
                "SELECT available_at, expires_at, retention_class FROM artifacts WHERE id = $1",
            )
            .bind(artifact)
            .fetch_one(store.pool())
            .await
            .unwrap();
        (
            available.expect("an available artifact has available_at"),
            expires.expect("a finite window sets expires_at"),
            class,
        )
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn a_retention_change_sets_the_expiry_of_artifacts_that_become_available_after_it(
        pool: PgPool,
    ) {
        let store = store(pool);
        let publisher = publisher(&store).await;
        let app = publisher.project_id;
        let mut agent = Agent::new("1", "e2e");
        let handle = store
            .open_run(&publisher, &agent.hello(None), Transport::WebSocket)
            .await
            .unwrap()
            .handle;
        // A failed flow, so its screenshots are failure evidence once they are available.
        let events = suite_run(&mut agent, SuiteIds::new(), &[("login", false)]);
        store.apply_batch(&handle, &events).await.unwrap();
        let flow: Uuid =
            sqlx::query_scalar::<_, Uuid>("SELECT id FROM flow_executions WHERE agent_run_id = $1")
                .bind(handle.agent_run_id)
                .fetch_one(store.pool())
                .await
                .unwrap();

        // Before the change: failure evidence lasts the seeded 90 days.
        let before = upload_shot(&store, &publisher, &handle, flow, None, "before").await;
        let (old_available, old_expires, class) = expiry(&store, before).await;
        assert_eq!(class, "failure_evidence");
        assert_eq!(old_expires - old_available, Duration::days(90));

        store
            .set_retention(AuditActor::System, app, "failure_evidence", Some(3))
            .await
            .unwrap();

        // After the change: an artifact that becomes available now lives 3 days from then.
        let after = upload_shot(&store, &publisher, &handle, flow, None, "after").await;
        let (available, expires, class) = expiry(&store, after).await;
        assert_eq!(class, "failure_evidence");
        assert_eq!(expires - available, Duration::days(3));

        // The artifact that was already there keeps the expiry it was given.
        let (_, expires_still, _) = expiry(&store, before).await;
        assert_eq!(expires_still, old_expires);
    }
}

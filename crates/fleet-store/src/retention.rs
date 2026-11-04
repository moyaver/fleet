// SPDX-License-Identifier: AGPL-3.0-only
//! Retention of time-based data (PLAN §7.7, §9; issue #45).
//!
//! [`Store::drop_expired`] removes what has outlived its retention at a given time:
//!
//! - **Partitions** of the four daily-partitioned tables (raw samples, journal, steps) are
//!   detached and dropped once their *whole* UTC day is older than the retention, which costs
//!   nothing compared with deleting rows. A raw sample partition is only dropped when its samples
//!   have been rolled up (the rollup watermark is past its end and no late-data mark points into
//!   it), or when it is empty: retention never destroys telemetry the rollups have not seen.
//! - **Rollup rows** (not partitioned) and **audit events** are deleted by age.
//!
//! `None` keeps a kind for ever. Time is a parameter, so tests control it.

use std::time::Duration;

use sqlx::PgPool;
use time::{Date, OffsetDateTime};

use crate::{
    Result, Store,
    partitions::{self, Table},
};

/// How long each kind of data is kept; `None` keeps it for ever.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retention {
    /// Raw 5 s telemetry samples.
    pub raw_samples: Option<Duration>,
    /// One-minute rollups.
    pub rollup_1m: Option<Duration>,
    /// One-hour rollups.
    pub rollup_1h: Option<Duration>,
    /// The raw agent journal.
    pub journal: Option<Duration>,
    /// Step executions.
    pub steps: Option<Duration>,
    /// The audit log.
    pub audit: Option<Duration>,
}

impl Default for Retention {
    fn default() -> Self {
        let days = |d: u64| Some(Duration::from_secs(d * 86_400));
        Self {
            raw_samples: days(7),
            rollup_1m: days(90),
            rollup_1h: days(365),
            journal: days(14),
            steps: days(14),
            audit: days(365),
        }
    }
}

/// What one retention pass removed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExpiredReport {
    /// Daily partitions dropped.
    pub partitions: u32,
    /// Expired raw partitions kept because their samples are not rolled up yet.
    pub held_back: u32,
    /// Rollup rows deleted.
    pub rollup_rows: u64,
    /// Audit events deleted.
    pub audit_events: u64,
}

impl Store {
    /// Drops what has outlived its retention at `now`.
    pub async fn drop_expired(
        &self,
        now: OffsetDateTime,
        retention: &Retention,
    ) -> Result<ExpiredReport> {
        let pool = self.pool();
        let mut report = ExpiredReport::default();
        let tables = [
            (
                Table::HostSamples,
                retention.raw_samples,
                Some("host_1m"),
                "host",
            ),
            (
                Table::DeviceSamples,
                retention.raw_samples,
                Some("device_1m"),
                "device",
            ),
            (Table::AgentEvents, retention.journal, None, ""),
            (Table::StepExecutions, retention.steps, None, ""),
        ];
        for (table, keep, rollup, source) in tables {
            let Some(keep) = keep else { continue };
            let cutoff = now - keep;
            for day in partitions::days(pool, table).await? {
                if partitions::day_start(day) + time::Duration::DAY > cutoff {
                    break;
                }
                if let Some(name) = rollup
                    && !rolled_up(pool, table, day, name, source).await?
                {
                    tracing::warn!(
                        table = table.name(), %day,
                        "keeping an expired partition: its samples are not rolled up yet"
                    );
                    report.held_back += 1;
                    continue;
                }
                partitions::drop_day(pool, table, day).await?;
                tracing::info!(table = table.name(), %day, "dropped an expired partition");
                report.partitions += 1;
            }
        }
        if let Some(keep) = retention.rollup_1m {
            let cutoff = now - keep;
            report.rollup_rows +=
                sqlx::query!("DELETE FROM host_rollup_1m WHERE bucket < $1", cutoff)
                    .execute(pool)
                    .await?
                    .rows_affected();
            report.rollup_rows +=
                sqlx::query!("DELETE FROM device_rollup_1m WHERE bucket < $1", cutoff)
                    .execute(pool)
                    .await?
                    .rows_affected();
        }
        if let Some(keep) = retention.rollup_1h {
            let cutoff = now - keep;
            report.rollup_rows +=
                sqlx::query!("DELETE FROM host_rollup_1h WHERE bucket < $1", cutoff)
                    .execute(pool)
                    .await?
                    .rows_affected();
            report.rollup_rows +=
                sqlx::query!("DELETE FROM device_rollup_1h WHERE bucket < $1", cutoff)
                    .execute(pool)
                    .await?
                    .rows_affected();
        }
        if let Some(keep) = retention.audit {
            report.audit_events +=
                sqlx::query!("DELETE FROM audit_events WHERE at < $1", now - keep)
                    .execute(pool)
                    .await?
                    .rows_affected();
        }
        Ok(report)
    }
}

/// Whether the partition's samples are safe to drop: it is empty, or the rollups have passed the
/// end of its day and no late sample of that day is waiting to be rolled up.
async fn rolled_up(
    pool: &PgPool,
    table: Table,
    day: Date,
    watermark: &str,
    source: &str,
) -> Result<bool> {
    let end = partitions::day_start(day) + time::Duration::DAY;
    let reached = sqlx::query_scalar!(
        "SELECT watermark >= $2 FROM rollup_watermarks WHERE name = $1",
        watermark,
        end
    )
    .fetch_optional(pool)
    .await?
    .flatten()
    .unwrap_or(false);
    if reached {
        let pending = sqlx::query_scalar!(
            "SELECT EXISTS (SELECT 1 FROM rollup_dirty WHERE source = $1 AND bucket < $2)",
            source,
            end
        )
        .fetch_one(pool)
        .await?
        .unwrap_or(false);
        if !pending {
            return Ok(true);
        }
    }
    partitions::is_empty(pool, table, day).await
}

#[cfg(test)]
mod tests {
    use time::Duration;
    use uuid::Uuid;

    use super::*;

    fn today() -> Date {
        OffsetDateTime::now_utc().date()
    }

    async fn make_partitions(store: &Store, table: &str, days_ago: &[i64]) {
        for n in days_ago {
            let day = today() - Duration::days(*n);
            sqlx::query("SELECT fleet_ensure_partitions($1::text::regclass, $2, $2)")
                .bind(table)
                .bind(day)
                .execute(store.pool())
                .await
                .unwrap();
        }
    }

    async fn exists(store: &Store, table: &str, days_ago: i64) -> bool {
        let day = today() - Duration::days(days_ago);
        let name = format!(
            "{table}_p{:04}{:02}{:02}",
            day.year(),
            u8::from(day.month()),
            day.day()
        );
        sqlx::query_scalar::<_, bool>("SELECT to_regclass($1) IS NOT NULL")
            .bind(name)
            .fetch_one(store.pool())
            .await
            .unwrap()
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn partitions_older_than_their_retention_are_dropped_and_newer_ones_kept(pool: PgPool) {
        let store = Store { pool };
        // Samples: 7 days. The journal and steps: 14 days. A partition goes once its whole day
        // is older than the retention: with `now` at noon, the day 8 days back ended 7 days and
        // 12 hours ago; the day 7 days back ended 6 days and 12 hours ago.
        make_partitions(&store, "host_samples", &[30, 8, 7, 3]).await;
        make_partitions(&store, "device_samples", &[30, 8, 7, 3]).await;
        make_partitions(&store, "agent_events", &[30, 15, 14, 3]).await;
        make_partitions(&store, "step_executions", &[30, 15, 14, 3]).await;
        let now = today().with_hms(12, 0, 0).unwrap().assume_utc();

        let report = store
            .drop_expired(now, &Retention::default())
            .await
            .unwrap();

        for table in ["host_samples", "device_samples"] {
            assert!(!exists(&store, table, 30).await, "{table} 30 d");
            assert!(!exists(&store, table, 8).await, "{table} 8 d");
            assert!(exists(&store, table, 7).await, "{table} 7 d");
            assert!(exists(&store, table, 3).await, "{table} 3 d");
        }
        for table in ["agent_events", "step_executions"] {
            assert!(!exists(&store, table, 30).await, "{table} 30 d");
            assert!(!exists(&store, table, 15).await, "{table} 15 d");
            assert!(exists(&store, table, 14).await, "{table} 14 d");
            assert!(exists(&store, table, 3).await, "{table} 3 d");
        }
        // Today's and the days ahead (created by the migrations) are untouched.
        assert!(exists(&store, "host_samples", 0).await);
        assert!(exists(&store, "agent_events", -7).await);
        assert_eq!(report.partitions, 8);
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn raw_samples_that_are_not_rolled_up_yet_outlive_their_retention(pool: PgPool) {
        let store = Store { pool };
        let project = store.upsert_project("app", "App").await.unwrap();
        make_partitions(&store, "host_samples", &[8]).await;
        let at = (today() - Duration::days(8))
            .with_hms(12, 0, 0)
            .unwrap()
            .assume_utc();
        sqlx::query!(
            "INSERT INTO host_samples (ts, agent_run_id, project_id, tseq, host_cpu_pct)
             VALUES ($1, $2, $3, 1, 42)",
            at,
            Uuid::now_v7(),
            project
        )
        .execute(store.pool())
        .await
        .unwrap();
        let now = today().with_hms(12, 0, 0).unwrap().assume_utc();

        let first = store
            .drop_expired(now, &Retention::default())
            .await
            .unwrap();
        assert!(
            exists(&store, "host_samples", 8).await,
            "kept: not rolled up"
        );
        assert_eq!((first.partitions, first.held_back), (0, 1));

        store.roll_up(now).await.unwrap();
        let second = store
            .drop_expired(now, &Retention::default())
            .await
            .unwrap();

        assert!(
            !exists(&store, "host_samples", 8).await,
            "dropped once rolled up"
        );
        assert_eq!((second.partitions, second.held_back), (1, 0));
        // The minute and hour rows outlive the raw samples.
        let rolled = sqlx::query_scalar!(
            "SELECT (SELECT count(*) FROM host_rollup_1m) + (SELECT count(*) FROM host_rollup_1h)"
        )
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert_eq!(rolled, Some(2));
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn rollups_and_audit_events_expire_by_their_own_retention(pool: PgPool) {
        let store = Store { pool };
        let project = store.upsert_project("app", "App").await.unwrap();
        let now = OffsetDateTime::now_utc();
        let run = Uuid::now_v7();
        // (age in days, expected to survive): 1 min rows live 90 d, 1 h rows 365 d, audit 365 d.
        for (table, ages) in [
            ("host_rollup_1m", [(91, false), (89, true)]),
            ("host_rollup_1h", [(366, false), (364, true)]),
        ] {
            for (age, _) in ages {
                sqlx::query(sqlx::AssertSqlSafe(format!(
                    "INSERT INTO {table} (bucket, agent_run_id, project_id, n) VALUES ($1, $2, $3, 1)"
                )))
                .bind(now - Duration::days(age))
                .bind(run)
                .bind(project)
                .execute(store.pool())
                .await
                .unwrap();
            }
        }
        for age in [366, 364] {
            sqlx::query!(
                "INSERT INTO audit_events (at, actor_kind, action) VALUES ($1, 'system', 'x')",
                now - Duration::days(age)
            )
            .execute(store.pool())
            .await
            .unwrap();
        }

        let report = store
            .drop_expired(now, &Retention::default())
            .await
            .unwrap();

        let ages = |rows: Vec<OffsetDateTime>| {
            let mut a: Vec<i64> = rows.iter().map(|b| (now - *b).whole_days()).collect();
            a.sort_unstable();
            a
        };
        let minutes = sqlx::query_scalar!("SELECT bucket FROM host_rollup_1m")
            .fetch_all(store.pool())
            .await
            .unwrap();
        let hours = sqlx::query_scalar!("SELECT bucket FROM host_rollup_1h")
            .fetch_all(store.pool())
            .await
            .unwrap();
        let audit = sqlx::query_scalar!("SELECT at FROM audit_events")
            .fetch_all(store.pool())
            .await
            .unwrap();
        assert_eq!(ages(minutes), [89]);
        assert_eq!(ages(hours), [364]);
        assert_eq!(ages(audit), [364]);
        assert_eq!((report.rollup_rows, report.audit_events), (2, 1));
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn no_retention_means_keep_everything(pool: PgPool) {
        let store = Store { pool };
        make_partitions(&store, "host_samples", &[400]).await;
        make_partitions(&store, "agent_events", &[400]).await;
        sqlx::query!("INSERT INTO audit_events (at, actor_kind, action) VALUES (now() - interval '5 years', 'system', 'x')")
            .execute(store.pool())
            .await
            .unwrap();
        let keep = Retention {
            raw_samples: None,
            rollup_1m: None,
            rollup_1h: None,
            journal: None,
            steps: None,
            audit: None,
        };

        let report = store
            .drop_expired(OffsetDateTime::now_utc(), &keep)
            .await
            .unwrap();

        assert!(exists(&store, "host_samples", 400).await);
        assert!(exists(&store, "agent_events", 400).await);
        assert_eq!(report, ExpiredReport::default());
    }
}

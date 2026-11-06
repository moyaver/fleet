// SPDX-License-Identifier: AGPL-3.0-only
//! Telemetry rollups: one-minute and one-hour aggregates of the raw samples (PLAN §7.7;
//! research 05 §10.2; issue #45).
//!
//! The job ([`Store::roll_up`]) is idempotent, so running it twice, concurrently, or after a
//! crash is always safe: it recomputes aggregates from raw rows and upserts them.
//!
//! - **Window.** Minutes older than two minutes are *closed*. A watermark per kind (host,
//!   device) records that everything before it is rolled up; each pass rolls `[watermark,
//!   cutoff)` in windows of at most six hours (so a long outage is caught up in bounded steps) and
//!   advances the watermark in the same transaction. The first pass starts at the oldest raw
//!   partition.
//! - **One hour** is computed from the raw rows too (not from the minute rows), so averages are
//!   exact, and its bucket is refreshed together with the minutes: the current hour is partial,
//!   covering the closed minutes so far.
//! - **Late data.** A sample inserted more than a minute after its time (a replay after a
//!   reconnect) marks its minute in `rollup_dirty` (a trigger, migration 0009). A pass takes the
//!   marks behind the watermark first and recomputes the span they cover. A mark is removed
//!   *before* its bucket is recomputed, so a sample committed meanwhile either is seen by the
//!   recomputation or keeps its own mark for the next pass; nothing is lost and nothing blocks
//!   ingest.
//! - **Unavailable stays unavailable.** An average over no readings is `NULL`, never zero.
//!   `n` counts samples, readings or not.
//!
//! Time comes in as a parameter, so tests control it.

use std::collections::BTreeMap;

use sqlx::PgPool;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use crate::{
    Result, Store,
    partitions::{self, Table},
};

/// Samples younger than this are not rolled up yet (late-arriving batches settle).
const LAG: Duration = Duration::minutes(2);
/// The longest window one transaction rolls up.
const WINDOW: Duration = Duration::hours(6);

/// What one rollup pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RollupReport {
    /// One-minute rows written or rewritten.
    pub minutes: u64,
    /// One-hour rows written or rewritten.
    pub hours: u64,
    /// Minute buckets recomputed because late samples arrived behind the watermark.
    pub late_buckets: u64,
}

impl RollupReport {
    fn add(&mut self, other: &Self) {
        self.minutes += other.minutes;
        self.hours += other.hours;
        self.late_buckets += other.late_buckets;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Host,
    Device,
}

impl Kind {
    fn watermark(self) -> &'static str {
        match self {
            Self::Host => "host_1m",
            Self::Device => "device_1m",
        }
    }

    fn source(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Device => "device",
        }
    }

    fn table(self) -> Table {
        match self {
            Self::Host => Table::HostSamples,
            Self::Device => Table::DeviceSamples,
        }
    }
}

fn floor(t: OffsetDateTime, step: Duration) -> OffsetDateTime {
    let step = step.whole_seconds();
    OffsetDateTime::from_unix_timestamp(t.unix_timestamp().div_euclid(step) * step)
        .unwrap_or(OffsetDateTime::UNIX_EPOCH)
}

fn ceil(t: OffsetDateTime, step: Duration) -> OffsetDateTime {
    let down = floor(t, step);
    if down == t { t } else { down + step }
}

impl Store {
    /// Rolls up every closed minute at `now` into the one-minute and one-hour tables, and
    /// recomputes what late samples touched. Safe to repeat and to run beside ingest.
    pub async fn roll_up(&self, now: OffsetDateTime) -> Result<RollupReport> {
        let cutoff = floor(now - LAG, Duration::MINUTE);
        let mut report = RollupReport::default();
        for kind in [Kind::Host, Kind::Device] {
            report.add(&roll_kind(self.pool(), kind, cutoff).await?);
        }
        Ok(report)
    }
}

async fn roll_kind(pool: &PgPool, kind: Kind, cutoff: OffsetDateTime) -> Result<RollupReport> {
    let mut report = RollupReport::default();
    let stored = sqlx::query_scalar!(
        "SELECT watermark FROM rollup_watermarks WHERE name = $1",
        kind.watermark()
    )
    .fetch_optional(pool)
    .await?;
    let mut watermark = match stored {
        Some(w) => w,
        // Never rolled up: start where the data starts.
        None => match partitions::days(pool, kind.table()).await?.first() {
            Some(day) => partitions::day_start(*day).min(cutoff),
            None => cutoff,
        },
    };
    report.add(&reroll_late(pool, kind, watermark).await?);
    while watermark < cutoff {
        let end = (watermark + WINDOW).min(cutoff);
        report.add(&roll_window(pool, kind, watermark, end).await?);
        watermark = end;
    }
    Ok(report)
}

/// Recomputes the spans late samples touched behind `watermark`, per subject.
async fn reroll_late(pool: &PgPool, kind: Kind, watermark: OffsetDateTime) -> Result<RollupReport> {
    let mut tx = pool.begin().await?;
    // Taken (deleted) first: see the module docs.
    let taken = sqlx::query!(
        "DELETE FROM rollup_dirty WHERE source = $1 AND bucket < $2
         RETURNING subject_id, bucket",
        kind.source(),
        watermark
    )
    .fetch_all(&mut *tx)
    .await?;
    let mut spans: BTreeMap<Uuid, (OffsetDateTime, OffsetDateTime)> = BTreeMap::new();
    for t in &taken {
        let span = spans.entry(t.subject_id).or_insert((t.bucket, t.bucket));
        span.0 = span.0.min(t.bucket);
        span.1 = span.1.max(t.bucket);
    }
    let mut report = RollupReport {
        late_buckets: taken.len() as u64,
        ..RollupReport::default()
    };
    for (subject, (first, last)) in spans {
        let from = first;
        let to = (last + Duration::MINUTE).min(watermark);
        let hour_from = floor(first, Duration::HOUR);
        let hour_to = ceil(to, Duration::HOUR).min(watermark);
        report.minutes += roll(&mut tx, kind, Resolution::Minute, from, to, Some(subject)).await?;
        report.hours += roll(
            &mut tx,
            kind,
            Resolution::Hour,
            hour_from,
            hour_to,
            Some(subject),
        )
        .await?;
    }
    tx.commit().await?;
    Ok(report)
}

/// Rolls up `[from, to)` and moves the watermark to `to`, in one transaction.
async fn roll_window(
    pool: &PgPool,
    kind: Kind,
    from: OffsetDateTime,
    to: OffsetDateTime,
) -> Result<RollupReport> {
    let mut tx = pool.begin().await?;
    // Marks inside the window are covered by it; take them before reading the samples.
    sqlx::query!(
        "DELETE FROM rollup_dirty WHERE source = $1 AND bucket >= $2 AND bucket < $3",
        kind.source(),
        from,
        to
    )
    .execute(&mut *tx)
    .await?;
    let minutes = roll(&mut tx, kind, Resolution::Minute, from, to, None).await?;
    let hours = roll(
        &mut tx,
        kind,
        Resolution::Hour,
        floor(from, Duration::HOUR),
        to,
        None,
    )
    .await?;
    sqlx::query!(
        "INSERT INTO rollup_watermarks (name, watermark) VALUES ($1, $2)
         ON CONFLICT (name) DO UPDATE SET watermark = excluded.watermark",
        kind.watermark(),
        to
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(RollupReport {
        minutes,
        hours,
        late_buckets: 0,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Resolution {
    Minute,
    Hour,
}

/// Upserts the aggregates of the samples in `[from, to)` (of one subject, or all).
async fn roll(
    tx: &mut sqlx::PgConnection,
    kind: Kind,
    resolution: Resolution,
    from: OffsetDateTime,
    to: OffsetDateTime,
    subject: Option<Uuid>,
) -> Result<u64> {
    if from >= to {
        return Ok(0);
    }
    Ok(match (kind, resolution) {
        (Kind::Host, Resolution::Minute) => host_minutes(tx, from, to, subject).await?,
        (Kind::Host, Resolution::Hour) => host_hours(tx, from, to, subject).await?,
        (Kind::Device, Resolution::Minute) => device_minutes(tx, from, to, subject).await?,
        (Kind::Device, Resolution::Hour) => device_hours(tx, from, to, subject).await?,
    })
}

async fn host_minutes(
    tx: &mut sqlx::PgConnection,
    from: OffsetDateTime,
    to: OffsetDateTime,
    subject: Option<Uuid>,
) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query!(
        "INSERT INTO host_rollup_1m (bucket, agent_run_id, worker_id, project_id, n,
                        host_cpu_pct_avg, host_cpu_pct_max, host_mem_used_avg, host_mem_used_max,
                        maestro_cpu_avg, maestro_rss_max, host_load_1m_avg, host_swap_used_avg,
                        host_psi_cpu_pct_avg, host_psi_memory_pct_avg, host_psi_io_pct_avg)
                 SELECT date_bin('1 minute', ts, TIMESTAMPTZ '2000-01-01'), agent_run_id,
                        (array_agg(worker_id ORDER BY ts DESC))[1],
                        (array_agg(project_id ORDER BY ts DESC))[1], count(*)::int,
                        avg(host_cpu_pct)::real, max(host_cpu_pct),
                        avg(host_mem_used_bytes)::bigint, max(host_mem_used_bytes),
                        avg(maestro_cpu_pct)::real, max(maestro_rss_bytes),
                        avg(host_load_1m)::real, avg(host_swap_used_bytes)::bigint,
                        avg(host_psi_cpu_pct)::real, avg(host_psi_memory_pct)::real,
                        avg(host_psi_io_pct)::real
                   FROM host_samples
                  WHERE ts >= $1 AND ts < $2 AND ($3::uuid IS NULL OR agent_run_id = $3)
                  GROUP BY 1, agent_run_id
                 ON CONFLICT (agent_run_id, bucket) DO UPDATE SET
                        worker_id = excluded.worker_id, project_id = excluded.project_id,
                        n = excluded.n, host_cpu_pct_avg = excluded.host_cpu_pct_avg,
                        host_cpu_pct_max = excluded.host_cpu_pct_max,
                        host_mem_used_avg = excluded.host_mem_used_avg,
                        host_mem_used_max = excluded.host_mem_used_max,
                        maestro_cpu_avg = excluded.maestro_cpu_avg,
                        maestro_rss_max = excluded.maestro_rss_max,
                        host_load_1m_avg = excluded.host_load_1m_avg,
                        host_swap_used_avg = excluded.host_swap_used_avg,
                        host_psi_cpu_pct_avg = excluded.host_psi_cpu_pct_avg,
                        host_psi_memory_pct_avg = excluded.host_psi_memory_pct_avg,
                        host_psi_io_pct_avg = excluded.host_psi_io_pct_avg",
        from,
        to,
        subject
    )
    .execute(tx)
    .await?
    .rows_affected())
}

async fn host_hours(
    tx: &mut sqlx::PgConnection,
    from: OffsetDateTime,
    to: OffsetDateTime,
    subject: Option<Uuid>,
) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query!(
        "INSERT INTO host_rollup_1h (bucket, agent_run_id, worker_id, project_id, n,
                        host_cpu_pct_avg, host_cpu_pct_max, host_mem_used_avg, host_mem_used_max,
                        maestro_cpu_avg, maestro_rss_max, host_load_1m_avg, host_swap_used_avg,
                        host_psi_cpu_pct_avg, host_psi_memory_pct_avg, host_psi_io_pct_avg)
                 SELECT date_bin('1 hour', ts, TIMESTAMPTZ '2000-01-01'), agent_run_id,
                        (array_agg(worker_id ORDER BY ts DESC))[1],
                        (array_agg(project_id ORDER BY ts DESC))[1], count(*)::int,
                        avg(host_cpu_pct)::real, max(host_cpu_pct),
                        avg(host_mem_used_bytes)::bigint, max(host_mem_used_bytes),
                        avg(maestro_cpu_pct)::real, max(maestro_rss_bytes),
                        avg(host_load_1m)::real, avg(host_swap_used_bytes)::bigint,
                        avg(host_psi_cpu_pct)::real, avg(host_psi_memory_pct)::real,
                        avg(host_psi_io_pct)::real
                   FROM host_samples
                  WHERE ts >= $1 AND ts < $2 AND ($3::uuid IS NULL OR agent_run_id = $3)
                  GROUP BY 1, agent_run_id
                 ON CONFLICT (agent_run_id, bucket) DO UPDATE SET
                        worker_id = excluded.worker_id, project_id = excluded.project_id,
                        n = excluded.n, host_cpu_pct_avg = excluded.host_cpu_pct_avg,
                        host_cpu_pct_max = excluded.host_cpu_pct_max,
                        host_mem_used_avg = excluded.host_mem_used_avg,
                        host_mem_used_max = excluded.host_mem_used_max,
                        maestro_cpu_avg = excluded.maestro_cpu_avg,
                        maestro_rss_max = excluded.maestro_rss_max,
                        host_load_1m_avg = excluded.host_load_1m_avg,
                        host_swap_used_avg = excluded.host_swap_used_avg,
                        host_psi_cpu_pct_avg = excluded.host_psi_cpu_pct_avg,
                        host_psi_memory_pct_avg = excluded.host_psi_memory_pct_avg,
                        host_psi_io_pct_avg = excluded.host_psi_io_pct_avg",
        from,
        to,
        subject
    )
    .execute(tx)
    .await?
    .rows_affected())
}

async fn device_minutes(
    tx: &mut sqlx::PgConnection,
    from: OffsetDateTime,
    to: OffsetDateTime,
    subject: Option<Uuid>,
) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query!(
        "INSERT INTO device_rollup_1m (bucket, device_session_id, agent_run_id, project_id,
                        n, emu_cpu_pct_avg, emu_cpu_pct_max, emu_rss_max)
                 SELECT date_bin('1 minute', ts, TIMESTAMPTZ '2000-01-01'), device_session_id,
                        (array_agg(agent_run_id ORDER BY ts DESC))[1],
                        (array_agg(project_id ORDER BY ts DESC))[1], count(*)::int,
                        avg(emu_cpu_pct)::real, max(emu_cpu_pct), max(emu_rss_bytes)
                   FROM device_samples
                  WHERE ts >= $1 AND ts < $2 AND ($3::uuid IS NULL OR device_session_id = $3)
                  GROUP BY 1, device_session_id
                 ON CONFLICT (device_session_id, bucket) DO UPDATE SET
                        agent_run_id = excluded.agent_run_id, project_id = excluded.project_id,
                        n = excluded.n, emu_cpu_pct_avg = excluded.emu_cpu_pct_avg,
                        emu_cpu_pct_max = excluded.emu_cpu_pct_max,
                        emu_rss_max = excluded.emu_rss_max",
        from,
        to,
        subject
    )
    .execute(tx)
    .await?
    .rows_affected())
}

async fn device_hours(
    tx: &mut sqlx::PgConnection,
    from: OffsetDateTime,
    to: OffsetDateTime,
    subject: Option<Uuid>,
) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query!(
        "INSERT INTO device_rollup_1h (bucket, device_session_id, agent_run_id, project_id,
                        n, emu_cpu_pct_avg, emu_cpu_pct_max, emu_rss_max)
                 SELECT date_bin('1 hour', ts, TIMESTAMPTZ '2000-01-01'), device_session_id,
                        (array_agg(agent_run_id ORDER BY ts DESC))[1],
                        (array_agg(project_id ORDER BY ts DESC))[1], count(*)::int,
                        avg(emu_cpu_pct)::real, max(emu_cpu_pct), max(emu_rss_bytes)
                   FROM device_samples
                  WHERE ts >= $1 AND ts < $2 AND ($3::uuid IS NULL OR device_session_id = $3)
                  GROUP BY 1, device_session_id
                 ON CONFLICT (device_session_id, bucket) DO UPDATE SET
                        agent_run_id = excluded.agent_run_id, project_id = excluded.project_id,
                        n = excluded.n, emu_cpu_pct_avg = excluded.emu_cpu_pct_avg,
                        emu_cpu_pct_max = excluded.emu_cpu_pct_max,
                        emu_rss_max = excluded.emu_rss_max",
        from,
        to,
        subject
    )
    .execute(tx)
    .await?
    .rows_affected())
}

#[cfg(test)]
mod tests {
    use fleet_protocol::rest::MetricResolution;

    use super::*;
    use crate::screens::HostRow;

    /// The test's timeline, anchored in the previous hour so that it is always in the past of the
    /// database clock (late-data marking compares against it) and inside the partitions the
    /// migrations create (yesterday ... a week ahead), and never straddles an hour boundary.
    struct Clock {
        /// Start of the previous hour.
        hour: OffsetDateTime,
    }

    impl Clock {
        fn new() -> Self {
            let now = OffsetDateTime::now_utc();
            let hour = now
                .replace_minute(0)
                .and_then(|t| t.replace_second(0))
                .and_then(|t| t.replace_nanosecond(0))
                .unwrap()
                - Duration::hours(1);
            Self { hour }
        }

        /// `minute` minutes and `second` seconds into the hour.
        fn at(&self, minute: i64, second: i64) -> OffsetDateTime {
            self.hour + Duration::minutes(minute) + Duration::seconds(second)
        }

        /// The job runs at 30 minutes and 30 seconds past: samples of the last two minutes are
        /// too young to roll up.
        fn now(&self) -> OffsetDateTime {
            self.at(30, 30)
        }
    }

    struct Rig {
        store: Store,
        project: Uuid,
        worker: Uuid,
        run: Uuid,
    }

    async fn rig(pool: PgPool) -> Rig {
        let store = Store {
            pool,
            origin: uuid::Uuid::now_v7(),
        };
        let project = store.upsert_project("app", "App").await.unwrap();
        let worker = sqlx::query_scalar!(
            "INSERT INTO workers (project_id, identity_key) VALUES ($1, 'runner-a') RETURNING id",
            project
        )
        .fetch_one(store.pool())
        .await
        .unwrap();
        Rig {
            store,
            project,
            worker,
            run: Uuid::now_v7(),
        }
    }

    impl Rig {
        async fn host_sample(
            &self,
            ts: OffsetDateTime,
            cpu: Option<f32>,
            maestro_cpu: Option<f32>,
            tseq: i64,
        ) {
            sqlx::query!(
                "INSERT INTO host_samples (ts, agent_run_id, worker_id, project_id, tseq,
                                           host_cpu_pct, host_mem_used_bytes, maestro_cpu_pct)
                 VALUES ($1, $2, $3, $4, $5, $6, 4294967296, $7)",
                ts,
                self.run,
                self.worker,
                self.project,
                tseq,
                cpu,
                maestro_cpu
            )
            .execute(self.store.pool())
            .await
            .unwrap();
        }

        async fn host(
            &self,
            clock: &Clock,
            resolution: MetricResolution,
        ) -> Vec<(OffsetDateTime, Option<f64>, Option<f64>)> {
            let rows: Vec<HostRow> = self
                .store
                .host_samples(
                    self.project,
                    self.worker,
                    (
                        clock.hour - Duration::days(2),
                        clock.hour + Duration::days(1),
                    ),
                    resolution,
                )
                .await
                .unwrap()
                .unwrap()
                .rows;
            rows.into_iter()
                .map(|r| (r.at, r.host_cpu_pct, r.maestro_cpu_pct))
                .collect()
        }

        /// The sample counts of the one-minute and one-hour rows, oldest first.
        async fn counts(&self) -> (Vec<i32>, Vec<i32>) {
            let minutes = sqlx::query_scalar!("SELECT n FROM host_rollup_1m ORDER BY bucket")
                .fetch_all(self.store.pool())
                .await
                .unwrap();
            let hours = sqlx::query_scalar!("SELECT n FROM host_rollup_1h ORDER BY bucket")
                .fetch_all(self.store.pool())
                .await
                .unwrap();
            (minutes, hours)
        }

        /// The worked example: three closed minutes and one that is too young.
        async fn worked_example(&self, c: &Clock) {
            self.host_sample(c.at(20, 5), Some(10.0), None, 1).await;
            self.host_sample(c.at(20, 10), Some(20.0), None, 2).await;
            self.host_sample(c.at(20, 15), Some(30.0), None, 3).await;
            self.host_sample(c.at(21, 5), Some(40.0), Some(80.0), 4)
                .await;
            self.host_sample(c.at(22, 5), None, Some(50.0), 5).await;
            // Inside the last two minutes before the job's `now`: not rolled up yet.
            self.host_sample(c.at(29, 0), Some(99.0), Some(99.0), 6)
                .await;
        }
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn closed_minutes_roll_up_into_minute_and_hour_rows_and_missing_readings_stay_missing(
        pool: PgPool,
    ) {
        let rig = rig(pool).await;
        let c = Clock::new();
        rig.worked_example(&c).await;

        rig.store.roll_up(c.now()).await.unwrap();

        // One row per closed minute: the average of the readings that exist; no reading at all
        // stays None (never zero).
        assert_eq!(
            rig.host(&c, MetricResolution::OneMinute).await,
            [
                (c.at(20, 0), Some(20.0), None),
                (c.at(21, 0), Some(40.0), Some(80.0)),
                (c.at(22, 0), None, Some(50.0)),
            ]
        );
        // The hour averages the readings that exist: (10+20+30+40)/4 and (80+50)/2.
        assert_eq!(
            rig.host(&c, MetricResolution::OneHour).await,
            [(c.at(0, 0), Some(25.0), Some(65.0))]
        );
        // `n` counts samples, readings or not.
        assert_eq!(rig.counts().await, (vec![3, 1, 1], vec![5]));
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn rolling_up_again_changes_nothing(pool: PgPool) {
        let rig = rig(pool).await;
        let c = Clock::new();
        rig.worked_example(&c).await;
        rig.store.roll_up(c.now()).await.unwrap();
        let first = (
            rig.host(&c, MetricResolution::OneMinute).await,
            rig.host(&c, MetricResolution::OneHour).await,
            rig.counts().await,
        );

        rig.store.roll_up(c.now()).await.unwrap();
        rig.store.roll_up(c.now()).await.unwrap();

        assert_eq!(
            (
                rig.host(&c, MetricResolution::OneMinute).await,
                rig.host(&c, MetricResolution::OneHour).await,
                rig.counts().await,
            ),
            first
        );
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn a_minute_becomes_rollable_as_time_passes(pool: PgPool) {
        let rig = rig(pool).await;
        let c = Clock::new();
        rig.worked_example(&c).await;
        rig.store.roll_up(c.now()).await.unwrap();

        // Three minutes later the 29:00 sample is old enough.
        rig.store
            .roll_up(c.now() + Duration::minutes(3))
            .await
            .unwrap();

        let minutes = rig.host(&c, MetricResolution::OneMinute).await;
        assert_eq!(minutes.last(), Some(&(c.at(29, 0), Some(99.0), Some(99.0))));
        assert_eq!(rig.counts().await, (vec![3, 1, 1, 1], vec![6]));
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn late_samples_reroll_the_minute_and_the_hour_they_belong_to(pool: PgPool) {
        let rig = rig(pool).await;
        let c = Clock::new();
        rig.worked_example(&c).await;
        rig.store.roll_up(c.now()).await.unwrap();

        // A replay after a reconnect: a sample for a minute that was already rolled up.
        rig.host_sample(c.at(20, 20), Some(60.0), None, 7).await;
        rig.store.roll_up(c.now()).await.unwrap();

        // (10+20+30+60)/4 and (10+20+30+60+40)/5; a late sample is neither lost nor double counted.
        assert_eq!(
            rig.host(&c, MetricResolution::OneMinute).await.first(),
            Some(&(c.at(20, 0), Some(30.0), None))
        );
        assert_eq!(
            rig.host(&c, MetricResolution::OneHour).await,
            [(c.at(0, 0), Some(32.0), Some(65.0))]
        );
        assert_eq!(rig.counts().await, (vec![4, 1, 1], vec![6]));
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn a_long_backlog_is_rolled_up_in_one_pass(pool: PgPool) {
        let rig = rig(pool).await;
        let c = Clock::new();
        // Samples 19 hours before the example: more than one catch-up window apart.
        let old = c.hour - Duration::hours(19);
        rig.host_sample(old + Duration::minutes(5), Some(7.0), Some(1.0), 1)
            .await;
        rig.worked_example(&c).await;

        rig.store.roll_up(c.now()).await.unwrap();

        let minutes = rig.host(&c, MetricResolution::OneMinute).await;
        assert_eq!(minutes.len(), 4);
        assert_eq!(
            minutes.first(),
            Some(&(old + Duration::minutes(5), Some(7.0), Some(1.0)))
        );
        assert_eq!(rig.host(&c, MetricResolution::OneHour).await.len(), 2);
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn load_swap_and_pressure_roll_up_as_averages_of_the_readings_there_are(pool: PgPool) {
        let rig = rig(pool).await;
        let c = Clock::new();
        for (second, load, psi, swap) in [
            (5, Some(1.0_f32), Some(10.0_f32), Some(100_i64)),
            (10, Some(3.0), None, Some(300)),
        ] {
            sqlx::query!(
                "INSERT INTO host_samples (ts, agent_run_id, worker_id, project_id, tseq,
                                           host_load_1m, host_psi_cpu_pct, host_swap_used_bytes)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
                c.at(20, second),
                rig.run,
                rig.worker,
                rig.project,
                second,
                load,
                psi,
                swap
            )
            .execute(rig.store.pool())
            .await
            .unwrap();
        }

        rig.store.roll_up(c.now()).await.unwrap();

        let m = sqlx::query!(
            "SELECT host_load_1m_avg, host_psi_cpu_pct_avg, host_psi_memory_pct_avg,
                    host_swap_used_avg
               FROM host_rollup_1m WHERE agent_run_id = $1",
            rig.run
        )
        .fetch_one(rig.store.pool())
        .await
        .unwrap();
        assert_eq!(
            (
                m.host_load_1m_avg,
                m.host_psi_cpu_pct_avg,
                m.host_psi_memory_pct_avg,
                m.host_swap_used_avg
            ),
            (Some(2.0), Some(10.0), None, Some(200)),
            "an average over the readings that exist; no reading stays missing"
        );
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn device_samples_roll_up_too(pool: PgPool) {
        let rig = rig(pool).await;
        let c = Clock::new();
        let session = Uuid::now_v7();
        for (second, cpu, rss) in [
            (5, Some(10.0_f32), Some(100_i64)),
            (10, Some(30.0), Some(300)),
            (15, None, None),
        ] {
            sqlx::query!(
                "INSERT INTO device_samples (ts, device_session_id, agent_run_id, project_id,
                                             emu_cpu_pct, emu_rss_bytes)
                 VALUES ($1, $2, $3, $4, $5, $6)",
                c.at(20, second),
                session,
                rig.run,
                rig.project,
                cpu,
                rss
            )
            .execute(rig.store.pool())
            .await
            .unwrap();
        }

        rig.store.roll_up(c.now()).await.unwrap();

        let m = sqlx::query!(
            "SELECT bucket, n, emu_cpu_pct_avg, emu_cpu_pct_max, emu_rss_max
               FROM device_rollup_1m WHERE device_session_id = $1",
            session
        )
        .fetch_one(rig.store.pool())
        .await
        .unwrap();
        assert_eq!(
            (
                m.bucket,
                m.n,
                m.emu_cpu_pct_avg,
                m.emu_cpu_pct_max,
                m.emu_rss_max
            ),
            (c.at(20, 0), 3, Some(20.0), Some(30.0), Some(300))
        );
        let h = sqlx::query!(
            "SELECT bucket, n FROM device_rollup_1h WHERE device_session_id = $1",
            session
        )
        .fetch_one(rig.store.pool())
        .await
        .unwrap();
        assert_eq!((h.bucket, h.n), (c.at(0, 0), 3));
    }
}

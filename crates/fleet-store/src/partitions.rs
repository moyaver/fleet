// SPDX-License-Identifier: AGPL-3.0-only
//! Daily partitions: which exist, and dropping one (PLAN §7.7; research 05 §10.2).
//!
//! `fleet_ensure_partitions` (migration 0004) creates `<table>_pYYYYMMDD`, covering the UTC day
//! `YYYYMMDD`. Names are rebuilt from a parsed date and a fixed table name before they reach
//! SQL, so no text from the database is ever interpolated into DDL.

use sqlx::{AssertSqlSafe, PgPool};
use time::{Date, Month, OffsetDateTime, Time};

use crate::Result;

/// A table partitioned by day.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Table {
    /// The agent journal, by `run_day`.
    AgentEvents,
    /// Flow steps, by `run_day`.
    StepExecutions,
    /// Host metric samples, by time.
    HostSamples,
    /// Device metric samples, by time.
    DeviceSamples,
}

impl Table {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::AgentEvents => "agent_events",
            Self::StepExecutions => "step_executions",
            Self::HostSamples => "host_samples",
            Self::DeviceSamples => "device_samples",
        }
    }

    fn partition(self, day: Date) -> String {
        format!(
            "{}_p{:04}{:02}{:02}",
            self.name(),
            day.year(),
            u8::from(day.month()),
            day.day()
        )
    }
}

/// The start of a UTC day.
pub(crate) fn day_start(day: Date) -> OffsetDateTime {
    day.with_time(Time::MIDNIGHT).assume_utc()
}

/// `20261104` as a date; anything else (including a partition of another naming scheme) is `None`.
fn parse_day(digits: &str) -> Option<Date> {
    if digits.len() != 8 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let year = digits.get(0..4)?.parse().ok()?;
    let month = Month::try_from(digits.get(4..6)?.parse::<u8>().ok()?).ok()?;
    Date::from_calendar_date(year, month, digits.get(6..8)?.parse().ok()?).ok()
}

/// The days that have a partition of `table`, oldest first.
pub(crate) async fn days(pool: &PgPool, table: Table) -> Result<Vec<Date>> {
    let names: Vec<String> = sqlx::query_scalar!(
        "SELECT c.relname::text AS \"name!\"
           FROM pg_inherits i JOIN pg_class c ON c.oid = i.inhrelid
          WHERE i.inhparent = $1::text::regclass",
        table.name()
    )
    .fetch_all(pool)
    .await?;
    let prefix = format!("{}_p", table.name());
    let mut days: Vec<Date> = names
        .iter()
        .filter_map(|n| n.strip_prefix(&prefix).and_then(parse_day))
        .collect();
    days.sort_unstable();
    Ok(days)
}

/// Whether the partition holds no rows.
pub(crate) async fn is_empty(pool: &PgPool, table: Table, day: Date) -> Result<bool> {
    let name = table.partition(day);
    let has_rows: bool = sqlx::query_scalar(AssertSqlSafe(format!(
        "SELECT EXISTS (SELECT 1 FROM {name})"
    )))
    .fetch_one(pool)
    .await?;
    Ok(!has_rows)
}

/// Detaches and drops one day's partition. `DETACH … CONCURRENTLY` cannot run in a transaction
/// block, so each statement is its own round trip: a crash in between leaves a detached table that
/// the next run no longer sees (it is no longer a partition), harmless but to be dropped by hand.
pub(crate) async fn drop_day(pool: &PgPool, table: Table, day: Date) -> Result<()> {
    let name = table.partition(day);
    sqlx::query(AssertSqlSafe(format!(
        "ALTER TABLE {} DETACH PARTITION {name} CONCURRENTLY",
        table.name()
    )))
    .execute(pool)
    .await?;
    sqlx::query(AssertSqlSafe(format!("DROP TABLE IF EXISTS {name}")))
        .execute(pool)
        .await?;
    Ok(())
}

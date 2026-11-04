// SPDX-License-Identifier: AGPL-3.0-only
//! Postgres persistence for the Fleet server (PLAN §9, research 05 §9; ADR-0006).
//!
//! - [`Store`]: the connection pool and migrations (`migrations/` at the repository root,
//!   embedded).
//! - [`auth`]: projects, API keys and agent tokens. Only SHA-256 hashes of secrets are stored.
//! - [`ingest`]: the agent run lifecycle and the ingest transaction: one transaction per batch
//!   that fences the epoch, skips duplicates, applies claims and entities with ownership checks,
//!   re-derives outcomes and advances the cursor (research 05 §9.4).
//! - [`telemetry`]: buffered metric samples, written in one statement per flush.
//! - [`live`]: read models for REST snapshots and live deltas, with a consistent `version`.
//! - [`plans`]: immutable Fleet Planner manifests and planned totals (ADR-0013).
//! - [`sweep`]: leader-only liveness sweeps (detach silent runs, abandon expired ones).
//!
//! Invariants upheld here: I3 (only the claim lattice decides a flow's outcome, and a `CHECK`
//! constraint refuses a passed claim from an inferred source) and I5 (gaps are recorded in
//! `ingest_gaps`, never silent). No `sqlx` type crosses this crate's API.

pub mod auth;
pub mod ingest;
pub mod live;
pub mod plans;
pub mod sweep;
pub mod telemetry;
#[cfg(feature = "test-util")]
pub mod testing;

use std::time::Duration;

use sqlx::{
    PgPool,
    postgres::{PgConnectOptions, PgPoolOptions},
};

/// A database operation failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The database refused or couldn't run a query.
    #[error("database query failed")]
    Database(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// A migration failed or the schema doesn't match this build.
    #[error("migrating the database")]
    Migrate(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// The connection settings are invalid.
    #[error("invalid database URL")]
    Url(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// Stored data broke an assumption this build relies on.
    #[error("inconsistent stored data: {0}")]
    Corrupt(String),
}

impl From<sqlx::Error> for Error {
    fn from(e: sqlx::Error) -> Self {
        Self::Database(Box::new(e))
    }
}

/// The result type of this crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Where the schema stands relative to this build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaStatus {
    /// Every migration this build knows is applied, and nothing else.
    Current,
    /// Migrations are pending: run `fleet migrate`.
    Behind {
        /// Applied migrations.
        applied: usize,
        /// Migrations this build knows.
        known: usize,
    },
    /// The database has migrations this build doesn't know: refuse to run on it.
    Ahead,
}

/// Pool settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolOptions {
    /// Most connections held.
    pub max_connections: u32,
    /// How long a caller waits for a free connection.
    pub acquire_timeout: Duration,
}

impl Default for PoolOptions {
    fn default() -> Self {
        Self {
            max_connections: 20,
            acquire_timeout: Duration::from_secs(5),
        }
    }
}

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");

/// The Fleet database.
#[derive(Debug, Clone)]
pub struct Store {
    pool: PgPool,
}

impl Store {
    /// Connects lazily: the pool opens connections on first use, so start-up never blocks on
    /// the database.
    pub fn connect_lazy(url: &str, options: PoolOptions) -> Result<Self> {
        let connect: PgConnectOptions = url.parse().map_err(|e| Error::Url(Box::new(e)))?;
        let pool = PgPoolOptions::new()
            .max_connections(options.max_connections)
            .acquire_timeout(options.acquire_timeout)
            .connect_lazy_with(connect);
        Ok(Self { pool })
    }

    pub(crate) fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Applies pending migrations under sqlx's advisory lock, so concurrent instances are safe.
    pub async fn migrate(&self) -> Result<()> {
        MIGRATOR
            .run(&self.pool)
            .await
            .map_err(|e| Error::Migrate(Box::new(e)))
    }

    /// Compares the applied migrations with the ones embedded in this build.
    pub async fn schema_status(&self) -> Result<SchemaStatus> {
        let exists: Option<String> =
            sqlx::query_scalar!("SELECT to_regclass('_sqlx_migrations')::text")
                .fetch_one(&self.pool)
                .await?;
        let known = MIGRATOR
            .iter()
            .filter(|m| m.migration_type.is_up_migration())
            .count();
        if exists.is_none() {
            return Ok(SchemaStatus::Behind { applied: 0, known });
        }
        let applied: Vec<i64> = sqlx::query_scalar(
            "SELECT version FROM _sqlx_migrations WHERE success ORDER BY version",
        )
        .fetch_all(&self.pool)
        .await?;
        let ours: Vec<i64> = MIGRATOR.iter().map(|m| m.version).collect();
        if applied.iter().any(|v| !ours.contains(v)) {
            return Ok(SchemaStatus::Ahead);
        }
        if applied.len() < known {
            return Ok(SchemaStatus::Behind {
                applied: applied.len(),
                known,
            });
        }
        Ok(SchemaStatus::Current)
    }

    /// Checks the database answers.
    pub async fn ping(&self) -> Result<()> {
        sqlx::query_scalar!("SELECT 1 AS one")
            .fetch_one(&self.pool)
            .await?;
        Ok(())
    }

    /// Creates the daily partitions of every partitioned table for `days_before` days back and
    /// `days_ahead` days ahead of today (UTC). Idempotent; returns how many it created.
    pub async fn ensure_partitions(&self, days_before: i32, days_ahead: i32) -> Result<i32> {
        let mut created = 0;
        for table in [
            "agent_events",
            "step_executions",
            "host_samples",
            "device_samples",
        ] {
            let n: Option<i32> = sqlx::query_scalar!(
                "SELECT fleet_ensure_partitions($1::text::regclass,
                        (now() AT TIME ZONE 'UTC')::date - $2::int,
                        (now() AT TIME ZONE 'UTC')::date + $3::int)",
                table,
                days_before,
                days_ahead
            )
            .fetch_one(&self.pool)
            .await?;
            created += n.unwrap_or(0);
        }
        Ok(created)
    }

    /// Closes the pool, waiting for checked-out connections to return.
    pub async fn close(&self) {
        self.pool.close().await;
    }
}

/// SHA-256 of a secret, as stored.
pub(crate) fn sha256(secret: &str) -> Vec<u8> {
    use sha2::Digest;
    sha2::Sha256::digest(secret.as_bytes()).to_vec()
}

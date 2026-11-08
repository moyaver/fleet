// SPDX-License-Identifier: AGPL-3.0-only
//! The Fleet server (PLAN §7; research 03, 05, 06).
//!
//! - [`config`]: figment configuration (TOML, `FLEET_*`, `*_FILE` secrets).
//! - `ingest`: the agent WebSocket (`/api/v1/agent/ws`, `fleet.v1+json`) and the HTTPS batch
//!   fallback (`/api/v1/agent/batch`). Each `events` frame is one transaction; the ack follows
//!   the commit (research 05 §3.5).
//! - `live` and `api`: the live hub (versioned, ordered deltas; rebuilt from Postgres), the
//!   browser socket `/api/v1/live`, REST snapshots and `/api/v1/openapi.json`.
//! - `artifacts`: the artifact broker (upload protocol, verification, blob store) and the
//!   usercontent origin that alone serves artifact bytes.
//! - `sweeps`: liveness, and the leader's sweeps (advisory lock).
//! - `app`: the router, graceful shutdown (`goaway`, close 1012) and start-up.
//!
//! `main.rs` only parses arguments and installs logging; [`run`] does the rest.

pub mod config;

mod api;
mod app;
mod artifacts;
mod client_ip;
mod health;
mod ingest;
mod limits;
mod live;
mod login;
mod oidc;
mod retention;
mod session;
mod state;
mod sweeps;
mod web;

use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, Subcommand};
use fleet_store::{PoolOptions, Store, auth::TrustLevel};
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

pub use crate::app::{Server, start};
use crate::config::Config;

/// Command line.
#[derive(Debug, Parser)]
#[command(name = "fleet", version, about = "Fleet server")]
pub struct Cli {
    /// TOML configuration file.
    #[arg(long, global = true, env = "FLEET_CONFIG")]
    pub config: Option<PathBuf>,
    /// What to do.
    #[command(subcommand)]
    pub command: Command,
}

/// Subcommands (PLAN §7.1).
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Serve the dashboard and the APIs.
    Serve,
    /// Apply database migrations, then exit.
    Migrate,
    /// Validate the configuration and print it with secrets redacted.
    CheckConfig,
    /// Exit 0 if the local server is ready (for Docker `HEALTHCHECK`; the image has no curl).
    Healthcheck,
    /// Manage API keys for generic CI (`mf_live_…`).
    ApiKey {
        /// What to do.
        #[command(subcommand)]
        action: ApiKeyAction,
    },
}

/// `fleet api-key …`.
#[derive(Debug, Subcommand)]
pub enum ApiKeyAction {
    /// Create a key and print it once. Only its hash is stored.
    Create {
        /// Project slug (created if missing).
        #[arg(long)]
        project: String,
        /// A name to recognise the key by.
        #[arg(long)]
        name: String,
        /// Days until the key expires. Defaults to `api.api_key_ttl_days` (90).
        #[arg(long)]
        expires_in_days: Option<u32>,
    },
    /// List a project's keys: their IDs, names, expiry and revocation. No secret is shown.
    List {
        /// Project slug.
        #[arg(long)]
        project: String,
    },
    /// Revoke a key at once. Agent tokens minted from it stop too. Works on keys made before
    /// expiry existed.
    Revoke {
        /// The key's ID, from `api-key list`.
        #[arg(long)]
        id: Uuid,
    },
    /// Set when a key stops working, for keys made before expiry existed or any other key.
    Expire {
        /// The key's ID, from `api-key list`.
        #[arg(long)]
        id: Uuid,
        /// Days from now until it stops working. `0` ends it now.
        #[arg(long)]
        in_days: u32,
    },
}

/// Runs a subcommand. `env` is the process environment (passed in so tests stay hermetic).
pub async fn run(cli: Cli, env: Vec<(String, String)>) -> anyhow::Result<()> {
    let config = Config::load(cli.config.as_deref(), &env)?;
    match cli.command {
        Command::CheckConfig => {
            let shown = config.redacted()?;
            tracing::info!(config = %shown, "configuration is valid");
            Ok(())
        }
        Command::Healthcheck => health::probe(config.listen).await,
        Command::Migrate => {
            let store = connect(&config)?;
            store.migrate().await.context("migrating")?;
            tracing::info!("migrations applied");
            Ok(())
        }
        Command::ApiKey {
            action:
                ApiKeyAction::Create {
                    project,
                    name,
                    expires_in_days,
                },
        } => {
            let store = connect(&config)?;
            // A project that exists keeps its name; only a new one takes its slug as its name.
            let project_id = match store.project_id(&project).await? {
                Some(id) => id,
                None => store.upsert_project(&project, &project).await?,
            };
            let expires_at = key_expiry(
                OffsetDateTime::now_utc(),
                expires_in_days,
                config.api.api_key_ttl_days,
            );
            let (id, key) = store
                .create_api_key(project_id, &name, TrustLevel::Trusted, expires_at)
                .await?;
            tracing::info!(%id, project, "api key created; it is shown once on stdout");
            app::print_once(key.expose())
        }
        Command::ApiKey {
            action: ApiKeyAction::List { project },
        } => {
            let store = connect(&config)?;
            let Some(project_id) = store.project_id(&project).await? else {
                anyhow::bail!("no project {project}");
            };
            let keys = store.list_api_keys(project_id).await?;
            app::print_table(&key_table(&keys))
        }
        Command::ApiKey {
            action: ApiKeyAction::Revoke { id },
        } => {
            let store = connect(&config)?;
            anyhow::ensure!(store.revoke_api_key(id).await?, "no api key {id}");
            tracing::info!(%id, "api key revoked");
            Ok(())
        }
        Command::ApiKey {
            action: ApiKeyAction::Expire { id, in_days },
        } => {
            let store = connect(&config)?;
            let expires_at = key_expiry(OffsetDateTime::now_utc(), Some(in_days), 0);
            anyhow::ensure!(
                store.set_api_key_expiry(id, expires_at).await?,
                "no active api key {id} (it may be revoked)"
            );
            tracing::info!(%id, %expires_at, "api key expiry set");
            Ok(())
        }
        Command::Serve => app::serve(config).await,
    }
}

/// One line per key for `api-key list`: ID, name, prefix, expiry, revocation and last use.
pub(crate) fn key_table(keys: &[fleet_store::auth::ApiKeyInfo]) -> String {
    use std::fmt::Write as _;
    let when = |t: Option<OffsetDateTime>| {
        t.map_or_else(
            || "-".to_owned(),
            |t| {
                t.format(&time::format_description::well_known::Rfc3339)
                    .unwrap_or_default()
            },
        )
    };
    let mut out = String::from("id\tname\tprefix\texpires_at\trevoked_at\tlast_used_at\n");
    for k in keys {
        let _ = writeln!(
            out,
            "{}\t{}\t{}\t{}\t{}\t{}",
            k.id,
            k.name,
            k.prefix,
            when(k.expires_at),
            when(k.revoked_at),
            when(k.last_used_at)
        );
    }
    out
}

/// When a new API key expires: `days` from `now` when given, else the configured default.
#[must_use]
pub(crate) fn key_expiry(
    now: OffsetDateTime,
    days: Option<u32>,
    default_days: u32,
) -> OffsetDateTime {
    now + Duration::days(i64::from(days.unwrap_or(default_days)))
}

pub(crate) fn connect(config: &Config) -> anyhow::Result<Store> {
    let url = config
        .database
        .url
        .as_ref()
        .context("database.url is not set (FLEET_DATABASE__URL or FLEET_DATABASE__URL_FILE)")?;
    Ok(Store::connect_lazy(
        url.expose(),
        PoolOptions {
            max_connections: config.database.max_connections,
            ..PoolOptions::default()
        },
    )?)
}

#[cfg(test)]
mod tests {
    use time::{Duration, OffsetDateTime};

    #[test]
    fn the_key_subcommands_take_ids_and_days() {
        use clap::Parser as _;

        use super::Cli;
        let id = "0190a000-0000-7000-8000-000000000001";
        for argv in [
            vec!["fleet", "api-key", "list", "--project", "app"],
            vec!["fleet", "api-key", "revoke", "--id", id],
            vec!["fleet", "api-key", "expire", "--id", id, "--in-days", "0"],
        ] {
            Cli::try_parse_from(argv.clone()).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
        }
        assert!(Cli::try_parse_from(["fleet", "api-key", "expire", "--id", id]).is_err());
    }

    use super::key_expiry;

    #[test]
    fn a_new_key_expires_in_the_configured_days_unless_asked_otherwise() {
        let now = OffsetDateTime::UNIX_EPOCH;
        assert_eq!(key_expiry(now, None, 90), now + Duration::days(90));
        assert_eq!(key_expiry(now, Some(7), 90), now + Duration::days(7));
    }
}

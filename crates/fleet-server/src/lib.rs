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
            action: ApiKeyAction::Create { project, name },
        } => {
            let store = connect(&config)?;
            let project_id = store.upsert_project(&project, &project).await?;
            let (id, key) = store
                .create_api_key(project_id, &name, TrustLevel::Trusted)
                .await?;
            tracing::info!(%id, project, "api key created; it is shown once on stdout");
            app::print_once(key.expose())
        }
        Command::Serve => app::serve(config).await,
    }
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

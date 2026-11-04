// SPDX-License-Identifier: AGPL-3.0-only
//! `fleet`: the Fleet server. M0 serves only the embedded dashboard. See PLAN §7.

mod web;

use std::net::SocketAddr;

use anyhow::Context;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "fleet", version, about = "Fleet server")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve the dashboard and API.
    Serve {
        /// Address to listen on.
        #[arg(long, env = "FLEET_LISTEN", default_value = "127.0.0.1:8080")]
        listen: SocketAddr,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_env("FLEET_LOG").unwrap_or_else(|_| "info".into()))
        .init();

    match Cli::parse().command {
        Command::Serve { listen } => {
            let listener = tokio::net::TcpListener::bind(listen)
                .await
                .with_context(|| format!("binding {listen}"))?;
            tracing::info!("listening on http://{listen}");
            axum::serve(listener, web::router())
                .with_graceful_shutdown(async {
                    let _ = tokio::signal::ctrl_c().await;
                })
                .await?;
        }
    }
    Ok(())
}

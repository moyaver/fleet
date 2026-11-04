// SPDX-License-Identifier: AGPL-3.0-only
//! `fleet`: the Fleet server. M0 serves only the embedded dashboard. See PLAN §7.

mod web;

use std::net::SocketAddr;

use anyhow::Context;
use clap::{Parser, Subcommand};
use tracing_subscriber::{EnvFilter, filter::LevelFilter};

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
    let filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .with_env_var("FLEET_LOG")
        .from_env()
        .context("parsing FLEET_LOG")?;
    tracing_subscriber::fmt().with_env_filter(filter).init();

    match Cli::parse().command {
        Command::Serve { listen } => {
            let listener = tokio::net::TcpListener::bind(listen)
                .await
                .with_context(|| format!("binding {listen}"))?;
            tracing::info!(%listen, "server listening");
            axum::serve(listener, web::router())
                .with_graceful_shutdown(shutdown_signal())
                .await?;
        }
    }
    Ok(())
}

/// Resolves on SIGINT or SIGTERM (`docker stop`). A handler that can't be installed is logged
/// and never fires, rather than shutting the server down at once.
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let ctrl_c = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::warn!(%error, "SIGINT handler unavailable");
            std::future::pending::<()>().await;
        }
    };
    let terminate = async {
        match signal(SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(error) => {
                tracing::warn!(%error, "SIGTERM handler unavailable");
                std::future::pending::<()>().await;
            }
        }
    };
    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
    tracing::info!("shutting down");
}

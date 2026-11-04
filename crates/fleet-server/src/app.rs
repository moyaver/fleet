// SPDX-License-Identifier: AGPL-3.0-only
//! Start-up, routing and graceful shutdown (research 03 §11, research 05 §3.9).
//!
//! Start-up: migrate (or refuse an outdated or newer schema), create partitions and configured
//! projects, then turn ready. Shutdown on SIGINT/SIGTERM: readiness off, stop accepting, every
//! agent socket gets `goaway` and close 1012 after its in-flight batch and final telemetry ack,
//! then the pool closes.

use std::{io::Write as _, net::SocketAddr, time::Duration};

use anyhow::{Context, bail};
use axum::Router;
use fleet_store::SchemaStatus;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::{
    api,
    config::{Config, MigrateMode},
    health, ingest,
    state::AppState,
    sweeps, web,
};

/// A running server, for `serve` and for tests.
#[derive(Debug)]
#[must_use = "dropping the handle leaves the server running until the runtime stops"]
pub struct Server {
    addr: SocketAddr,
    state: AppState,
    stop: CancellationToken,
    serving: JoinHandle<std::io::Result<()>>,
}

impl Server {
    /// Where it listens.
    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Shuts down gracefully and waits for every socket and background task.
    pub async fn stop(self) -> anyhow::Result<()> {
        self.stop.cancel();
        self.serving.await.context("joining the HTTP server")??;
        self.state.tracker.close();
        let grace = Duration::from_millis(self.state.config.ingest.shutdown_grace_ms);
        if tokio::time::timeout(grace, self.state.tracker.wait())
            .await
            .is_err()
        {
            tracing::warn!(
                tasks = self.state.tracker.len(),
                "shutdown grace expired with tasks running"
            );
        }
        self.state.store.close().await;
        tracing::info!("server stopped");
        Ok(())
    }
}

/// Starts the server; returns once it listens and is ready.
pub async fn start(config: Config) -> anyhow::Result<Server> {
    let store = crate::connect(&config)?;
    if config.database.migrate == MigrateMode::Auto {
        store.migrate().await.context("migrating on start")?;
    }
    match store.schema_status().await.context("checking the schema")? {
        SchemaStatus::Current => {}
        SchemaStatus::Behind { applied, known } => {
            bail!("the database has {applied} of {known} migrations: run `fleet migrate`")
        }
        SchemaStatus::Ahead => {
            bail!("the database schema is newer than this build: refusing to run on it")
        }
    }
    store
        .ensure_partitions(1, 7)
        .await
        .context("creating partitions")?;
    for p in &config.projects {
        store
            .upsert_project(&p.slug, &p.name)
            .await
            .with_context(|| format!("creating project {}", p.slug))?;
    }

    let listener = tokio::net::TcpListener::bind(config.listen)
        .await
        .with_context(|| format!("binding {}", config.listen))?;
    let addr = listener.local_addr()?;
    let state = AppState::new(store, config);
    // The live hub restarts from Postgres: versions continue where they were (§7.3).
    let open = state
        .hub
        .rebuild(&state.store)
        .await
        .context("restoring live versions")?;
    tracing::info!(open_attempts = open, "live hub restored");
    state
        .tracker
        .spawn(ingest::telemetry::flusher(state.clone()));
    state.tracker.spawn(sweeps::run(state.clone()));

    let router = Router::new()
        .merge(health::router())
        .merge(ingest::router(&state))
        .merge(api::router())
        .with_state(state.clone())
        .merge(web::router());
    let stop = CancellationToken::new();
    let serving = {
        let (stop, state) = (stop.clone(), state.clone());
        tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async move {
                    stop.cancelled().await;
                    state.set_ready(false);
                    // Upgraded sockets are not tracked by hyper: tell them directly.
                    state.shutdown.cancel();
                })
                .await
        })
    };
    state.set_ready(true);
    tracing::info!(listen = %addr, "server listening");
    Ok(Server {
        addr,
        state,
        stop,
        serving,
    })
}

/// `fleet serve`: runs until SIGINT or SIGTERM.
pub(crate) async fn serve(config: Config) -> anyhow::Result<()> {
    let server = start(config).await?;
    shutdown_signal().await;
    server.stop().await
}

/// Writes a secret to stdout once (`fleet api-key create`), never to the log.
pub(crate) fn print_once(secret: &str) -> anyhow::Result<()> {
    let mut out = std::io::stdout().lock();
    writeln!(out, "{secret}")?;
    out.flush()?;
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

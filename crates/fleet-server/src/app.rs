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
    api, artifacts,
    config::{Config, MigrateMode},
    health, ingest, login,
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
    /// The usercontent origin's own listener, when `usercontent.listen` is set.
    usercontent: Option<(SocketAddr, JoinHandle<std::io::Result<()>>)>,
}

impl Server {
    /// Where it listens.
    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Where the usercontent origin listens when it has a listener of its own.
    #[must_use]
    pub fn usercontent_addr(&self) -> Option<SocketAddr> {
        self.usercontent.as_ref().map(|(addr, _)| *addr)
    }

    /// Shuts down gracefully and waits for every socket and background task.
    pub async fn stop(self) -> anyhow::Result<()> {
        self.stop.cancel();
        self.serving.await.context("joining the HTTP server")??;
        if let Some((_, serving)) = self.usercontent {
            serving
                .await
                .context("joining the usercontent listener")??;
        }
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

/// Everything `start` does before it listens: migrations, partitions, configured projects and
/// group mappings, and the live hub rebuilt from Postgres. Tests drive [`router`] on it directly.
pub(crate) async fn init(config: Config) -> anyhow::Result<AppState> {
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
        let id = store
            .upsert_project(&p.slug, &p.name)
            .await
            .with_context(|| format!("creating project {}", p.slug))?;
        store
            .sync_trust_policies(id, &policies(p))
            .await
            .with_context(|| format!("storing the trust policies of {}", p.slug))?;
    }
    if let Some(login) = &config.login {
        sync_group_mappings(&store, login)
            .await
            .context("storing the group mappings")?;
    }
    let state = AppState::new(store, config)?;
    // The live hub restarts from Postgres: versions continue where they were (§7.3).
    let open = state
        .hub
        .rebuild(&state.store)
        .await
        .context("restoring live versions")?;
    tracing::info!(open_attempts = open, "live hub restored");
    // Say early that the blob store can't be opened; the first upload retries.
    if let Err(error) = state.blobs().await {
        tracing::warn!(
            error = format!("{error:#}"),
            "the artifact blob store is not available"
        );
    }
    Ok(state)
}

/// Makes the stored identity-provider group → role mappings equal the configured ones.
async fn sync_group_mappings(
    store: &fleet_store::Store,
    login: &crate::config::Login,
) -> anyhow::Result<()> {
    let mut rows = Vec::new();
    for g in &login.group_roles {
        let project = match &g.project {
            Some(slug) => Some(
                store
                    .project_id(slug)
                    .await?
                    .with_context(|| format!("no project {slug}"))?,
            ),
            None => None,
        };
        rows.push(fleet_store::sessions::GroupMapping {
            project,
            group: g.group.clone(),
            role: g.role,
        });
    }
    store.sync_group_mappings(&login.issuer, &rows).await?;
    Ok(())
}

/// Every route, with security headers on the dashboard's static files.
pub(crate) fn router(state: &AppState) -> Router {
    let app = Router::new()
        .merge(health::router())
        .merge(ingest::router(state))
        .merge(artifacts::router())
        .merge(api::router())
        .merge(login::router())
        .with_state(state.clone())
        .merge(web::router());
    // The usercontent host, when it shares this listener, gets artifacts and nothing else.
    match state.config.usercontent.hosts() {
        hosts if !hosts.is_empty() => {
            artifacts::usercontent::dispatch(app, artifacts::usercontent::router(state), hosts)
        }
        _ => app,
    }
}

/// Starts the server; returns once it listens and is ready.
pub async fn start(config: Config) -> anyhow::Result<Server> {
    let state = init(config).await?;
    let listener = tokio::net::TcpListener::bind(state.config.listen)
        .await
        .with_context(|| format!("binding {}", state.config.listen))?;
    let addr = listener.local_addr()?;
    state
        .tracker
        .spawn(ingest::telemetry::flusher(state.clone()));
    state.tracker.spawn(sweeps::run(state.clone()));

    let router = router(&state);
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
    let usercontent = match state.config.usercontent.listen {
        Some(listen) => {
            let listener = tokio::net::TcpListener::bind(listen)
                .await
                .with_context(|| format!("binding the usercontent origin to {listen}"))?;
            let bound = listener.local_addr()?;
            let router = artifacts::usercontent::router(&state);
            let stop = stop.clone();
            tracing::info!(listen = %bound, "usercontent origin listening");
            Some((
                bound,
                tokio::spawn(async move {
                    axum::serve(listener, router)
                        .with_graceful_shutdown(async move { stop.cancelled().await })
                        .await
                }),
            ))
        }
        None => None,
    };
    state.set_ready(true);
    tracing::info!(listen = %addr, "server listening");
    Ok(Server {
        addr,
        state,
        stop,
        serving,
        usercontent,
    })
}

fn policies(p: &crate::config::ProjectConfig) -> Vec<fleet_store::auth::PolicyRow> {
    use fleet_core::trust::{Policy, PrPolicy};
    p.trust_policies
        .iter()
        .map(|t| fleet_store::auth::PolicyRow {
            name: t.name.clone(),
            issuer: t.issuer.clone(),
            audience: t.audience.clone(),
            policy: Policy {
                repository_id: t.repository_id.clone(),
                repository_owner_id: t.repository_owner_id.clone(),
                trusted_refs: t.trusted_refs.clone(),
                allowed_events: t.allowed_events.clone(),
                pr_policy: match t.pr_policy.as_str() {
                    "full" => PrPolicy::Full,
                    "deny" => PrPolicy::Deny,
                    _ => PrPolicy::Restricted,
                },
                job_workflow_ref: t.job_workflow_ref.clone(),
                environment: t.environment.clone(),
            },
        })
        .collect()
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

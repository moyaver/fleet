// SPDX-License-Identifier: AGPL-3.0-only
//! Shared server state.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use fleet_blob::BlobStore;
use fleet_store::{Store, ingest::Change};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use crate::{
    artifacts::links::Signer, config::Config, ingest::telemetry::TelemetryBuffer, limits::Limits,
    live::Hub, login::idp::Idp, oidc::Jwks,
};

/// Cheap to clone; everything lives behind one `Arc`.
#[derive(Debug, Clone)]
pub(crate) struct AppState(Arc<Inner>);

#[derive(Debug)]
pub(crate) struct Inner {
    pub(crate) store: Store,
    pub(crate) config: Config,
    /// Cancelled on shutdown: sockets send `goaway` and close with 1012.
    pub(crate) shutdown: CancellationToken,
    /// Every socket and background task, so shutdown can wait for them.
    pub(crate) tracker: TaskTracker,
    pub(crate) telemetry: TelemetryBuffer,
    /// Per-IP and per-credential request limits, and the per-socket event rate.
    pub(crate) limits: Limits,
    pub(crate) hub: Hub,
    pub(crate) jwks: Jwks,
    /// The browser sign-in identity provider (discovery cache and token calls).
    pub(crate) idp: Idp,
    /// Signs and checks usercontent links; `None` without `usercontent.url`.
    pub(crate) signer: Option<Signer>,
    /// The artifact blob store, opened on first use (see [`AppState::blobs`]).
    blobs: tokio::sync::OnceCell<Arc<dyn BlobStore>>,
    /// A test's own store, which takes precedence over `blobs` (see [`AppState::install_blobs`]).
    #[cfg(test)]
    blobs_override: std::sync::OnceLock<Arc<dyn BlobStore>>,
    /// Readiness: false until start-up finished, and again while draining.
    ready: AtomicBool,
    /// Busy refusals of the ingest path, counted so their warning is rate-limited.
    pub(crate) busy: std::sync::Mutex<crate::ingest::BusyLog>,
}

impl std::ops::Deref for AppState {
    type Target = Inner;

    fn deref(&self) -> &Inner {
        &self.0
    }
}

impl AppState {
    pub(crate) fn new(store: Store, config: Config) -> anyhow::Result<Self> {
        let hub = Hub::new(
            std::time::Duration::from_millis(config.liveness.stale_after_ms),
            std::time::Duration::from_millis(config.liveness.sweep_interval_ms),
            config.api.live_buffer,
        );
        let limits = Limits::new(&config.rate_limits, std::time::Instant::now());
        Ok(Self(Arc::new(Inner {
            hub,
            jwks: Jwks::new()?,
            idp: Idp::new()?,
            signer: Signer::from_config(&config.usercontent),
            blobs: tokio::sync::OnceCell::new(),
            #[cfg(test)]
            blobs_override: std::sync::OnceLock::new(),
            store,
            config,
            shutdown: CancellationToken::new(),
            tracker: TaskTracker::new(),
            telemetry: TelemetryBuffer::default(),
            limits,
            ready: AtomicBool::new(false),
            busy: std::sync::Mutex::default(),
        })))
    }

    /// The artifact blob store. Opened on first use and then kept; a failure to open it (an
    /// unwritable directory, a bad S3 setting) is returned each time until it works.
    pub(crate) async fn blobs(&self) -> anyhow::Result<&Arc<dyn BlobStore>> {
        #[cfg(test)]
        if let Some(store) = self.blobs_override.get() {
            return Ok(store);
        }
        self.blobs
            .get_or_try_init(|| async { self.config.storage.open() })
            .await
    }

    /// Replaces the blob store for this server. Tests use it for a store the configuration cannot
    /// name (one that stages direct uploads in memory); start-up has already opened the
    /// configured one, so the override is consulted first.
    #[cfg(test)]
    pub(crate) fn install_blobs(&self, store: Arc<dyn BlobStore>) {
        let _ = self.blobs_override.set(store);
    }

    pub(crate) fn is_ready(&self) -> bool {
        self.ready.load(Ordering::SeqCst) && !self.shutdown.is_cancelled()
    }

    pub(crate) fn set_ready(&self, ready: bool) {
        self.ready.store(ready, Ordering::SeqCst);
    }

    /// Hands a committed change to the live hub. Called after the ack, never before commit.
    pub(crate) fn publish(&self, change: Option<Change>) {
        let Some(change) = change else { return };
        let state = self.clone();
        self.tracker.spawn(async move {
            state.hub.publish(&state.store, change).await;
        });
    }
}

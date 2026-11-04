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
    artifacts::links::Signer, config::Config, ingest::telemetry::TelemetryBuffer, live::Hub,
    login::idp::Idp, oidc::Jwks,
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
    pub(crate) hub: Hub,
    pub(crate) jwks: Jwks,
    /// The browser sign-in identity provider (discovery cache and token calls).
    pub(crate) idp: Idp,
    /// Signs and checks usercontent links; `None` without `usercontent.url`.
    pub(crate) signer: Option<Signer>,
    /// The artifact blob store, opened on first use (see [`AppState::blobs`]).
    blobs: tokio::sync::OnceCell<Arc<dyn BlobStore>>,
    /// Readiness: false until start-up finished, and again while draining.
    ready: AtomicBool,
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
            config.api.live_buffer,
        );
        Ok(Self(Arc::new(Inner {
            hub,
            jwks: Jwks::new()?,
            idp: Idp::new()?,
            signer: Signer::from_config(&config.usercontent),
            blobs: tokio::sync::OnceCell::new(),
            store,
            config,
            shutdown: CancellationToken::new(),
            tracker: TaskTracker::new(),
            telemetry: TelemetryBuffer::default(),
            ready: AtomicBool::new(false),
        })))
    }

    /// The artifact blob store. Opened on first use and then kept; a failure to open it (an
    /// unwritable directory, a bad S3 setting) is returned each time until it works.
    pub(crate) async fn blobs(&self) -> anyhow::Result<&Arc<dyn BlobStore>> {
        self.blobs
            .get_or_try_init(|| async { self.config.storage.open() })
            .await
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

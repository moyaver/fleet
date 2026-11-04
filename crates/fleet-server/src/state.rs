// SPDX-License-Identifier: AGPL-3.0-only
//! Shared server state.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use fleet_store::{Store, ingest::Change};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use crate::{config::Config, ingest::telemetry::TelemetryBuffer};

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
    pub(crate) fn new(store: Store, config: Config) -> Self {
        Self(Arc::new(Inner {
            store,
            config,
            shutdown: CancellationToken::new(),
            tracker: TaskTracker::new(),
            telemetry: TelemetryBuffer::default(),
            ready: AtomicBool::new(false),
        }))
    }

    pub(crate) fn is_ready(&self) -> bool {
        self.ready.load(Ordering::SeqCst) && !self.shutdown.is_cancelled()
    }

    pub(crate) fn set_ready(&self, ready: bool) {
        self.ready.store(ready, Ordering::SeqCst);
    }

    /// Hands a committed change to the live layer. Called after the ack, never before commit.
    #[expect(
        clippy::unused_self,
        reason = "the live hub, next, publishes through the state"
    )]
    pub(crate) fn publish(&self, change: Option<Change>) {
        if let Some(change) = change {
            tracing::trace!(attempt = %change.pipeline_attempt_id, version = change.version,
                entities = change.entities.len(), "committed change");
        }
    }
}

// SPDX-License-Identifier: AGPL-3.0-only
//! The artifact broker (PLAN §6.6, §7.6; ADR-0007).
//!
//! - `upload`: the agent side, three calls (`fleet_protocol::artifact`): `upload_request` → grant,
//!   `PUT` the bytes (relayed through the server into a spool file, counted as they arrive), then
//!   `upload_complete`, which verifies size, SHA-256, magic bytes and archive safety
//!   (`fleet_blob::verify`) and only then writes the content-addressed blob.
//! - [`list`]: `GET /api/v1/projects/{project}/flows/{id}/artifacts`, with signed links.
//! - `links` and `usercontent`: the second origin that is the only place artifact bytes are
//!   served from, behind short-lived signed links, sandboxed and cookieless (invariant I7).
//!
//! The app origin never serves artifact bytes: its routes list and describe artifacts, and the
//! links they hand out point at the usercontent origin.

pub(crate) mod links;
pub(crate) mod list;
pub(crate) mod upload;
pub(crate) mod usercontent;

use axum::{
    Router,
    extract::DefaultBodyLimit,
    routing::{post, put},
};

use crate::state::AppState;

pub(crate) use upload::purge_spool;

/// The agent upload routes (publisher credential).
pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/agent/artifacts", post(upload::request))
        .route(
            "/api/v1/agent/artifacts/{artifact}/content",
            // The body is streamed and counted against the declared size, not buffered.
            put(upload::content).layer(DefaultBodyLimit::disable()),
        )
        .route(
            "/api/v1/agent/artifacts/{artifact}/complete",
            post(upload::complete),
        )
}

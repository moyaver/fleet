// SPDX-License-Identifier: AGPL-3.0-only
//! The REST read models behind screens A–D over HTTP (seam S4): the real router on a fresh
//! Postgres database, driven in-process with axum-test. Every row is produced through the real
//! ingest path (`POST /api/v1/agent/batch`, the agent socket, or `fleet-loadgen`); expected
//! values come from the spec and from worked literal examples.

mod access;
mod artifacts;
mod compare;
mod fleet;
mod flows;
mod history;
mod live_recovery;
mod metrics;
mod openapi;
mod overview;
mod plans;
mod retention;
mod rig;
mod timeline;
mod usercontent;
mod viewer_limits;
mod workers;

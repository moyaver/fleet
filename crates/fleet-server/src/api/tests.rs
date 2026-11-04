// SPDX-License-Identifier: AGPL-3.0-only
//! The REST read models behind screens A–D over HTTP (seam S4): the real router on a fresh
//! Postgres database, driven in-process with axum-test. Every row is produced through the real
//! ingest path (`POST /api/v1/agent/batch`, the agent socket, or `fleet-loadgen`); expected
//! values come from the spec and from worked literal examples.

mod access;
mod artifacts;
mod fleet;
mod flows;
mod metrics;
mod openapi;
mod overview;
mod retention;
mod rig;
mod timeline;
mod usercontent;
mod workers;

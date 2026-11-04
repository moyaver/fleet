// SPDX-License-Identifier: AGPL-3.0-only
//! Queries behind the dashboard's screens A–D: overview, timeline, worker and device detail,
//! flow investigation (PLAN §10.3). The views are `fleet_protocol::rest`.
//!
//! Every query is scoped by project: an ID that exists under another project reads as missing.
//! Display liveness lives in the server's memory (heartbeats never touch the database), so
//! workers come back as a [`WorkerRow`]: the view plus the runs the server derives liveness from.

mod flow;
mod overview;
mod samples;
mod timeline;
mod workers;

pub use overview::{Overview, RunState, WorkerRow};
pub use samples::{DeviceRow, HostRow, SAMPLE_LIMIT, Samples};
pub use workers::{RunCursor, WorkerDetail};

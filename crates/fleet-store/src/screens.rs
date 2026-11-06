// SPDX-License-Identifier: AGPL-3.0-only
//! Queries behind the dashboard's screens A–D: overview, timeline, worker and device detail,
//! flow investigation (PLAN §10.3), and screens E and F (history and compare). The views are
//! `fleet_protocol::rest`.
//!
//! Every query is scoped by project: an ID that exists under another project reads as missing.
//! Display liveness lives in the server's memory (heartbeats never touch the database), so
//! workers come back as a [`WorkerRow`]: the view plus the runs the server derives liveness from.

mod compare;
mod flow;
mod history;
mod overview;
mod samples;
mod timeline;
mod workers;

pub use compare::{CompareSide, SIDE_LIMIT, SideFlow};
pub use history::{FlowStatsData, HistoryCursor, HistoryData, HistoryQuery, LOOKBACK};
pub use overview::{Overview, RunState, WorkerRow};
pub use samples::{DeviceRow, HostRow, ProcessRow, SAMPLE_LIMIT, Samples};
pub use workers::{RunCursor, WorkerDetail};

// SPDX-License-Identifier: AGPL-3.0-only
//! Wire types shared by the agent, the server and the dashboard. See PLAN §8, ADR-0005 and
//! research 05 §3–§5.
//!
//! This crate must stay free of async runtimes and build for `wasm32-unknown-unknown` (PLAN §4.2).
//!
//! - [`artifact`]: the artifact upload protocol (HTTPS, never the socket).
//! - [`frame`]: what travels over the agent WebSocket (`fleet.v1+json`).
//! - [`journal`]: the envelope every journal event shares, with its provenance.
//! - [`event`]: the typed bodies the agent emits.
//! - [`live`]: the browser's live socket: snapshots, deltas and their read models.
//! - [`rest`]: REST read models of the dashboard's screens A–D (overview, timeline, worker and
//!   device detail, metric series, flow investigation).
//! - `schema` (feature `schema`): the JSON Schema documents generated from these types.
//! - [`plan`]: Fleet Planner manifests (ADR-0013).

pub mod artifact;
pub mod brand;
pub mod event;
pub mod frame;
pub mod journal;
pub mod live;
pub mod plan;
pub mod rest;
#[cfg(feature = "schema")]
pub mod schema;

/// The protocol version this build speaks.
pub const PROTOCOL_VERSION: u16 = 1;

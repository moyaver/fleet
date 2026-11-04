// SPDX-License-Identifier: AGPL-3.0-only
//! Wire types shared by the agent, the server and the dashboard. See PLAN §8, ADR-0005 and
//! research 05 §3–§5.
//!
//! This crate must stay free of async runtimes and build for `wasm32-unknown-unknown` (PLAN §4.2).
//!
//! - [`frame`]: what travels over the agent WebSocket (`fleet.v1+json`).
//! - [`journal`]: the envelope every journal event shares, with its provenance.
//! - [`event`]: the typed bodies the agent emits.

pub mod brand;
pub mod event;
pub mod frame;
pub mod journal;

/// The protocol version this build speaks.
pub const PROTOCOL_VERSION: u16 = 1;

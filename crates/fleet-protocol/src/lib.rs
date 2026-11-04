// SPDX-License-Identifier: AGPL-3.0-only
//! Wire types shared by the agent, the server and the dashboard. See PLAN §8 and ADR-0005.
//!
//! This crate must stay free of async runtimes and build for `wasm32-unknown-unknown` (PLAN §4.2).

pub mod brand;

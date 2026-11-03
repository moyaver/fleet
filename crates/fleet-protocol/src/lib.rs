// SPDX-License-Identifier: LicenseRef-PolyForm-Internal-Use-1.0.0
//! Wire types shared by the agent, the server and the dashboard. See PLAN §8 and ADR-0005.
//!
//! This crate must stay free of async runtimes and build for `wasm32-unknown-unknown` (PLAN §4.2).

pub mod brand;

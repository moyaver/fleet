// SPDX-License-Identifier: AGPL-3.0-only
//! Full-stack tests: the real agent binary against the real server (needs `DATABASE_URL`).
#![expect(
    clippy::unwrap_used,
    reason = "test helpers: an unexpected error should fail the test right there"
)]

mod artifacts;
mod browser;
mod dashboard_ab;
mod dashboard_cd;
mod dashboard_ef;
mod fixtures;
mod network;
mod scenarios;
mod support;

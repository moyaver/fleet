// SPDX-License-Identifier: AGPL-3.0-only
//! `fleet-blob` through its public API: one storage contract run against every backend (S8)
//! and artifact verification over crafted inputs (S7).

#![expect(
    clippy::unwrap_used,
    reason = "test helpers: an unexpected error should fail the test right there"
)]

mod contract;
mod garage;
mod local;
mod s3_config;
mod verify;

// SPDX-License-Identifier: AGPL-3.0-only
//! Server integration tests: the real server on a fresh Postgres 18 database per test, driven
//! over the wire (research 05 §3.4–§3.9). Needs `DATABASE_URL` (CI's service, or
//! `cargo xtask ci`'s throwaway container).
#![expect(
    clippy::unwrap_used,
    reason = "test helpers: an unexpected error should fail the test right there"
)]

mod ingest;
mod live;
mod support;

// SPDX-License-Identifier: AGPL-3.0-only
//! Integration tests for the `fleet-agent` binary: one test binary, one module per area.
#![expect(
    clippy::unwrap_used,
    reason = "test helpers: an unexpected error should fail the test right there"
)]

mod inspect;
mod stream;
mod summary;
mod supervisor;

// SPDX-License-Identifier: AGPL-3.0-only
//! Parsers against captured Maestro fixtures (PLAN §13: every fixture × every parser).
#![expect(
    clippy::unwrap_used,
    reason = "test code: an unexpected shape should fail the test right there"
)]

mod fixtures;

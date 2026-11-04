// SPDX-License-Identifier: AGPL-3.0-only
//! Pure domain logic for Fleet (PLAN §7.4). No I/O.
//!
//! - [`claims`]: the claim lattice that decides a flow's outcome (invariants I3, I4).
//! - [`redact`]: secret redaction for everything that leaves the runner (invariant I6).
//! - [`reconcile`]: matching the final JUnit report to the flows seen live.
//! - [`outcome`]: suite and job outcomes, which never read as successful by default (I3).
//! - [`policy`]: who may do what: the role × action table behind `authorize` (PLAN §7.5).
//! - [`trust`]: publisher trust policies for CI OIDC identities (ADR-0008).
#![warn(
    clippy::indexing_slicing,
    clippy::string_slice,
    clippy::expect_used,
    clippy::panic
)]

pub mod claims;
pub mod outcome;
pub mod policy;
pub mod reconcile;
pub mod redact;
pub mod trust;

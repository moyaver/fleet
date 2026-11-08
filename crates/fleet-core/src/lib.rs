// SPDX-License-Identifier: AGPL-3.0-only
//! Pure domain logic for Fleet (PLAN §7.4). No I/O.
//!
//! - [`downsample`]: LTTB downsampling of chart series (PLAN §5.4).
//! - [`claims`]: the claim lattice that decides a flow's outcome (invariants I3, I4).
//! - [`redact`]: secret redaction for everything that leaves the runner (invariant I6).
//! - [`reconcile`]: matching the final JUnit report to the flows seen live.
//! - [`outcome`]: suite and job outcomes, which never read as successful by default (I3).
//! - [`policy`]: who may do what: the role × action table behind `authorize` (PLAN §7.5).
//! - [`fingerprint`]: failure and condition fingerprints (ADR-0009).
//! - [`flaky`]: flaky v1 classification over a run history (ADR-0009).
//! - [`trust`]: publisher trust policies for CI OIDC identities (ADR-0008).
//! - [`plan`]: what an equal-count split of a plan would have produced (ADR-0013, #81).
//! - [`reproduce`]: the rerun command of a flow and the manifest of a run's failed flows (#82).
#![warn(
    clippy::indexing_slicing,
    clippy::string_slice,
    clippy::expect_used,
    clippy::panic
)]

pub mod claims;
pub mod downsample;
pub mod fingerprint;
pub mod flaky;
pub mod outcome;
pub mod plan;
pub mod policy;
pub mod reconcile;
pub mod redact;
pub mod reproduce;
pub mod trust;

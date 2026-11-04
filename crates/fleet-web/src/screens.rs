// SPDX-License-Identifier: AGPL-3.0-only
//! The dashboard's screens.
//!
//! - [`overview`]: screen A, the fleet overview.
//! - [`pipeline`]: screen B, one pipeline attempt.
//! - [`common`]: wording and mappings shared by screens C and D (outcome provenance, sources).
//! - [`metrics`]: metric charts and the range picker, shared by the worker and device pages.
//! - [`parts`]: Leptos pieces shared by the pages of screens C and D.
//! - [`routes`]: the routes of screens C and D, fetching with the REST client.
//! - [`worker`], [`device`]: screen C. [`flow`]: screen D.

pub mod common;
pub mod device;
pub mod flow;
pub mod metrics;
pub mod overview;
pub mod parts;
pub mod pipeline;
pub mod routes;
pub mod timeline;
pub mod worker;

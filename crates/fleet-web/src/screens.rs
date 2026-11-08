// SPDX-License-Identifier: AGPL-3.0-only
//! The dashboard's screens.
//!
//! - [`overview`]: screen A, the fleet overview.
//! - [`pipeline`]: screen B, one pipeline attempt.
//! - [`common`]: wording and mappings shared by screens C and D (outcome provenance, sources).
//! - [`metrics`]: metric charts and the range picker, shared by the worker and device pages.
//! - [`parts`]: Leptos pieces shared by the pages of screens C and D.
//! - [`history`]: screen E. [`compare`]: screen F. [`project`]: which project they are about.
//! - [`routes`]: the routes of screens C and D, fetching with the REST client.
//! - [`admin`]: Screen G, the administration pages (global administrators only).
//! - [`worker`], [`device`]: screen C. [`flow`]: screen D. [`triage`]: a failure's recorded state,
//!   shown on D and E.

pub mod admin;
pub mod common;
pub mod compare;
pub mod device;
pub mod flow;
pub mod history;
pub mod metrics;
pub mod overview;
pub mod parts;
pub mod pipeline;
pub mod project;
pub mod rerun;
pub mod routes;
pub mod timeline;
pub mod triage;
pub mod worker;

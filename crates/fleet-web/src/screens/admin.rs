// SPDX-License-Identifier: AGPL-3.0-only
//! Screen G, administration (spec §8; PLAN §10.3, M6): the installation-wide pages a global
//! administrator reads and changes. Each area is one table over the REST reads of
//! `/api/v1/admin/…`, and the areas that can change carry forms that post to its write routes;
//! the server refuses everyone who is not a global administrator.
//!
//! - [`model`]: what each value reads as (durations, scopes, statuses) and which links show. No
//!   Leptos.
//! - [`forms`]: the plain forms and row actions that change an area (posted to the write routes).
//! - [`view`]: the tables and sections.
//! - [`route`]: the `/admin` page (every area) and `/admin/audit` (the audit history).

pub mod forms;
pub mod model;
pub mod route;
pub mod view;

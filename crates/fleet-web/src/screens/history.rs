// SPDX-License-Identifier: AGPL-3.0-only
//! Screen E: test history (spec §8). A filter bar synced to the URL, the finished flow executions
//! with their provenance, fingerprints and flaky markers, and a per-flow panel with a duration
//! chart.
//!
//! - [`model`]: the URL query, the REST query it makes, and the rows and panel as text. No Leptos.
//! - [`view`]: the Leptos pieces over the model.
//! - [`route`]: the `/history` route, fetching with [`crate::api`].

pub mod model;
pub mod route;
pub mod view;

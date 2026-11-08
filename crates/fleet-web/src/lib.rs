// SPDX-License-Identifier: AGPL-3.0-only
//! The Fleet dashboard (Leptos CSR, PLAN §10).
//!
//! The pure parts (paths, progress, status wording, reconnect backoff, frame coalescing, the
//! per-entity key plan and each page's view-model) have no Leptos and no browser APIs and are
//! tested natively. The Leptos views and the browser I/O (`api`, `live::socket`) are thin
//! layers over them; DOM behaviour is covered by the browser end-to-end tests in `fleet-e2e`.
//!
//! - [`app`]: the shell, router and sign-in gate.
//! - [`api`]: the REST client over `fleet_protocol` types.
//! - [`live`]: the browser live socket, its reconnect policy and the reactive store adapter.
//! - [`screens`]: the pages.

pub mod announce;
pub mod api;
pub mod app;
pub mod clock;
pub mod format;
pub mod live;
pub mod paths;
pub mod progress;
pub mod remote;
pub mod screens;
pub mod status;
pub mod throttle;

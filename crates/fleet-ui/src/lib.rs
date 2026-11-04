// SPDX-License-Identifier: AGPL-3.0-only
//! Leptos component kit. See PLAN §10.2.
//!
//! Every non-trivial component has a pure view-model module (no Leptos, no browser APIs) that is
//! tested natively, and a thin Leptos view over it in [`components`]. User text is only ever
//! rendered as text nodes, never as HTML (PLAN §10.1).
//!
//! - [`live_store`]: the dashboard's client-side mirror of the live socket, pure Rust.

pub mod components;
pub mod live_store;
pub mod pagination;
pub mod provenance;
pub mod table;
pub mod tone;
pub mod virtual_list;

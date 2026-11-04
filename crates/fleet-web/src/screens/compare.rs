// SPDX-License-Identifier: AGPL-3.0-only
//! Screen F: compare two pipeline attempts (spec §8). Every flow of either side, grouped into
//! newly failing, recovered, changed, only in A, only in B and unchanged, with each mismatch
//! explained from the server's evidence only.
//!
//! - [`model`]: the attempt pickers, the sections and the wording of every explanation. No Leptos.
//! - [`view`]: the Leptos pieces over the model.
//! - [`route`]: the `/compare` route, fetching with [`crate::api`].

pub mod model;
pub mod route;
pub mod view;

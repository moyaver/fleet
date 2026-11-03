// SPDX-License-Identifier: LicenseRef-PolyForm-Internal-Use-1.0.0
//! The built dashboard, embedded at compile time.
//!
//! Debug builds read `crates/fleet-web/dist` from disk at runtime, so `cargo xtask web` is
//! enough to see changes. If `dist` doesn't exist yet, the embed is empty and the server answers 404.

#[derive(rust_embed::Embed)]
#[folder = "../fleet-web/dist"]
#[allow_missing = true]
pub struct Dist;

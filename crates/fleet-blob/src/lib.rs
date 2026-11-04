// SPDX-License-Identifier: AGPL-3.0-only
//! Blob storage and artifact verification (PLAN §7.6, ADR-0007).
//!
//! - [`BlobStore`]: the storage contract. Blobs are **content-addressed per project**: a
//!   [`BlobKey`] is a project id plus the SHA-256 of the bytes, so a key can neither escape its
//!   backend's namespace nor name different content.
//! - [`LocalFs`]: a directory on disk (small installs, tests). Writes are atomic: temp file,
//!   `fsync`, rename.
//! - [`S3`]: any S3-compatible store through `object_store`, path-style with a custom endpoint
//!   (Garage is the reference, `SeaweedFS` the alternative). Nothing provider-specific (I8).
//! - [`verify`]: pure checks on an uploaded artifact before it is stored: size, SHA-256, magic
//!   bytes per artifact class, and archive safety (zip bombs, path traversal, links).
//!
//! `put` verifies size and digest *while streaming*: a body that does not match its key is never
//! stored. The upload broker (M5-N) calls [`verify::verify`] on the spooled body first, then
//! [`BlobStore::put`].
#![warn(
    clippy::indexing_slicing,
    clippy::string_slice,
    clippy::expect_used,
    clippy::panic
)]

mod check;
mod error;
mod key;
mod local;
mod s3;
mod store;
pub mod verify;

pub use error::BlobError;
pub use key::{BlobKey, Sha256Digest};
pub use local::LocalFs;
pub use s3::{S3, S3Config};
pub use store::{BlobInfo, BlobRead, BlobStore, ByteStream, PutOutcome, Scope};

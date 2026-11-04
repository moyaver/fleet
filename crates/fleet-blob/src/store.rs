// SPDX-License-Identifier: AGPL-3.0-only
//! The storage contract.

use std::{io, ops::Range};

use bytes::Bytes;
use futures_util::stream::BoxStream;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{BlobError, BlobKey};

/// A body in flight: chunks of bytes, or the I/O error that cut the stream short.
pub type ByteStream<'a> = BoxStream<'a, io::Result<Bytes>>;

/// What [`BlobStore::put`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PutOutcome {
    /// The blob is new.
    Created,
    /// The project already had these bytes. The body was still checked, nothing was rewritten.
    AlreadyExists,
}

/// Which blobs [`BlobStore::list`] visits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Every project (the garbage collector's sweep).
    All,
    /// One project.
    Project(Uuid),
}

/// What a store knows about a blob without reading it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobInfo {
    /// The blob.
    pub key: BlobKey,
    /// Size in bytes.
    pub size: u64,
    /// When it was written; the garbage collector's grace period counts from here.
    pub last_modified: OffsetDateTime,
}

/// A blob, or a range of it, being read.
pub struct BlobRead {
    /// The whole blob's size (the `/total` of a `Content-Range`).
    pub size: u64,
    /// The bytes `body` yields, as offsets into the blob.
    pub range: Range<u64>,
    /// The bytes themselves.
    pub body: ByteStream<'static>,
}

impl std::fmt::Debug for BlobRead {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlobRead")
            .field("size", &self.size)
            .field("range", &self.range)
            .finish_non_exhaustive()
    }
}

/// Content-addressed blob storage (ADR-0007).
///
/// Every backend passes the same contract suite (`tests/it/contract.rs`). Keys carry their own
/// content digest, so there is no way to overwrite a blob with different bytes.
#[async_trait::async_trait]
pub trait BlobStore: Send + Sync + std::fmt::Debug {
    /// Stores `body` under `key`, after checking it is exactly `size` bytes hashing to the key's
    /// digest. On any mismatch or stream error nothing is stored and a previously stored blob
    /// is untouched. Storing the same blob twice is not an error ([`PutOutcome::AlreadyExists`]).
    ///
    /// # Cancel safety
    /// Dropping the future leaves no visible blob. A local backend removes its temp file; an
    /// object store may keep an unfinished multipart upload until its bucket lifecycle rule
    /// expires it.
    async fn put(
        &self,
        key: &BlobKey,
        size: u64,
        body: ByteStream<'_>,
    ) -> Result<PutOutcome, BlobError>;

    /// Reads the blob, or `range` of it. The end of a range past the blob's end is clamped (HTTP
    /// semantics); a start at or past the end is [`BlobError::RangeNotSatisfiable`].
    async fn get(&self, key: &BlobKey, range: Option<Range<u64>>) -> Result<BlobRead, BlobError>;

    /// Size and age of the blob, or `None` if it does not exist.
    async fn head(&self, key: &BlobKey) -> Result<Option<BlobInfo>, BlobError>;

    /// Removes the blob. Removing a missing blob is not an error (retention and GC retry).
    async fn delete(&self, key: &BlobKey) -> Result<(), BlobError>;

    /// Lists blobs for garbage collection, in no particular order. Objects that are not Fleet
    /// blobs (temp files, foreign keys in a shared bucket) are skipped.
    fn list(&self, scope: Scope) -> BoxStream<'_, Result<BlobInfo, BlobError>>;
}

/// Validates a read range against the blob's size; the same rule for every backend.
pub(crate) fn resolve_range(size: u64, range: Option<Range<u64>>) -> Result<Range<u64>, BlobError> {
    let Some(range) = range else {
        return Ok(0..size);
    };
    if range.start >= size || range.start >= range.end {
        return Err(BlobError::RangeNotSatisfiable { size });
    }
    Ok(range.start..range.end.min(size))
}

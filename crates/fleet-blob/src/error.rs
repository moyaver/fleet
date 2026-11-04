// SPDX-License-Identifier: AGPL-3.0-only
//! The error type shared by every backend.

use crate::key::Sha256Digest;

/// Why a blob operation failed.
#[derive(Debug, thiserror::Error)]
pub enum BlobError {
    /// No blob with this key.
    #[error("blob not found")]
    NotFound,
    /// The body's length is not the declared size. Nothing was stored.
    #[error("blob size mismatch: declared {declared} bytes, received {received}")]
    SizeMismatch {
        /// Bytes the caller promised.
        declared: u64,
        /// Bytes seen (at least this many when the body was cut short for being too long).
        received: u64,
    },
    /// The body's SHA-256 is not the key's. Nothing was stored.
    #[error("blob digest mismatch: key is {expected}, content hashes to {actual}")]
    DigestMismatch {
        /// The digest in the key.
        expected: Sha256Digest,
        /// The digest of the received bytes.
        actual: Sha256Digest,
    },
    /// The requested byte range starts at or past the end of the blob (HTTP 416).
    #[error("range not satisfiable for a blob of {size} bytes")]
    RangeNotSatisfiable {
        /// The blob's size.
        size: u64,
    },
    /// The body stream failed (for example the client went away).
    #[error("reading the blob body")]
    Body(#[source] std::io::Error),
    /// The backend failed: disk or network. The source says which.
    #[error("blob backend: {context}")]
    Backend {
        /// The operation that failed.
        context: &'static str,
        /// The underlying error.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// The configuration cannot make a store.
    #[error("blob store configuration: {0}")]
    Config(String),
}

impl BlobError {
    pub(crate) fn backend(
        context: &'static str,
        source: impl Into<Box<dyn std::error::Error + Send + Sync>>,
    ) -> Self {
        Self::Backend {
            context,
            source: source.into(),
        }
    }
}

// SPDX-License-Identifier: AGPL-3.0-only
//! The streaming size and digest check every backend applies to a `put` body.

use sha2::{Digest, Sha256};

use crate::{BlobError, BlobKey, key::Sha256Digest};

/// Counts and hashes a body as it streams by. Fails as soon as the body is longer than declared,
/// so an endless stream cannot fill a disk or a buffer.
pub(crate) struct Check {
    declared: u64,
    expected: Sha256Digest,
    received: u64,
    hasher: Sha256,
}

impl Check {
    pub(crate) fn new(key: &BlobKey, declared: u64) -> Self {
        Self {
            declared,
            expected: key.digest(),
            received: 0,
            hasher: Sha256::new(),
        }
    }

    /// Accounts for one chunk.
    pub(crate) fn feed(&mut self, chunk: &[u8]) -> Result<(), BlobError> {
        self.received = self.received.saturating_add(chunk.len() as u64);
        if self.received > self.declared {
            return Err(BlobError::SizeMismatch {
                declared: self.declared,
                received: self.received,
            });
        }
        self.hasher.update(chunk);
        Ok(())
    }

    /// The body ended: it must be exactly the declared size with exactly the key's digest.
    pub(crate) fn finish(self) -> Result<(), BlobError> {
        if self.received != self.declared {
            return Err(BlobError::SizeMismatch {
                declared: self.declared,
                received: self.received,
            });
        }
        let actual = Sha256Digest::from_bytes(self.hasher.finalize().into());
        if actual == self.expected {
            Ok(())
        } else {
            Err(BlobError::DigestMismatch {
                expected: self.expected,
                actual,
            })
        }
    }
}

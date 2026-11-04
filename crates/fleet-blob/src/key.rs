// SPDX-License-Identifier: AGPL-3.0-only
//! Content-addressed keys: `(project, sha256)`.

use std::fmt;

use sha2::{Digest, Sha256};
use uuid::Uuid;

/// A SHA-256 digest. Its text form is 64 lowercase hex digits and nothing else, so a parsed
/// digest can never contain a path separator or `..`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Sha256Digest([u8; 32]);

impl Sha256Digest {
    /// The digest of `bytes`.
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }

    pub(crate) fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Parses 64 lowercase hex digits; anything else (uppercase, wrong length, separators) is
    /// refused, so there is exactly one text form per digest.
    #[must_use]
    pub fn from_hex(text: &str) -> Option<Self> {
        let bytes = text.as_bytes();
        if bytes.len() != 64 {
            return None;
        }
        let mut out = [0_u8; 32];
        for (slot, [hi, lo]) in out.iter_mut().zip(bytes.as_chunks::<2>().0) {
            *slot = (nibble(*hi)? << 4) | nibble(*lo)?;
        }
        Some(Self(out))
    }

    /// The raw 32 bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

fn nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    }
}

impl fmt::Display for Sha256Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|b| write!(f, "{b:02x}"))
    }
}

impl fmt::Debug for Sha256Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Sha256Digest({self})")
    }
}

/// Names one blob: the project it belongs to and the SHA-256 of its bytes.
///
/// The same bytes in two projects are two blobs (a project can never read or delete another's),
/// and uploading the same bytes twice to one project stores them once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlobKey {
    project: Uuid,
    digest: Sha256Digest,
}

impl BlobKey {
    /// The key of the blob with this content in this project.
    #[must_use]
    pub fn new(project: Uuid, digest: Sha256Digest) -> Self {
        Self { project, digest }
    }

    /// The owning project.
    #[must_use]
    pub fn project(&self) -> Uuid {
        self.project
    }

    /// The content digest.
    #[must_use]
    pub fn digest(&self) -> Sha256Digest {
        self.digest
    }

    /// The backend-relative path: `<project>/<first two hex digits>/<digest>`. The two-digit
    /// level keeps directories small on a filesystem; object stores ignore it.
    pub(crate) fn path(&self) -> String {
        let hex = self.digest.to_string();
        let shard = hex.get(..2).unwrap_or_default();
        format!("{}/{shard}/{hex}", self.project.hyphenated())
    }

    /// Inverse of [`path`](Self::path); `None` for anything else, including a wrong shard.
    pub(crate) fn from_path(path: &str) -> Option<Self> {
        let mut parts = path.split('/');
        let (project, shard, hex) = (parts.next()?, parts.next()?, parts.next()?);
        if parts.next().is_some() || project.len() != 36 {
            return None;
        }
        let key = Self::new(Uuid::parse_str(project).ok()?, Sha256Digest::from_hex(hex)?);
        (key.path() == path && hex.starts_with(shard)).then_some(key)
    }
}

#[cfg(test)]
mod tests {
    use super::Sha256Digest;

    /// SHA-256 of the empty input (FIPS 180-4 test vector).
    const EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    #[test]
    fn digests_have_one_lowercase_hex_form() {
        let d = Sha256Digest::of(b"");
        assert_eq!(d.to_string(), EMPTY);
        assert_eq!(Sha256Digest::from_hex(EMPTY), Some(d));
        assert_eq!(Sha256Digest::from_hex(&EMPTY.to_uppercase()), None);
        assert_eq!(Sha256Digest::from_hex("../../etc/passwd"), None);
        assert_eq!(Sha256Digest::from_hex(EMPTY.get(1..).unwrap()), None);
        let with_slash = format!("/{}", EMPTY.get(1..).unwrap());
        assert_eq!(Sha256Digest::from_hex(&with_slash), None);
    }
}

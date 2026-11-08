// SPDX-License-Identifier: AGPL-3.0-only
//! Artifact verification before storage (PLAN §7.6 step 1, research 06 threat T11).
//!
//! [`verify`] takes a spooled upload (anything `Read + Seek`: a temp file, a `Cursor`) and
//! answers one question: is this what the uploader declared, and is it safe to keep and serve?
//! It checks, in order:
//!
//! 1. **Size:** the declared size equals the actual length and is within [`Limits::max_bytes`].
//! 2. **Type:** magic bytes (`infer`) must be on the allowlist for the artifact's class
//!    ([`ArtifactClass`]). A `.png` that is a zip is refused, whatever its name or `Content-Type`.
//!    Logs have no magic: they must be UTF-8 text without NUL bytes.
//! 3. **Digest:** the SHA-256 of the actual bytes equals the declared one.
//! 4. **Archive safety** for zip and `tar.zst` (see [`ArchiveError`]): entry count, total
//!    uncompressed bytes, per-entry compression ratio, unsafe paths, links and devices, nested
//!    archives. Sizes in archive headers are never trusted: entries are decompressed into a
//!    counter and stopped at the limit, so a header that lies cannot bypass a check.
//!
//! Nothing is ever extracted to disk, and nested archives are rejected rather than expanded
//! (depth 0). Every function here treats its input as hostile: malformed input is an `Err`,
//! never a panic, and memory use is bounded by the limits.
//!
//! This module is a pure library step: the upload broker (M5-N) spools the body, calls
//! [`verify`], and only then hands the same bytes to [`BlobStore::put`](crate::BlobStore::put).

mod archive;

use std::{
    fmt,
    io::{self, Read, Seek, SeekFrom},
};

use sha2::{Digest, Sha256};

use crate::key::Sha256Digest;
pub use archive::{ArchiveError, ArchiveSummary};

/// What an artifact is for; each class has its own allowlist of file types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArtifactClass {
    /// A failure or step screenshot: PNG, JPEG or WebP.
    Screenshot,
    /// A JUnit report: XML.
    Junit,
    /// Maestro's HTML report: one HTML file, or a zip of the report directory.
    HtmlReport,
    /// A log file: UTF-8 text.
    Log,
    /// A debug bundle: a zip, or a `tar.zst` (the agent's offline bundle).
    Bundle,
}

impl ArtifactClass {
    /// The class's name as it appears in messages and the API.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Screenshot => "screenshot",
            Self::Junit => "junit",
            Self::HtmlReport => "html_report",
            Self::Log => "log",
            Self::Bundle => "bundle",
        }
    }

    fn allowed(self) -> &'static [Kind] {
        match self {
            Self::Screenshot => &[Kind::Png, Kind::Jpeg, Kind::Webp],
            Self::Junit => &[Kind::Xml],
            Self::HtmlReport => &[Kind::Html, Kind::Zip],
            Self::Log => &[Kind::Text],
            Self::Bundle => &[Kind::Zip, Kind::TarZst],
        }
    }
}

/// The file type found from the content (never from a name or header).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    /// PNG image.
    Png,
    /// JPEG image.
    Jpeg,
    /// WebP image.
    Webp,
    /// XML document.
    Xml,
    /// HTML document.
    Html,
    /// Zip archive.
    Zip,
    /// Zstandard-compressed tar archive.
    TarZst,
    /// UTF-8 text.
    Text,
}

impl Kind {
    /// A short name, for example `png` or `tar.zst`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpeg",
            Self::Webp => "webp",
            Self::Xml => "xml",
            Self::Html => "html",
            Self::Zip => "zip",
            Self::TarZst => "tar.zst",
            Self::Text => "text",
        }
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// What the uploader claimed in `upload_request`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Declared {
    /// Size in bytes.
    pub size: u64,
    /// SHA-256 of the content.
    pub sha256: Sha256Digest,
}

/// Hard limits. The defaults suit Maestro output; an operator may tighten them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Largest artifact accepted, whatever its class.
    pub max_bytes: u64,
    /// Most entries in an archive (research 06: 5,000).
    pub max_entries: usize,
    /// Most bytes all of an archive's entries may expand to.
    pub max_uncompressed_bytes: u64,
    /// Largest expansion ratio (uncompressed ÷ compressed) of one zip entry, or of a whole
    /// `tar.zst` stream (research 06: 100).
    pub max_ratio: u64,
    /// Ratios are only enforced beyond this many uncompressed bytes: a few kilobytes of
    /// repeated text legitimately compresses far beyond 100:1, and is bounded by the total.
    pub ratio_floor_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_bytes: 512 * 1024 * 1024,
            max_entries: 5_000,
            max_uncompressed_bytes: 1024 * 1024 * 1024,
            max_ratio: 100,
            ratio_floor_bytes: 1024 * 1024,
        }
    }
}

/// An artifact that passed every check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    /// What the content really is.
    pub kind: Kind,
    /// Size in bytes (equal to the declared size).
    pub size: u64,
    /// SHA-256 (equal to the declared digest).
    pub sha256: Sha256Digest,
    /// For archives, what was found inside.
    pub archive: Option<ArchiveSummary>,
}

/// Why an artifact was refused.
#[derive(Debug, thiserror::Error)]
pub enum VerifyError {
    /// The actual length is not the declared size.
    #[error("size mismatch: declared {declared} bytes, actual {actual}")]
    SizeMismatch {
        /// Declared size.
        declared: u64,
        /// Actual size.
        actual: u64,
    },
    /// Bigger than [`Limits::max_bytes`].
    #[error("artifact of {size} bytes exceeds the limit of {limit}")]
    TooLarge {
        /// Actual size.
        size: u64,
        /// The limit.
        limit: u64,
    },
    /// The content's SHA-256 is not the declared one.
    #[error("digest mismatch: declared {declared}, actual {actual}")]
    DigestMismatch {
        /// Declared digest.
        declared: Sha256Digest,
        /// Actual digest.
        actual: Sha256Digest,
    },
    /// The content is a type this class does not accept.
    #[error("{class} artifacts may not be {}", detected.map_or("an unrecognised file type", Kind::name))]
    TypeNotAllowed {
        /// The class declared.
        class: &'static str,
        /// What the bytes look like, if recognised.
        detected: Option<Kind>,
    },
    /// A log that is not UTF-8 text.
    #[error("log is not valid UTF-8 text without NUL bytes")]
    NotText,
    /// The archive is unsafe or unreadable.
    #[error("unsafe archive: {0}")]
    Archive(#[from] ArchiveError),
    /// Reading the spooled body failed.
    #[error("reading the artifact")]
    Io(#[source] io::Error),
}

/// Bytes read to recognise a type: `tar` needs 262, `infer` never looks further than this.
const HEAD_LEN: usize = 8 * 1024;

/// Checks a spooled artifact. See the [module docs](self).
///
/// Blocking: reads and decompresses the whole body, so call it from `spawn_blocking`.
pub fn verify<R: Read + Seek>(
    class: ArtifactClass,
    declared: Declared,
    mut source: R,
    limits: &Limits,
) -> Result<Verified, VerifyError> {
    let actual = source.seek(SeekFrom::End(0)).map_err(VerifyError::Io)?;
    source.seek(SeekFrom::Start(0)).map_err(VerifyError::Io)?;
    if actual != declared.size {
        return Err(VerifyError::SizeMismatch {
            declared: declared.size,
            actual,
        });
    }
    if actual > limits.max_bytes {
        return Err(VerifyError::TooLarge {
            size: actual,
            limit: limits.max_bytes,
        });
    }

    let (kind, sha256) = scan(class, &mut source)?;
    if sha256 != declared.sha256 {
        return Err(VerifyError::DigestMismatch {
            declared: declared.sha256,
            actual: sha256,
        });
    }

    source.seek(SeekFrom::Start(0)).map_err(VerifyError::Io)?;
    let archive = match kind {
        Kind::Zip => Some(archive::check_zip(source, limits)?),
        Kind::TarZst => Some(archive::check_tar_zst(source, actual, limits)?),
        _ => None,
    };
    Ok(Verified {
        kind,
        size: actual,
        sha256,
        archive,
    })
}

/// One pass over the body: finds the type from the first bytes (refusing early when it is not
/// allowed for the class), validates text, and hashes everything.
fn scan<R: Read>(
    class: ArtifactClass,
    source: &mut R,
) -> Result<(Kind, Sha256Digest), VerifyError> {
    let mut hasher = Sha256::new();
    let mut buf = vec![0_u8; HEAD_LEN];
    let head_len = read_up_to(source, &mut buf).map_err(VerifyError::Io)?;
    let head = buf.get(..head_len).unwrap_or_default();
    let kind = classify(class, head)?;
    let mut text = (kind == Kind::Text).then(Utf8Stream::default);
    let mut feed = |chunk: &[u8]| -> Result<(), VerifyError> {
        if let Some(text) = text.as_mut()
            && !text.push(chunk)
        {
            return Err(VerifyError::NotText);
        }
        hasher.update(chunk);
        Ok(())
    };
    feed(head)?;
    if head_len == HEAD_LEN {
        loop {
            let n = source.read(&mut buf).map_err(VerifyError::Io)?;
            if n == 0 {
                break;
            }
            feed(buf.get(..n).unwrap_or_default())?;
        }
    }
    if text.is_some_and(|t| !t.is_complete()) {
        return Err(VerifyError::NotText);
    }
    Ok((kind, Sha256Digest::from_bytes(hasher.finalize().into())))
}

/// Like `read_exact` but a short body is not an error: returns the bytes read.
fn read_up_to<R: Read>(source: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while let Some(rest) = buf.get_mut(filled..).filter(|r| !r.is_empty()) {
        match source.read(rest) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

/// Recognises the content type from its first bytes and checks the class allows it.
fn classify(class: ArtifactClass, head: &[u8]) -> Result<Kind, VerifyError> {
    let detected = detect(head);
    // Logs have no magic: text, or text that happens to start like XML or HTML.
    let kind = match (class, detected) {
        (ArtifactClass::Log, None | Some(Kind::Xml | Kind::Html)) => Some(Kind::Text),
        (_, other) => other,
    };
    match kind {
        Some(k) if class.allowed().contains(&k) => Ok(k),
        _ => Err(VerifyError::TypeNotAllowed {
            class: class.name(),
            detected,
        }),
    }
}

/// Whether the head names an `<svg` element, case-insensitively.
fn mentions_svg(head: &[u8]) -> bool {
    head.windows(4).any(|w| w.eq_ignore_ascii_case(b"<svg"))
}

fn detect(head: &[u8]) -> Option<Kind> {
    use infer::{archive, image, text};
    if image::is_png(head) {
        Some(Kind::Png)
    } else if image::is_jpeg(head) {
        Some(Kind::Jpeg)
    } else if head.starts_with(b"RIFF") && image::is_webp(head) {
        // `infer` alone checks only the "WEBP" tag at offset 8.
        Some(Kind::Webp)
    } else if archive::is_zip(head) {
        Some(Kind::Zip)
    } else if archive::is_zst(head) {
        Some(Kind::TarZst)
    } else if text::is_xml(head) && !mentions_svg(head) {
        // An SVG with an XML prolog is XML to `infer`; it is never an accepted type (no SVG).
        Some(Kind::Xml)
    } else if text::is_html(head) {
        Some(Kind::Html)
    } else {
        None
    }
}

/// Validates UTF-8 across chunk boundaries and refuses NUL bytes.
#[derive(Default)]
struct Utf8Stream {
    /// An incomplete character carried over from the previous chunk (at most 3 bytes).
    pending: Vec<u8>,
}

impl Utf8Stream {
    fn push(&mut self, chunk: &[u8]) -> bool {
        if chunk.contains(&0) {
            return false;
        }
        self.pending.extend_from_slice(chunk);
        match std::str::from_utf8(&self.pending) {
            Ok(_) => {
                self.pending.clear();
                true
            }
            // Cut mid-character: keep the tail for the next chunk.
            Err(e) if e.error_len().is_none() => {
                self.pending.drain(..e.valid_up_to());
                true
            }
            Err(_) => false,
        }
    }

    fn is_complete(&self) -> bool {
        self.pending.is_empty()
    }
}

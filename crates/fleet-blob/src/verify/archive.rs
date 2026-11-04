// SPDX-License-Identifier: AGPL-3.0-only
//! Archive safety for zip and `tar.zst` (research 06 threat T11).
//!
//! Every entry is *read*, never extracted: its bytes go through a counter and a 512-byte sniff
//! for nested archives, then are dropped. Header sizes are ignored; only bytes actually produced
//! by the decompressor count, and reading stops at the first limit hit.

use std::{
    cell::Cell,
    io::{self, BufReader, Read, Seek},
    rc::Rc,
};

use crate::verify::Limits;

/// What a safe archive holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArchiveSummary {
    /// Entries, directories included.
    pub entries: usize,
    /// Bytes the entries really expand to (counted while decompressing, not read from headers).
    pub uncompressed_bytes: u64,
}

/// Why an archive was refused.
#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    /// More entries than [`Limits::max_entries`].
    #[error("archive has more than {limit} entries")]
    TooManyEntries {
        /// The limit.
        limit: usize,
    },
    /// The entries expand to more than [`Limits::max_uncompressed_bytes`].
    #[error("archive expands to more than {limit} bytes")]
    TooLarge {
        /// The limit.
        limit: u64,
    },
    /// An entry (or a whole `tar.zst`) expands more than [`Limits::max_ratio`] to 1.
    #[error("{name} expands by more than {limit}:1")]
    RatioExceeded {
        /// The entry's name (escaped).
        name: String,
        /// The limit.
        limit: u64,
    },
    /// An entry path is absolute, climbs with `..`, uses backslashes, a drive letter, control
    /// or direction-override characters, or is empty or too long.
    #[error("unsafe entry path {name:?}")]
    UnsafePath {
        /// The entry's name (escaped).
        name: String,
    },
    /// An entry is a symlink, hardlink, device, fifo or socket.
    #[error("entry {name:?} is a {what}")]
    SpecialFile {
        /// The entry's name (escaped).
        name: String,
        /// What it is.
        what: &'static str,
    },
    /// An entry is itself an archive. Nested archives are never expanded (depth 0).
    #[error("entry {name:?} is a nested archive")]
    NestedArchive {
        /// The entry's name (escaped).
        name: String,
    },
    /// Bytes follow the end of the `tar.zst` stream (or a second zstd frame), which another
    /// tool might read differently.
    #[error("data after the end of the archive")]
    TrailingData,
    /// Not a valid archive: truncated, corrupt, or using something unsupported (encryption,
    /// compression other than stored and deflate).
    #[error("malformed archive: {0}")]
    Malformed(String),
    /// Reading the spooled body failed.
    #[error("reading the archive")]
    Io(#[source] io::Error),
}

/// Longest entry path accepted, in bytes.
const MAX_PATH_BYTES: usize = 1024;
/// Bytes of each entry inspected for a nested archive signature (`tar` needs 262).
const SNIFF_BYTES: usize = 512;
/// Widest zstd window the decoder will allocate (the reference decoder allows 128 MiB).
const MAX_ZSTD_WINDOW: u64 = 64 * 1024 * 1024;
/// Tar headers and padding add up to a little over 512 bytes per entry beyond entry content.
const TAR_OVERHEAD_PER_ENTRY: u64 = 1024;

/// A name that is safe to show in a message or log: escaped, truncated.
fn shown(name: &str) -> String {
    let escaped: String = name
        .chars()
        .take(100)
        .flat_map(char::escape_debug)
        .collect();
    if name.chars().count() > 100 {
        format!("{escaped}…")
    } else {
        escaped
    }
}

/// Is this entry path one a consumer may safely join onto a directory? Not a filesystem check:
/// a pure rule on the text.
fn is_safe_path(name: &str) -> bool {
    if name.is_empty() || name.len() > MAX_PATH_BYTES || name.starts_with('/') {
        return false;
    }
    let bad_char = |c: char| {
        c.is_control()
            || c == '\\'
            // Bidirectional overrides and isolates: names that read as another name.
            || matches!(c, '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
    };
    if name.chars().any(bad_char) {
        return false;
    }
    let mut chars = name.chars();
    if matches!((chars.next(), chars.next()), (Some(d), Some(':')) if d.is_ascii_alphabetic()) {
        return false;
    }
    name.split('/').all(|part| part != "..")
}

fn check_name(name: &str) -> Result<(), ArchiveError> {
    if is_safe_path(name) {
        Ok(())
    } else {
        Err(ArchiveError::UnsafePath { name: shown(name) })
    }
}

/// Rejects Unix file types other than regular files and directories (`0` means "not recorded").
fn check_unix_mode(name: &str, mode: Option<u32>) -> Result<(), ArchiveError> {
    let what = match mode.map(|m| m & 0o170_000) {
        None | Some(0 | 0o100_000 | 0o040_000) => return Ok(()),
        Some(0o120_000) => "symlink",
        Some(0o020_000) => "character device",
        Some(0o060_000) => "block device",
        Some(0o010_000) => "fifo",
        Some(0o140_000) => "socket",
        Some(_) => "special file",
    };
    Err(ArchiveError::SpecialFile {
        name: shown(name),
        what,
    })
}

/// Reads an entry to its end, counting. `ratio_exceeded(bytes so far)` is asked after each read.
/// Returns the entry's real size.
fn drain(
    mut entry: impl Read,
    name: &str,
    budget: u64,
    limits: &Limits,
    ratio_exceeded: impl Fn(u64) -> bool,
) -> Result<u64, ArchiveError> {
    let mut total = 0_u64;
    let mut sniff = [0_u8; SNIFF_BYTES];
    let mut sniffed = 0;
    let mut buf = vec![0_u8; 32 * 1024];
    loop {
        let n = match entry.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(read_error(e)),
        };
        total += n as u64;
        if total > budget {
            return Err(ArchiveError::TooLarge {
                limit: limits.max_uncompressed_bytes,
            });
        }
        if ratio_exceeded(total) {
            return Err(ArchiveError::RatioExceeded {
                name: shown(name),
                limit: limits.max_ratio,
            });
        }
        if let (Some(room), Some(chunk)) = (sniff.get_mut(sniffed..), buf.get(..n)) {
            let take = room.len().min(chunk.len());
            if let (Some(dst), Some(src)) = (room.get_mut(..take), chunk.get(..take)) {
                dst.copy_from_slice(src);
            }
            sniffed += take;
        }
    }
    if infer::is_archive(sniff.get(..sniffed).unwrap_or_default()) {
        return Err(ArchiveError::NestedArchive { name: shown(name) });
    }
    Ok(total)
}

/// Decompressor and parser errors mean a bad archive; anything else is the disk failing.
fn read_error(e: io::Error) -> ArchiveError {
    match e.kind() {
        io::ErrorKind::InvalidData | io::ErrorKind::UnexpectedEof | io::ErrorKind::Other => {
            ArchiveError::Malformed(e.to_string())
        }
        _ => ArchiveError::Io(e),
    }
}

fn zip_error(e: zip::result::ZipError) -> ArchiveError {
    match e {
        zip::result::ZipError::Io(e) => read_error(e),
        other => ArchiveError::Malformed(other.to_string()),
    }
}

pub(super) fn check_zip<R: Read + Seek>(
    source: R,
    limits: &Limits,
) -> Result<ArchiveSummary, ArchiveError> {
    let mut zip = zip::ZipArchive::new(source).map_err(zip_error)?;
    if zip.len() > limits.max_entries {
        return Err(ArchiveError::TooManyEntries {
            limit: limits.max_entries,
        });
    }
    let mut budget = limits.max_uncompressed_bytes;
    for index in 0..zip.len() {
        // Judge the entry by its name and type before decompressing a byte of it.
        let (name, compressed, declared, is_dir) = {
            let raw = zip.by_index_raw(index).map_err(zip_error)?;
            let name = match raw.name() {
                Ok(name) => name.into_owned(),
                // Not decodable as text: nothing downstream could name it reliably.
                Err(_) => {
                    return Err(ArchiveError::UnsafePath {
                        name: shown(&String::from_utf8_lossy(raw.name_raw())),
                    });
                }
            };
            check_name(&name)?;
            check_unix_mode(&name, raw.unix_mode())?;
            if raw.is_symlink() {
                return Err(ArchiveError::SpecialFile {
                    name: shown(&name),
                    what: "symlink",
                });
            }
            (name, raw.compressed_size(), raw.size(), raw.is_dir())
        };
        if is_dir {
            continue;
        }
        let entry = zip.by_index(index).map_err(zip_error)?;
        let used = drain(entry, &name, budget, limits, |total| {
            total > limits.ratio_floor_bytes && total / compressed.max(1) > limits.max_ratio
        })?;
        if used != declared {
            // A header that disagrees with the data is a lie to whoever trusts it later.
            return Err(ArchiveError::Malformed(format!(
                "entry {} is {used} bytes but its header says {declared}",
                shown(&name)
            )));
        }
        budget = budget.saturating_sub(used);
    }
    Ok(ArchiveSummary {
        entries: zip.len(),
        uncompressed_bytes: limits.max_uncompressed_bytes - budget,
    })
}

/// Counts the compressed bytes the decoder takes from the body.
struct Counted<R> {
    inner: R,
    count: Rc<Cell<u64>>,
}

impl<R: Read> Read for Counted<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.count.set(self.count.get().saturating_add(n as u64));
        Ok(n)
    }
}

/// Which stream-wide limit stopped the decoder.
#[derive(Clone, Copy)]
enum Hit {
    TooLarge,
    Ratio,
}

/// Sits after the decoder: stops the stream as soon as it expands past the total limit or the
/// ratio, so a bomb is never decompressed beyond what a limit allows.
struct Guarded<R> {
    inner: R,
    produced: u64,
    consumed: Rc<Cell<u64>>,
    cap: u64,
    limits: Limits,
    hit: Rc<Cell<Option<Hit>>>,
}

impl<R: Read> Read for Guarded<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.produced = self.produced.saturating_add(n as u64);
        let hit = if self.produced > self.cap {
            Some(Hit::TooLarge)
        } else if self.produced > self.limits.ratio_floor_bytes
            && self.produced / self.consumed.get().max(1) > self.limits.max_ratio
        {
            Some(Hit::Ratio)
        } else {
            None
        };
        if hit.is_some() {
            self.hit.set(hit);
            return Err(io::Error::other("stream limit reached"));
        }
        Ok(n)
    }
}

fn tar_kind(kind: tar::EntryType) -> Result<(), &'static str> {
    use tar::EntryType as T;
    match kind {
        t if t.is_file() || t.is_dir() || t.is_pax_global_extensions() => Ok(()),
        T::Symlink => Err("symlink"),
        T::Link => Err("hardlink"),
        T::Char => Err("character device"),
        T::Block => Err("block device"),
        T::Fifo => Err("fifo"),
        T::GNUSparse => Err("sparse file"),
        _ => Err("special entry"),
    }
}

pub(super) fn check_tar_zst<R: Read>(
    source: R,
    compressed_len: u64,
    limits: &Limits,
) -> Result<ArchiveSummary, ArchiveError> {
    let consumed = Rc::new(Cell::new(0_u64));
    let hit = Rc::new(Cell::new(None));
    let compressed = Counted {
        inner: BufReader::with_capacity(64 * 1024, source),
        count: Rc::clone(&consumed),
    };
    let decoder =
        ruzstd::decoding::StreamingDecoder::new_with_max_window_size(compressed, MAX_ZSTD_WINDOW)
            .map_err(|e| ArchiveError::Malformed(format!("zstd: {e}")))?;
    let guarded = Guarded {
        inner: decoder,
        produced: 0,
        consumed: Rc::clone(&consumed),
        cap: limits
            .max_uncompressed_bytes
            .saturating_add(TAR_OVERHEAD_PER_ENTRY.saturating_mul(limits.max_entries as u64 + 2)),
        limits: *limits,
        hit: Rc::clone(&hit),
    };

    // A stream limit explains an I/O error better than the error itself does.
    let explain = |e: io::Error| match hit.get() {
        Some(Hit::TooLarge) => ArchiveError::TooLarge {
            limit: limits.max_uncompressed_bytes,
        },
        Some(Hit::Ratio) => ArchiveError::RatioExceeded {
            name: "the whole archive".into(),
            limit: limits.max_ratio,
        },
        None => read_error(e),
    };

    let mut archive = tar::Archive::new(guarded);
    let mut entries = 0_usize;
    let mut budget = limits.max_uncompressed_bytes;
    for entry in archive.entries().map_err(explain)? {
        let mut entry = entry.map_err(explain)?;
        entries += 1;
        if entries > limits.max_entries {
            return Err(ArchiveError::TooManyEntries {
                limit: limits.max_entries,
            });
        }
        let path = entry.path_bytes();
        let name = std::str::from_utf8(&path).map_err(|_| ArchiveError::UnsafePath {
            name: shown(&String::from_utf8_lossy(&path)),
        })?;
        check_name(name)?;
        let name = name.to_owned();
        let kind = entry.header().entry_type();
        if let Err(what) = tar_kind(kind) {
            return Err(ArchiveError::SpecialFile {
                name: shown(&name),
                what,
            });
        }
        if !kind.is_file() {
            continue;
        }
        let used = match drain(&mut entry, &name, budget, limits, |_| false) {
            Err(ArchiveError::Malformed(_) | ArchiveError::Io(_)) if hit.get().is_some() => {
                return Err(explain(io::Error::other("stream limit reached")));
            }
            other => other?,
        };
        budget = budget.saturating_sub(used);
    }

    // Read on to the end of the zstd frame, so nothing after the tar's own end marker is
    // skipped unseen, then insist that the body ends there.
    io::copy(&mut archive.into_inner(), &mut io::sink()).map_err(explain)?;
    if consumed.get() != compressed_len {
        return Err(ArchiveError::TrailingData);
    }
    Ok(ArchiveSummary {
        entries,
        uncompressed_bytes: limits.max_uncompressed_bytes - budget,
    })
}

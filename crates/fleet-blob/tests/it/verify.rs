// SPDX-License-Identifier: AGPL-3.0-only
//! Artifact verification over bytes (S7): size, digest, magic bytes per class, archive safety.
//!
//! Inputs are real Maestro output from `fixtures/maestro` where Maestro writes the file type, and
//! crafted bytes otherwise (archives, hostile inputs: those are not Maestro output).

use std::io::Cursor;

use fleet_blob::{
    Sha256Digest,
    verify::{ArtifactClass, Declared, Kind, Limits, Verified, VerifyError, verify},
};

use ArtifactClass::{Bundle, HtmlReport, Junit, Log, Screenshot};

/// Real Maestro output (fixtures are captured, never invented).
const JUNIT: &[u8] =
    include_bytes!("../../../../fixtures/maestro/2.12.1/pass-machine-log/run/report.xml");
const MAESTRO_LOG: &[u8] =
    include_bytes!("../../../../fixtures/maestro/2.12.1/pass-machine-log/run/debug/maestro.log");
const SCREENSHOT: &[u8] = include_bytes!(
    "../../../../fixtures/maestro/2.12.1/fail/run/debug/fail/screenshots/step-005-assertCondition-Fleet_never_shows_this.png"
);

/// An honest uploader: declares exactly what it sends.
pub(crate) fn check(
    class: ArtifactClass,
    bytes: &[u8],
    limits: &Limits,
) -> Result<Verified, VerifyError> {
    let declared = Declared {
        size: bytes.len() as u64,
        sha256: Sha256Digest::of(bytes),
    };
    verify(class, declared, Cursor::new(bytes), limits)
}

fn accepts(class: ArtifactClass, bytes: &[u8]) -> Verified {
    check(class, bytes, &Limits::default())
        .unwrap_or_else(|e| panic!("{} should be accepted: {e}", class.name()))
}

#[test]
fn real_maestro_output_is_accepted_for_its_class() {
    assert_eq!(accepts(Junit, JUNIT).kind, Kind::Xml);
    assert_eq!(accepts(Log, MAESTRO_LOG).kind, Kind::Text);
    assert_eq!(accepts(Screenshot, SCREENSHOT).kind, Kind::Png);
    let verified = accepts(Screenshot, SCREENSHOT);
    assert_eq!(verified.size, SCREENSHOT.len() as u64);
    assert_eq!(verified.sha256, Sha256Digest::of(SCREENSHOT));
    assert_eq!(verified.archive, None);
}

#[test]
fn magic_bytes_must_match_the_class() {
    let jpeg = [&[0xFF, 0xD8, 0xFF, 0xE0][..], &[0; 32]].concat();
    let webp = [&b"RIFF"[..], &[0x24, 0, 0, 0], b"WEBPVP8 ", &[0; 16]].concat();
    let fake_webp = [&b"NOPE"[..], &[0x24, 0, 0, 0], b"WEBPVP8 ", &[0; 16]].concat();
    let html = b"<!DOCTYPE html>\n<html><body>report</body></html>";
    let gzip = [0x1F, 0x8B, 0x08, 0, 0, 0, 0, 0, 0, 3, 0, 0];

    // (class, bytes, what it is): the table of what each class takes.
    let accepted: &[(ArtifactClass, &[u8], Kind)] = &[
        (Screenshot, &jpeg, Kind::Jpeg),
        (Screenshot, &webp, Kind::Webp),
        (HtmlReport, html, Kind::Html),
        (Log, b"plain text\nwith lines\n", Kind::Text),
        (Log, b"", Kind::Text),
    ];
    for (class, bytes, kind) in accepted {
        assert_eq!(accepts(*class, bytes).kind, *kind, "{}", class.name());
    }

    let refused: &[(ArtifactClass, &[u8], Option<Kind>)] = &[
        (Screenshot, JUNIT, Some(Kind::Xml)),
        (Screenshot, html, Some(Kind::Html)),
        (Screenshot, &fake_webp, None),
        (Junit, SCREENSHOT, Some(Kind::Png)),
        (Junit, html, Some(Kind::Html)),
        (Junit, b"just text", None),
        // An SVG with an XML prolog is XML to the sniffer: it must still be refused (no SVG).
        (Junit, b"<?xml version=\"1.0\"?>\n<svg xmlns=\"http://www.w3.org/2000/svg\"><script>alert(1)</script></svg>", None),
        (HtmlReport, SCREENSHOT, Some(Kind::Png)),
        (HtmlReport, b"plain text, not a page", None),
        (Bundle, SCREENSHOT, Some(Kind::Png)),
        (Bundle, html, Some(Kind::Html)),
        (Bundle, &gzip, None),
        (Log, SCREENSHOT, Some(Kind::Png)),
        (Screenshot, b"", None),
    ];
    for (class, bytes, detected) in refused {
        match check(*class, bytes, &Limits::default()) {
            Err(VerifyError::TypeNotAllowed { detected: d, .. }) => {
                assert_eq!(d, *detected, "{} given {detected:?}", class.name());
            }
            other => panic!("{} should refuse {detected:?}: {other:?}", class.name()),
        }
    }
}

#[test]
fn a_liar_about_size_or_digest_is_refused() {
    let honest = Declared {
        size: SCREENSHOT.len() as u64,
        sha256: Sha256Digest::of(SCREENSHOT),
    };
    let run = |declared| {
        verify(
            Screenshot,
            declared,
            Cursor::new(SCREENSHOT),
            &Limits::default(),
        )
    };

    let short = Declared {
        size: honest.size - 1,
        ..honest
    };
    assert!(
        matches!(run(short), Err(VerifyError::SizeMismatch { actual, .. }) if actual == honest.size)
    );
    let long = Declared {
        size: honest.size + 1,
        ..honest
    };
    assert!(matches!(run(long), Err(VerifyError::SizeMismatch { .. })));
    let forged = Declared {
        sha256: Sha256Digest::of(b"something else"),
        ..honest
    };
    assert!(
        matches!(run(forged), Err(VerifyError::DigestMismatch { actual, .. }) if actual == honest.sha256)
    );
}

#[test]
fn the_size_limit_applies_to_every_class() {
    let tight = Limits {
        max_bytes: 100,
        ..Limits::default()
    };
    assert!(matches!(
        check(Screenshot, SCREENSHOT, &tight),
        Err(VerifyError::TooLarge { limit: 100, .. })
    ));
    assert!(check(Log, &[b'x'; 100], &tight).is_ok());
    assert!(check(Log, &[b'x'; 101], &tight).is_err());
}

#[test]
fn logs_must_be_utf8_text_without_nul() {
    // 'é' is two bytes; put the pair across the 8 KiB sniff boundary and the 64 KiB reads.
    for boundary in [8 * 1024, 8 * 1024 + 1, 8 * 1024 + 64 * 1024] {
        let mut log = vec![b'a'; boundary - 1];
        log.extend_from_slice("é and 日本語 and 🙂\n".as_bytes());
        accepts(Log, &log);
    }
    let cases: &[(&str, Vec<u8>)] = &[
        ("invalid byte", b"ok so far \xFF\xFE not ok".to_vec()),
        ("lone continuation byte", b"\x80".to_vec()),
        (
            "cut mid-character at the end",
            [b"fine ", &"é".as_bytes()[..1]].concat(),
        ),
        ("NUL byte", b"text\0text".to_vec()),
        (
            "NUL far into the file",
            [vec![b'a'; 100_000], vec![0], vec![b'b'; 10]].concat(),
        ),
        (
            "invalid byte far into the file",
            [vec![b'a'; 100_000], vec![0xC0, 0x80]].concat(),
        ),
    ];
    for (name, bytes) in cases {
        let result = check(Log, bytes, &Limits::default());
        assert!(
            matches!(result, Err(VerifyError::NotText)),
            "{name}: {result:?}"
        );
    }
}

// ---- zip archives ----------------------------------------------------------------------------

use std::io::Write;

use fleet_blob::verify::ArchiveError;
use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

fn opts(method: CompressionMethod) -> SimpleFileOptions {
    SimpleFileOptions::default().compression_method(method)
}

/// A zip with these entries (a name ending in `/` is a directory).
pub(crate) fn zip_of(method: CompressionMethod, entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    for (name, data) in entries {
        if name.ends_with('/') {
            zip.add_directory(*name, opts(method)).unwrap();
        } else {
            zip.start_file(*name, opts(method)).unwrap();
            zip.write_all(data).unwrap();
        }
    }
    zip.finish().unwrap().into_inner()
}

/// Rewrites the central-directory header of the entry called `name` (a crafted lie the writer
/// would never produce).
fn patch_central(zip: &mut [u8], name: &str, patch: impl FnOnce(&mut [u8])) {
    const SIG: &[u8] = b"PK\x01\x02";
    let at = zip
        .windows(SIG.len())
        .enumerate()
        .filter(|(_, w)| *w == SIG)
        .map(|(i, _)| i)
        .find(|i| zip[i + 46..].starts_with(name.as_bytes()))
        .expect("central directory entry");
    patch(&mut zip[at..at + 46]);
}

const DEFLATED: CompressionMethod = CompressionMethod::Deflated;
const STORED: CompressionMethod = CompressionMethod::Stored;

fn archive_error(class: ArtifactClass, bytes: &[u8], limits: &Limits) -> ArchiveError {
    match check(class, bytes, limits) {
        Err(VerifyError::Archive(e)) => e,
        other => panic!("expected an archive error, got {other:?}"),
    }
}

#[test]
fn a_well_formed_report_zip_is_accepted_for_reports_and_bundles() {
    let zip = zip_of(
        DEFLATED,
        &[
            ("report/", b""),
            ("report/index.html", b"<!DOCTYPE html><html></html>"),
            ("report/shots/step-1.png", SCREENSHOT),
            ("junit.xml", JUNIT),
        ],
    );
    for class in [HtmlReport, Bundle] {
        let verified = accepts(class, &zip);
        let summary = verified.archive.expect("archive summary");
        assert_eq!(verified.kind, Kind::Zip);
        assert_eq!(summary.entries, 4);
        assert_eq!(
            summary.uncompressed_bytes,
            (28 + SCREENSHOT.len() + JUNIT.len()) as u64
        );
    }
    // And an empty archive is still an archive.
    assert_eq!(
        accepts(Bundle, &zip_of(DEFLATED, &[]))
            .archive
            .unwrap()
            .entries,
        0
    );
}

#[test]
fn unsafe_entry_names_are_refused() {
    let names = [
        "../evil.txt",
        "report/../../evil.txt",
        "a/b/../../../evil",
        "/etc/passwd",
        "..\\evil.txt",
        "report\\..\\evil.txt",
        "C:\\Windows\\evil.dll",
        "c:/evil.dll",
        "with\0nul",
        "line\nbreak",
        "\u{202e}gnp.exe",
        "",
    ];
    for name in names {
        let zip = zip_of(STORED, &[("ok.txt", b"fine"), (name, b"payload")]);
        let result = check(Bundle, &zip, &Limits::default());
        assert!(
            matches!(
                result,
                Err(VerifyError::Archive(ArchiveError::UnsafePath { .. }))
            ),
            "{name:?}: {result:?}"
        );
    }
    // Dots and odd-but-safe characters are fine.
    let fine = zip_of(
        STORED,
        &[
            ("a..b/c..d.txt", b"x"),
            ("...", b"x"),
            ("dir/.hidden", b"x"),
            ("sp ace/é.txt", b"x"),
        ],
    );
    accepts(Bundle, &fine);
}

#[test]
fn links_and_device_files_are_refused() {
    let mut symlink = ZipWriter::new(Cursor::new(Vec::new()));
    symlink.start_file("ok.txt", opts(STORED)).unwrap();
    symlink.write_all(b"fine").unwrap();
    symlink
        .add_symlink("link", "/etc/passwd", opts(STORED))
        .unwrap();
    let symlink = symlink.finish().unwrap().into_inner();
    assert!(
        matches!(
            archive_error(Bundle, &symlink, &Limits::default()),
            ArchiveError::SpecialFile { .. }
        ),
        "symlink"
    );

    // Unix file types the writer cannot produce: set them in the external attributes.
    for (what, mode) in [
        ("character device", 0o020_666_u32),
        ("block device", 0o060_660),
        ("fifo", 0o010_644),
        ("socket", 0o140_755),
    ] {
        let mut zip = zip_of(STORED, &[("dev", b"")]);
        patch_central(&mut zip, "dev", |h| {
            h[5] = 3; // made by: Unix
            h[38..42].copy_from_slice(&(mode << 16).to_le_bytes());
        });
        let err = archive_error(Bundle, &zip, &Limits::default());
        assert!(
            matches!(err, ArchiveError::SpecialFile { .. }),
            "{what}: {err:?}"
        );
    }
}

#[test]
fn too_many_entries_are_refused() {
    let names: Vec<String> = (0..6).map(|i| format!("f{i}.txt")).collect();
    let entries: Vec<(&str, &[u8])> = names.iter().map(|n| (n.as_str(), &b"x"[..])).collect();
    let zip = zip_of(STORED, &entries);
    let limits = Limits {
        max_entries: 5,
        ..Limits::default()
    };
    assert!(matches!(
        archive_error(Bundle, &zip, &limits),
        ArchiveError::TooManyEntries { limit: 5 }
    ));
    assert!(
        check(
            Bundle,
            &zip,
            &Limits {
                max_entries: 6,
                ..Limits::default()
            }
        )
        .is_ok()
    );
}

#[test]
fn the_total_uncompressed_size_is_capped() {
    let mib = vec![7_u8; 1024 * 1024];
    let zip = zip_of(STORED, &[("a", &mib), ("b", &mib), ("c", &mib)]);
    let limits = Limits {
        max_uncompressed_bytes: 2 * 1024 * 1024,
        ..Limits::default()
    };
    assert!(matches!(
        archive_error(Bundle, &zip, &limits),
        ArchiveError::TooLarge { .. }
    ));
    let roomy = Limits {
        max_uncompressed_bytes: 3 * 1024 * 1024,
        ..Limits::default()
    };
    assert_eq!(
        check(Bundle, &zip, &roomy)
            .unwrap()
            .archive
            .unwrap()
            .uncompressed_bytes,
        3 << 20
    );
}

#[test]
fn a_single_entry_zip_bomb_is_stopped_by_the_ratio() {
    // 64 MiB of zeros deflates to about 64 KiB: a ratio near 1000:1.
    let zeros = vec![0_u8; 64 * 1024 * 1024];
    let bomb = zip_of(DEFLATED, &[("bomb.bin", &zeros)]);
    assert!(
        bomb.len() < 200 * 1024,
        "the fixture really is a bomb: {} bytes",
        bomb.len()
    );
    let err = archive_error(Bundle, &bomb, &Limits::default());
    assert!(
        matches!(&err, ArchiveError::RatioExceeded { name, .. } if name == "bomb.bin"),
        "{err:?}"
    );

    // Under the floor, repetition is harmless and allowed (a repetitive log is fine).
    let small = zip_of(DEFLATED, &[("log.txt", &vec![b'a'; 512 * 1024])]);
    accepts(Bundle, &small);
}

#[test]
fn many_small_entries_cannot_add_up_to_a_bomb() {
    // The classic shape: each entry is under the ratio floor, the sum is not.
    let one_mib = vec![0_u8; 1024 * 1024];
    let names: Vec<String> = (0..80).map(|i| format!("part-{i}.bin")).collect();
    let entries: Vec<(&str, &[u8])> = names.iter().map(|n| (n.as_str(), &one_mib[..])).collect();
    let bomb = zip_of(DEFLATED, &entries);
    let limits = Limits {
        max_uncompressed_bytes: 64 * 1024 * 1024,
        ..Limits::default()
    };
    assert!(matches!(
        archive_error(Bundle, &bomb, &limits),
        ArchiveError::TooLarge { .. }
    ));
}

#[test]
fn headers_that_lie_about_sizes_do_not_bypass_the_limits() {
    let zeros = vec![0_u8; 32 * 1024 * 1024];
    let mut bomb = zip_of(DEFLATED, &[("bomb.bin", &zeros)]);
    // The directory now claims 10 bytes. The real data is still 32 MiB.
    patch_central(&mut bomb, "bomb.bin", |h| {
        h[24..28].copy_from_slice(&10_u32.to_le_bytes());
    });
    let limits = Limits {
        max_uncompressed_bytes: 1024 * 1024,
        ..Limits::default()
    };
    let result = check(Bundle, &bomb, &limits);
    assert!(matches!(result, Err(VerifyError::Archive(_))), "{result:?}");

    // Claiming a huge size for a tiny entry is refused too, not trusted.
    let mut tiny = zip_of(STORED, &[("t.txt", b"hi")]);
    patch_central(&mut tiny, "t.txt", |h| {
        h[24..28].copy_from_slice(&u32::MAX.to_le_bytes());
    });
    assert!(matches!(
        check(Bundle, &tiny, &Limits::default()),
        Err(VerifyError::Archive(_))
    ));
}

#[test]
fn nested_archives_are_refused_not_expanded() {
    let zip_inside = zip_of(STORED, &[("inner.txt", b"x")]);
    let inner_gzip = [
        0x1F, 0x8B, 0x08, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    ];
    let nested = [
        ("inner.zip", &zip_inside[..]),
        ("inner.tgz", &inner_gzip[..]),
    ];
    for (name, bytes) in nested {
        let zip = zip_of(DEFLATED, &[("ok.txt", b"fine"), (name, bytes)]);
        let err = archive_error(Bundle, &zip, &Limits::default());
        assert!(
            matches!(&err, ArchiveError::NestedArchive { name: n } if n == name),
            "{name}: {err:?}"
        );
    }
}

#[test]
fn garbage_that_starts_like_a_zip_is_malformed() {
    let valid = zip_of(DEFLATED, &[("a.txt", b"hello hello hello hello")]);
    let truncated = &valid[..valid.len() - 10];
    let garbage = [&b"PK\x03\x04"[..], &[0xAB; 200]].concat();
    for (name, bytes) in [("truncated", truncated), ("garbage", &garbage[..])] {
        let result = check(Bundle, bytes, &Limits::default());
        assert!(
            matches!(
                result,
                Err(VerifyError::Archive(ArchiveError::Malformed(_)))
            ),
            "{name}: {result:?}"
        );
    }
}

// ---- tar.zst bundles -------------------------------------------------------------------------

use ruzstd::encoding::{CompressionLevel, compress_to_vec};
use tar::{Builder, EntryType, Header};

fn zstd(bytes: &[u8]) -> Vec<u8> {
    compress_to_vec(bytes, CompressionLevel::Fastest)
}

/// A header with a raw name, bypassing the builder's own path checks (it refuses `..`).
fn raw_header(name: &[u8], kind: EntryType, size: u64) -> Header {
    let mut header = Header::new_gnu();
    header.as_old_mut().name[..name.len()].copy_from_slice(name);
    header.set_entry_type(kind);
    header.set_size(size);
    header.set_mode(0o644);
    header.set_cksum();
    header
}

/// A tar of `(raw name, entry type, content)`; a link's target is the content.
fn tar_of(entries: &[(&[u8], EntryType, &[u8])]) -> Vec<u8> {
    let mut builder = Builder::new(Vec::new());
    for (name, kind, data) in entries {
        let is_link = matches!(kind, EntryType::Symlink | EntryType::Link);
        let size = if is_link { 0 } else { data.len() as u64 };
        let mut header = raw_header(name, *kind, size);
        if is_link {
            header
                .set_link_name(std::str::from_utf8(data).unwrap())
                .unwrap();
            header.set_cksum();
        }
        builder
            .append(&header, if is_link { &b""[..] } else { data })
            .unwrap();
    }
    builder.into_inner().unwrap()
}

fn files(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let entries: Vec<_> = entries
        .iter()
        .map(|(n, d)| (n.as_bytes(), EntryType::Regular, *d))
        .collect();
    tar_of(&entries)
}

#[test]
fn a_well_formed_tar_zst_bundle_is_accepted() {
    let bundle = zstd(&tar_of(&[
        (b"bundle/", EntryType::Directory, b""),
        (b"bundle/junit.xml", EntryType::Regular, JUNIT),
        (b"bundle/maestro.log", EntryType::Regular, MAESTRO_LOG),
        (b"bundle/step-1.png", EntryType::Regular, SCREENSHOT),
    ]));
    let verified = accepts(Bundle, &bundle);
    let summary = verified.archive.expect("archive summary");
    assert_eq!(verified.kind, Kind::TarZst);
    assert_eq!(summary.entries, 4);
    assert_eq!(
        summary.uncompressed_bytes,
        (JUNIT.len() + MAESTRO_LOG.len() + SCREENSHOT.len()) as u64
    );
    // Only bundles are tar.zst: it is not a report or an image.
    assert!(matches!(
        check(HtmlReport, &bundle, &Limits::default()),
        Err(VerifyError::TypeNotAllowed { .. })
    ));
}

#[test]
fn a_plain_tar_is_not_a_bundle() {
    let plain = files(&[("a.txt", b"x")]);
    assert!(matches!(
        check(Bundle, &plain, &Limits::default()),
        Err(VerifyError::TypeNotAllowed { detected: None, .. })
    ));
}

#[test]
fn tar_entries_with_unsafe_paths_are_refused() {
    for name in [
        "../evil",
        "/etc/passwd",
        "a/b/../../../evil",
        "..\\evil",
        "C:\\evil.dll",
        "line\nbreak",
    ] {
        let bundle = zstd(&files(&[("ok.txt", b"fine"), (name, b"payload")]));
        let result = check(Bundle, &bundle, &Limits::default());
        assert!(
            matches!(
                result,
                Err(VerifyError::Archive(ArchiveError::UnsafePath { .. }))
            ),
            "{name:?}: {result:?}"
        );
    }
}

#[test]
fn tar_links_and_devices_are_refused() {
    let kinds: [(&str, EntryType, &[u8]); 5] = [
        ("symlink", EntryType::Symlink, b"/etc/passwd"),
        ("hardlink", EntryType::Link, b"ok.txt"),
        ("character device", EntryType::Char, b""),
        ("block device", EntryType::Block, b""),
        ("fifo", EntryType::Fifo, b""),
    ];
    for (what, kind, target) in kinds {
        let bundle = zstd(&tar_of(&[
            (b"ok.txt", EntryType::Regular, b"fine"),
            (b"special", kind, target),
        ]));
        let err = archive_error(Bundle, &bundle, &Limits::default());
        assert!(
            matches!(&err, ArchiveError::SpecialFile { name, .. } if name == "special"),
            "{what}: {err:?}"
        );
    }
}

#[test]
fn tar_entry_count_total_size_and_nesting_are_limited() {
    let names: Vec<String> = (0..6).map(|i| format!("f{i}")).collect();
    let many: Vec<(&str, &[u8])> = names.iter().map(|n| (n.as_str(), &b"x"[..])).collect();
    let bundle = zstd(&files(&many));
    let limits = Limits {
        max_entries: 5,
        ..Limits::default()
    };
    assert!(matches!(
        archive_error(Bundle, &bundle, &limits),
        ArchiveError::TooManyEntries { limit: 5 }
    ));

    let mib: Vec<u8> = (0..1024 * 1024_u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8)
        .collect();
    let big = zstd(&files(&[("a", &mib), ("b", &mib), ("c", &mib)]));
    let limits = Limits {
        max_uncompressed_bytes: 2 * 1024 * 1024,
        ..Limits::default()
    };
    assert!(matches!(
        archive_error(Bundle, &big, &limits),
        ArchiveError::TooLarge { .. }
    ));

    let inner = zip_of(STORED, &[("x", b"y")]);
    let nested = zstd(&files(&[("inner.zip", &inner)]));
    assert!(matches!(
        archive_error(Bundle, &nested, &Limits::default()),
        ArchiveError::NestedArchive { .. }
    ));
    let inner_tar_zst = zstd(&files(&[("x", b"y")]));
    let nested = zstd(&files(&[("inner.tar.zst", &inner_tar_zst)]));
    assert!(matches!(
        archive_error(Bundle, &nested, &Limits::default()),
        ArchiveError::NestedArchive { .. }
    ));
}

#[test]
fn a_tar_zst_bomb_is_stopped_by_the_ratio() {
    let zeros = vec![0_u8; 48 * 1024 * 1024];
    let bomb = zstd(&files(&[("zeros.bin", &zeros)]));
    assert!(
        bomb.len() < 1024 * 1024,
        "the fixture really is a bomb: {} bytes",
        bomb.len()
    );
    let err = archive_error(Bundle, &bomb, &Limits::default());
    assert!(matches!(err, ArchiveError::RatioExceeded { .. }), "{err:?}");
}

#[test]
fn damaged_and_smuggling_tar_zst_is_refused() {
    let good = zstd(&files(&[("a.txt", b"hello hello hello")]));
    let second_frame = zstd(&files(&[("../evil", b"later frame")]));
    let window_bomb = [&[0x28, 0xB5, 0x2F, 0xFD, 0x00, 0xA0][..], &[0x55; 64]].concat(); // 1 GiB window

    let cases: Vec<(&str, Vec<u8>, &str)> = vec![
        ("truncated", good[..good.len() - 8].to_vec(), "malformed"),
        (
            "junk after the magic",
            [&[0x28, 0xB5, 0x2F, 0xFD][..], &[0x99; 100]].concat(),
            "malformed",
        ),
        ("not a tar inside", zstd(&[b'x'; 2000]), "malformed"),
        ("window of 1 GiB", window_bomb, "malformed"),
        (
            "bytes after the frame",
            [good.clone(), vec![0; 7]].concat(),
            "trailing",
        ),
        (
            "a second frame after the first",
            [good.clone(), second_frame].concat(),
            "trailing",
        ),
    ];
    for (name, bytes, expected) in cases {
        let result = check(Bundle, &bytes, &Limits::default());
        match (&result, expected) {
            (Err(VerifyError::Archive(ArchiveError::Malformed(_))), "malformed")
            | (Err(VerifyError::Archive(ArchiveError::TrailingData)), "trailing") => {}
            _ => panic!("{name}: expected {expected}, got {result:?}"),
        }
    }
}

// ---- hostile input never panics --------------------------------------------------------------

mod garbage {
    use proptest::prelude::*;

    use super::*;

    const CLASSES: [ArtifactClass; 5] = [Screenshot, Junit, HtmlReport, Log, Bundle];
    /// Real magic numbers, so random tails reach the deeper parsers.
    const MAGIC: [&[u8]; 8] = [
        b"\x89PNG\r\n\x1a\n",
        b"\xFF\xD8\xFF\xE0",
        b"RIFF\0\0\0\0WEBP",
        b"PK\x03\x04",
        b"PK\x05\x06",
        b"\x28\xB5\x2F\xFD",
        b"<?xml version=\"1.0\"?>",
        b"<!DOCTYPE html>",
    ];

    /// Verifying never panics and always returns a verdict; a tight limit keeps cases fast.
    fn survives(class: ArtifactClass, bytes: &[u8]) {
        let limits = Limits {
            max_bytes: 1 << 20,
            max_uncompressed_bytes: 1 << 20,
            max_entries: 64,
            ..Limits::default()
        };
        let _ = check(class, bytes, &limits);
        // Lying about size or digest on top of hostile content, too.
        let declared = Declared {
            size: bytes.len() as u64 + 1,
            sha256: Sha256Digest::of(b"x"),
        };
        let _ = verify(class, declared, Cursor::new(bytes), &limits);
    }

    fn mutate(base: &[u8], flips: &[(usize, u8)], cut: usize) -> Vec<u8> {
        let mut bytes = base.to_vec();
        for (at, value) in flips {
            let len = bytes.len();
            if let Some(b) = bytes.get_mut(at % len.max(1)) {
                *b = *value;
            }
        }
        bytes.truncate(bytes.len().saturating_sub(cut));
        bytes
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(96))]

        #[test]
        fn random_bytes_with_or_without_a_magic_prefix(
            magic in proptest::option::of(0..MAGIC.len()),
            tail in proptest::collection::vec(any::<u8>(), 0..3000),
            class in 0..CLASSES.len(),
        ) {
            let bytes = [magic.map_or(&[][..], |m| MAGIC[m]), &tail[..]].concat();
            survives(CLASSES[class], &bytes);
        }

        #[test]
        fn damaged_zip_and_tar_zst(
            flips in proptest::collection::vec((any::<usize>(), any::<u8>()), 0..6),
            cut in 0_usize..64,
            use_zip in any::<bool>(),
        ) {
            let base = if use_zip {
                zip_of(DEFLATED, &[("a/b.txt", b"hello hello hello hello"), ("c.bin", &[7; 900])])
            } else {
                zstd(&files(&[("a/b.txt", b"hello hello hello hello"), ("c.bin", &[7; 900])]))
            };
            survives(Bundle, &mutate(&base, &flips, cut));
        }
    }
}

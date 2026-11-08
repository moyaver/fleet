// SPDX-License-Identifier: AGPL-3.0-only
//! The offline bundle: `$RUN/fleet-bundle.tar.zst` (PLAN §6.7, issue #83).
//!
//! A run with no server (a fork PR has no OIDC token) keeps what it would have sent. The bundle
//! holds the journal (the events, as `journal.jsonl` records them), every artifact the run would
//! have uploaded, and a manifest. Each artifact goes through [`crate::upload::materialize`], the
//! same redaction a live upload gets (I6), so a bundle never holds more than the server would
//! have. `fleet-agent publish` reads the bundle back ([`crate::publish`]).
//!
//! The bundle is written when the run has ended, after Maestro has exited, so it cannot change
//! the run's output or exit status (I1, I2). Its failure is one notice on stderr.
//!
//! Layout, a tar file compressed with zstd:
//! - `manifest.json`: a [`Manifest`].
//! - `journal.jsonl`: the events, one per line.
//! - `artifacts/<n>`: the bytes of the artifact the manifest names as `n`.

use std::{
    fs::{self, File},
    io::{self, BufReader, Read},
    path::{Path, PathBuf},
};

use fleet_protocol::frame::CiContext;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::upload::{self, Item, Outcome, Source};

/// The bundle's file name in the run directory.
pub(crate) const FILE_NAME: &str = "fleet-bundle.tar.zst";
/// Where the bundle is built before it is complete.
const TAR_PART: &str = "fleet-bundle.tar.part";
const ZST_PART: &str = "fleet-bundle.zst.part";
/// The bundle format this build writes and reads.
pub(crate) const FORMAT: u32 = 1;
/// The journal inside the bundle, and in the run directory.
pub(crate) const JOURNAL: &str = "journal.jsonl";
/// The manifest inside the bundle.
pub(crate) const MANIFEST: &str = "manifest.json";
/// The directory of artifacts inside the bundle.
pub(crate) const ARTIFACTS: &str = "artifacts";

/// What the bundle holds, besides the journal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Manifest {
    /// [`FORMAT`].
    pub(crate) format: u32,
    /// The agent that wrote it.
    pub(crate) agent_version: String,
    /// The agent run that wrote the journal.
    pub(crate) agent_run_id: Uuid,
    /// The CI identity of the run, as the agent saw it.
    pub(crate) ci: CiContext,
    /// Artifacts in the bundle.
    pub(crate) artifacts: Vec<Artifact>,
    /// Artifacts the run found but could not put in the bundle, and why (a `RejectReason`).
    pub(crate) refused: Vec<Refused>,
}

/// One artifact in the bundle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Artifact {
    /// `ArtifactClass::as_str`.
    pub(crate) class: String,
    /// The path label, redacted.
    pub(crate) path: String,
    /// Its content type.
    pub(crate) content_type: String,
    /// The flow it is evidence for.
    pub(crate) flow: Option<Uuid>,
    /// The step that produced it.
    pub(crate) step: Option<u32>,
    /// Where it is in the bundle, `artifacts/<n>`.
    pub(crate) file: String,
    /// Its size in bytes.
    pub(crate) size: u64,
    /// Its SHA-256, lowercase hex.
    pub(crate) sha256: String,
}

/// An artifact that is not in the bundle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Refused {
    /// `ArtifactClass::as_str`.
    pub(crate) class: String,
    /// The path label.
    pub(crate) path: String,
    /// The flow it was evidence for.
    pub(crate) flow: Option<Uuid>,
    /// Why: the reason code a live upload would have journaled.
    pub(crate) reason: String,
}

/// What [`write`] put in the bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Written {
    /// The bundle file.
    pub(crate) path: PathBuf,
    /// Artifacts in it.
    pub(crate) artifacts: usize,
    /// Artifacts left out.
    pub(crate) refused: usize,
}

/// Writes the bundle into `run_dir` from the run's `journal` and the artifacts it would have
/// uploaded. The file appears under its final name only when it is complete.
///
/// # Errors
/// Returns why the bundle could not be written: the journal is missing, a file cannot be read or
/// written, or the compressor failed. Nothing is left under the final name then.
pub(crate) async fn write(
    run_dir: &Path,
    journal: &Path,
    items: Vec<Item>,
    agent_run_id: Uuid,
    ci: CiContext,
) -> Result<Written, String> {
    let handle = tokio::runtime::Handle::current();
    let (dir, journal_path) = (run_dir.to_owned(), journal.to_owned());
    let task = {
        let dir = dir.clone();
        tokio::task::spawn_blocking(move || {
            write_blocking(&handle, &dir, &journal_path, items, agent_run_id, ci)
        })
    };
    match task.await {
        Ok(written) => written,
        Err(e) => {
            // The writer panicked (the compressor does on an I/O error): nothing partial stays.
            let _ = fs::remove_file(dir.join(TAR_PART));
            let _ = fs::remove_file(dir.join(ZST_PART));
            Err(format!("the bundle writer stopped: {e}"))
        }
    }
}

fn write_blocking(
    handle: &tokio::runtime::Handle,
    run_dir: &Path,
    journal: &Path,
    items: Vec<Item>,
    agent_run_id: Uuid,
    ci: CiContext,
) -> Result<Written, String> {
    let tar_part = run_dir.join(TAR_PART);
    let zst_part = run_dir.join(ZST_PART);
    let finished = build_tar(handle, &tar_part, journal, items, agent_run_id, ci).and_then(
        |(artifacts, refused)| {
            // The compressor reads and writes with `unwrap`: a failure panics, which the blocking
            // task reports as a join error and the caller turns into this same error.
            let compressed = (|| -> io::Result<()> {
                let input = BufReader::new(File::open(&tar_part)?);
                let output = File::create(&zst_part)?;
                ruzstd::encoding::compress(
                    input,
                    output,
                    ruzstd::encoding::CompressionLevel::Fastest,
                );
                fs::rename(&zst_part, run_dir.join(FILE_NAME))
            })();
            compressed
                .map(|()| Written {
                    path: run_dir.join(FILE_NAME),
                    artifacts,
                    refused,
                })
                .map_err(|e| format!("compressing the bundle: {e}"))
        },
    );
    let _ = fs::remove_file(&tar_part);
    if finished.is_err() {
        let _ = fs::remove_file(&zst_part);
    }
    finished
}

/// Writes the tar file: artifacts first (each materialized, then dropped), then the journal, then
/// the manifest, which is last so it can count what went in.
fn build_tar(
    handle: &tokio::runtime::Handle,
    tar_part: &Path,
    journal: &Path,
    items: Vec<Item>,
    agent_run_id: Uuid,
    ci: CiContext,
) -> Result<(usize, usize), String> {
    let file = File::create(tar_part).map_err(|e| format!("creating the bundle: {e}"))?;
    let mut tar = tar::Builder::new(file);
    let mut artifacts = Vec::new();
    let mut refused = Vec::new();
    for (n, item) in items.into_iter().enumerate() {
        let class = item.class.as_str().to_owned();
        let (flow, step, path) = (item.flow, item.step, item.path.clone());
        let content_type = item.content_type.to_owned();
        match handle.block_on(upload::materialize(&item)) {
            Ok(source) => {
                let name = format!("{ARTIFACTS}/{n:06}");
                match append_source(&mut tar, &name, &source)? {
                    Some((size, sha256)) => artifacts.push(Artifact {
                        class,
                        path,
                        content_type,
                        flow,
                        step,
                        file: name,
                        size,
                        sha256,
                    }),
                    None => refused.push(Refused {
                        class,
                        path,
                        flow,
                        reason: "upload_failed".to_owned(),
                    }),
                }
            }
            Err(outcome) => refused.push(Refused {
                class,
                path,
                flow,
                reason: match outcome {
                    Outcome::Rejected(reason) | Outcome::Failed(reason) => reason,
                    Outcome::Uploaded { .. } => "not_uploaded".to_owned(),
                },
            }),
        }
    }
    let counts = (artifacts.len(), refused.len());
    tar.append_path_with_name(journal, JOURNAL)
        .map_err(|e| format!("reading {}: {e}", journal.display()))?;
    let manifest = Manifest {
        format: FORMAT,
        agent_version: env!("CARGO_PKG_VERSION").to_owned(),
        agent_run_id,
        ci,
        artifacts,
        refused,
    };
    let text = serde_json::to_vec_pretty(&manifest).map_err(|e| format!("manifest: {e}"))?;
    append_bytes(&mut tar, MANIFEST, &text)?;
    tar.into_inner()
        .and_then(|mut f| std::io::Write::flush(&mut f))
        .map_err(|e| format!("finishing the bundle: {e}"))?;
    Ok(counts)
}

/// Appends one materialized artifact and returns its size and digest. `None` when the file cannot
/// be opened: nothing is appended then, so the tar stays whole. An error means the tar is broken.
fn append_source(
    tar: &mut tar::Builder<File>,
    name: &str,
    source: &Source,
) -> Result<Option<(u64, String)>, String> {
    match source {
        Source::Bytes(bytes) => {
            append_bytes(tar, name, bytes)?;
            Ok(Some((bytes.len() as u64, hex(&Sha256::digest(bytes)))))
        }
        Source::File(path) => {
            let Ok(file) = File::open(path) else {
                return Ok(None);
            };
            let Ok(meta) = file.metadata() else {
                return Ok(None);
            };
            let mut header = header(meta.len());
            let mut reader = Hashing::new(file);
            tar.append_data(&mut header, name, &mut reader)
                .map_err(|e| format!("writing {name}: {e}"))?;
            Ok(Some((reader.size, hex(&reader.finish()))))
        }
        // `materialize` turns text and device logs into bytes.
        Source::Text(path, _) | Source::Logcat(upload::Logcat { path, .. }) => Err(format!(
            "{} was not read before it was bundled",
            path.display()
        )),
    }
}

fn append_bytes(tar: &mut tar::Builder<File>, name: &str, bytes: &[u8]) -> Result<(), String> {
    let mut header = header(bytes.len() as u64);
    tar.append_data(&mut header, name, bytes)
        .map_err(|e| format!("writing {name}: {e}"))
}

fn header(size: u64) -> tar::Header {
    let mut header = tar::Header::new_gnu();
    header.set_size(size);
    header.set_mode(0o644);
    header
}

/// Counts and digests what is read through it.
struct Hashing<R> {
    inner: R,
    hasher: Sha256,
    size: u64,
}

impl<R: Read> Hashing<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
            size: 0,
        }
    }

    fn finish(self) -> sha2::digest::Output<Sha256> {
        self.hasher.finalize()
    }
}

impl<R: Read> Read for Hashing<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hasher.update(&buf[..n]);
        self.size += n as u64;
        Ok(n)
    }
}

/// Lowercase hex, as the protocol writes digests.
pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

#[cfg(test)]
mod tests {
    use fleet_protocol::artifact::ArtifactClass;

    use super::*;

    #[test]
    fn hex_is_lowercase_and_padded() {
        assert_eq!(hex(&[0x0a, 0xff, 0x00]), "0aff00");
    }

    #[tokio::test]
    async fn an_unreadable_artifact_is_refused_and_the_rest_is_kept() {
        let dir = std::env::temp_dir().join(format!("fleet-bundle-unit-{}", Uuid::now_v7()));
        fs::create_dir_all(&dir).unwrap();
        let journal = dir.join(JOURNAL);
        fs::write(&journal, "{}\n").unwrap();
        let items = vec![
            Item {
                class: ArtifactClass::Commands,
                path: "commands.json".into(),
                content_type: "application/json",
                flow: None,
                step: None,
                source: Source::Bytes(b"[]".to_vec().into()),
            },
            Item {
                class: ArtifactClass::FailureScreenshot,
                path: "gone.png".into(),
                content_type: "image/png",
                flow: None,
                step: None,
                source: Source::File(dir.join("missing.png")),
            },
        ];
        let written = write(
            &dir,
            &journal,
            items,
            Uuid::now_v7(),
            crate::ci::context(|_| None),
        )
        .await
        .unwrap();
        assert_eq!((written.artifacts, written.refused), (1, 1));

        let file = fs::File::open(&written.path).unwrap();
        let mut archive = tar::Archive::new(ruzstd::decoding::StreamingDecoder::new(file).unwrap());
        let mut manifest = None;
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            if entry.path().unwrap().to_string_lossy() == MANIFEST {
                let mut text = String::new();
                entry.read_to_string(&mut text).unwrap();
                manifest = Some(serde_json::from_str::<Manifest>(&text).unwrap());
            }
        }
        let manifest = manifest.expect("a manifest");
        assert_eq!(manifest.refused.len(), 1);
        assert_eq!(manifest.refused[0].path, "gone.png");
        assert_eq!(manifest.refused[0].reason, "upload_failed");
        let _ = fs::remove_dir_all(&dir);
    }
}

// SPDX-License-Identifier: AGPL-3.0-only
//! `fleet-agent publish <path>`: sends a finished run to the server (issue #83).
//!
//! The input is either:
//! - an offline bundle (`fleet-bundle.tar.zst`, or its extracted directory; see [`crate::bundle`]).
//!   Its journal is sent as it was recorded, and its artifacts are uploaded as they were redacted
//!   when the run ended; or
//! - a Maestro output directory (a capture, or a saved debug directory). It is replayed through the
//!   same session the live observer drives ([`crate::inspect::replay`]), and its files are uploaded
//!   through the live upload path.
//!
//! Either way the events go over the live transport ([`crate::transport`]) and the artifacts
//! through the live uploader ([`crate::upload`]), so the server takes the run through the same
//! ingest as a live one. The run gets a new agent run id. After the fact, live console output is
//! not live: its events are sent as debug-log evidence (I4).
//!
//! Needs `FLEET_SERVER`, and `FLEET_TOKEN` or the CI OIDC variables, as a live run does.

use std::{
    fs::{self, File},
    io::{self, Read},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, atomic::Ordering},
    time::{Duration, Instant},
};

use anyhow::{Context as _, anyhow, bail};
use fleet_protocol::{
    artifact::ArtifactClass,
    event::{EventBody, JournalGap},
    frame::{CiContext, Telemetry},
    journal::{ClockSample, EventEnvelope, Provenance, SourceKind, Subject},
};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use tokio::{sync::mpsc, task::JoinHandle};
use uuid::Uuid;

use crate::{
    bundle::{self, Manifest},
    inspect::{self, Replayed},
    journal,
    observer::{self, candidate_item},
    outbox::{Budget, Outbox},
    transport::{self, Shared},
    upload::{self, Done, Item, Outcome, Source, Uploader},
};

/// How long the uploads may take, unless `FLEET_UPLOAD_TIMEOUT` says otherwise.
const DEFAULT_UPLOAD_BUDGET: Duration = Duration::from_secs(120);
/// How long the server may take to take every event, unless `FLEET_FLUSH_TIMEOUT` says otherwise.
const DEFAULT_FLUSH_BUDGET: Duration = Duration::from_secs(60);

/// What a publish sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Published {
    /// The agent run the server took.
    pub(crate) run: Uuid,
    /// Events sent.
    pub(crate) events: usize,
    /// Artifacts the server took.
    pub(crate) uploaded: usize,
    /// Artifacts not sent, or not taken.
    pub(crate) not_uploaded: usize,
}

/// Publishes the run at `input`. Nothing is kept on disk afterwards.
///
/// # Errors
/// Returns why the run could not be published: no server configured, no credential, an input
/// that is neither a bundle nor a Maestro output directory, or a server that refused or did not
/// take the run.
pub(crate) fn run(input: &Path) -> anyhow::Result<Published> {
    // Installed like the observer does: TLS to the server needs a crypto provider.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let url = observer::server_url()
        .context("FLEET_SERVER is not set: publish sends the run to a Fleet server")?;
    // Checked before anything is read or sent: without a credential the transport would only
    // report the run as refused, which does not say what to set.
    if !crate::net::has_credential(|k| std::env::var(k).ok()) {
        bail!(
            "publish needs a credential: set FLEET_TOKEN, or the CI OIDC variables \
             (ACTIONS_ID_TOKEN_REQUEST_URL and ACTIONS_ID_TOKEN_REQUEST_TOKEN)"
        );
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("starting the runtime")?;
    let work = std::env::temp_dir().join(format!("fleet-publish-{}", Uuid::now_v7()));
    fs::create_dir_all(&work).with_context(|| format!("creating {}", work.display()))?;
    let result = rt.block_on(publish(input, &url, &work));
    let _ = fs::remove_dir_all(&work);
    result
}

async fn publish(input: &Path, url: &str, work: &Path) -> anyhow::Result<Published> {
    let start = Instant::now();
    if is_bundle_dir(input) || is_bundle_file(input) {
        publish_bundle(input, url, work, start).await
    } else if input.is_dir() {
        publish_saved(input, url, work, start).await
    } else {
        bail!(
            "{} is neither an offline bundle nor a Maestro output directory",
            input.display()
        )
    }
}

fn is_bundle_dir(path: &Path) -> bool {
    path.is_dir() && path.join(bundle::MANIFEST).is_file()
}

fn is_bundle_file(path: &Path) -> bool {
    path.is_file()
        && path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.ends_with(".tar.zst"))
}

/// Publishes an offline bundle: its journal as recorded, then its artifacts, then the run's end.
async fn publish_bundle(
    input: &Path,
    url: &str,
    work: &Path,
    start: Instant,
) -> anyhow::Result<Published> {
    let (input_owned, work_owned) = (input.to_owned(), work.to_owned());
    let loaded = tokio::task::spawn_blocking(move || load_bundle(&input_owned, &work_owned))
        .await
        .context("reading the bundle")??;
    let Loaded {
        manifest,
        mut events,
        truncated,
        uploads,
    } = loaded;
    imported(&mut events);
    let end = match truncated {
        // The journal stopped at its cap: the events from `first_missing_seq` on are not in the
        // bundle, so they are reported as a gap, and the run is not ended here.
        Some(t) => {
            events.retain(|e| e.seq < t.first_missing_seq);
            events.push(EventEnvelope::new(
                t.first_missing_seq,
                clock_at(start),
                Subject::default(),
                Provenance::new(SourceKind::AgentObserved),
                &EventBody::JournalGap(JournalGap {
                    from_seq: t.first_missing_seq,
                    to_seq: t.first_missing_seq,
                    reason: format!("the bundle's journal stopped ({})", t.reason),
                }),
            ));
            None
        }
        None if events.last().is_some_and(|e| e.kind == "run.ended") => events.pop(),
        None => None,
    };
    let run = Uuid::now_v7();
    let before = events.len();
    let sender = Sender::start(url, run, manifest.ci, events, work, start)?;
    let dones = sender.upload(uploads).await;
    let mut next = sender.next_seq();
    let mut post = Vec::new();
    for done in dones.iter().cloned() {
        post.push(artifact_event(next, sender.now(), done));
        next += 1;
    }
    if let Some(mut end) = end {
        end.seq = next;
        post.push(end);
    }
    let sent = before + post.len();
    sender
        .finish(post, budget("FLEET_FLUSH_TIMEOUT", DEFAULT_FLUSH_BUDGET))
        .await?;
    Ok(summary(run, sent, &dones))
}

/// Publishes a Maestro output directory: replayed through a session, its files uploaded, and the
/// run ended.
async fn publish_saved(
    input: &Path,
    url: &str,
    work: &Path,
    start: Instant,
) -> anyhow::Result<Published> {
    let dir = input.to_owned();
    let mut replayed = tokio::task::spawn_blocking(move || {
        inspect::replay(&dir, Box::new(Uuid::now_v7)).map(|r| (r, dir))
    })
    .await
    .context("reading the run")??;
    let (replayed, dir) = &mut replayed;
    let mut events = replayed.session.drain();
    imported(&mut events);
    let before = events.len();
    let ci = crate::ci::context(|k| std::env::var(k).ok());
    let run = Uuid::now_v7();
    let pending = saved_uploads(replayed, dir);
    let sender = Sender::start(url, run, ci, events, work, start)?;
    let dones = sender.upload(pending).await;
    let last = replayed.last_ms;
    for done in dones.iter().cloned() {
        let at = replayed.at(last);
        replayed
            .session
            .artifact_result(at, done.flow, &observer::artifact_body(done));
    }
    replayed.session.end(replayed.at(last), 0);
    let mut post = replayed.session.drain();
    imported(&mut post);
    let sent = before + post.len();
    sender
        .finish(post, budget("FLEET_FLUSH_TIMEOUT", DEFAULT_FLUSH_BUDGET))
        .await?;
    Ok(summary(run, sent, &dones))
}

/// What a bundle holds, read and checked.
struct Loaded {
    manifest: Manifest,
    /// The journal's events, in order.
    events: Vec<EventEnvelope>,
    /// Set when the journal stopped early.
    truncated: Option<journal::Truncation>,
    /// The artifacts to upload, and the ones the run refused.
    uploads: Vec<Pending>,
}

/// Reads a bundle (a directory, or a `.tar.zst` extracted into `work`), checks each artifact's
/// size and digest against the manifest, and returns what to send.
fn load_bundle(input: &Path, work: &Path) -> anyhow::Result<Loaded> {
    let root = if input.is_dir() {
        input.to_owned()
    } else {
        let root = work.join("bundle");
        extract(input, &root)?;
        root
    };
    let manifest: Manifest = serde_json::from_slice(
        &fs::read(root.join(bundle::MANIFEST)).context("reading the bundle manifest")?,
    )
    .context("reading the bundle manifest")?;
    if manifest.format != bundle::FORMAT {
        bail!(
            "the bundle is format {}; this agent reads format {}",
            manifest.format,
            bundle::FORMAT
        );
    }
    let text =
        fs::read_to_string(root.join(bundle::JOURNAL)).context("reading the bundle journal")?;
    let journal = journal::parse(&text);
    let mut uploads = Vec::new();
    for a in &manifest.artifacts {
        let Some(class) = ArtifactClass::parse(&a.class) else {
            continue;
        };
        let Some(file) = artifact_file(&root, &a.file) else {
            uploads.push(Pending::Refused(refused_done(
                class,
                &a.path,
                a.flow,
                "bundle_corrupt",
            )));
            continue;
        };
        match digest(&file) {
            Ok((size, sha)) if size == a.size && sha == a.sha256 => {
                uploads.push(Pending::Upload(Item {
                    class,
                    path: a.path.clone(),
                    content_type: content_type(&a.content_type),
                    flow: a.flow,
                    step: a.step,
                    source: Source::File(file),
                }));
            }
            _ => uploads.push(Pending::Refused(refused_done(
                class,
                &a.path,
                a.flow,
                "bundle_corrupt",
            ))),
        }
    }
    for r in &manifest.refused {
        if let Some(class) = ArtifactClass::parse(&r.class) {
            uploads.push(Pending::Refused(refused_done(
                class, &r.path, r.flow, &r.reason,
            )));
        }
    }
    Ok(Loaded {
        manifest,
        events: journal.events,
        truncated: journal.truncated,
        uploads,
    })
}

/// Extracts the bundle's own files (the manifest, the journal and `artifacts/<n>`) into `dest`.
/// Other entries are skipped, so nothing is written outside `dest`.
fn extract(bundle: &Path, dest: &Path) -> anyhow::Result<()> {
    let file = File::open(bundle).with_context(|| format!("opening {}", bundle.display()))?;
    let decoder = ruzstd::decoding::StreamingDecoder::new(file)
        .map_err(|e| anyhow!("{} is not a zstd bundle: {e}", bundle.display()))?;
    let mut archive = tar::Archive::new(decoder);
    fs::create_dir_all(dest.join(bundle::ARTIFACTS))?;
    for entry in archive.entries().context("reading the bundle")? {
        let mut entry = entry.context("reading the bundle")?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let name = entry
            .path()
            .context("reading the bundle")?
            .to_string_lossy()
            .into_owned();
        let target = if name == bundle::MANIFEST || name == bundle::JOURNAL {
            dest.join(&name)
        } else if let Some(file) = artifact_file(dest, &name) {
            file
        } else {
            continue;
        };
        let mut out =
            File::create(&target).with_context(|| format!("writing {}", target.display()))?;
        io::copy(&mut entry, &mut out).context("reading the bundle")?;
    }
    Ok(())
}

/// `root/artifacts/<digits>` for an artifact's name in the bundle, or `None` for any other name.
fn artifact_file(root: &Path, name: &str) -> Option<PathBuf> {
    let digits = name.strip_prefix(&format!("{}/", bundle::ARTIFACTS))?;
    (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
        .then(|| root.join(bundle::ARTIFACTS).join(digits))
}

/// Size and SHA-256 of a file, streamed.
fn digest(path: &Path) -> io::Result<(u64, String)> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut size = 0_u64;
    let mut buf = vec![0_u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        size += n as u64;
    }
    Ok((size, bundle::hex(&hasher.finalize())))
}

/// The content type a manifest names, as the upload protocol takes it. Unknown names are binary.
fn content_type(name: &str) -> &'static str {
    match name {
        "image/png" => "image/png",
        "image/jpeg" => "image/jpeg",
        "image/webp" => "image/webp",
        "text/plain" => "text/plain",
        "text/html" => "text/html",
        "application/xml" => "application/xml",
        "application/json" => "application/json",
        _ => "application/octet-stream",
    }
}

/// An artifact to send: uploaded, or already refused.
enum Pending {
    Upload(Item),
    Refused(Done),
}

fn refused_done(class: ArtifactClass, path: &str, flow: Option<Uuid>, reason: &str) -> Done {
    Done {
        class,
        path: path.to_owned(),
        flow,
        outcome: Outcome::Rejected(reason.to_owned()),
    }
}

/// The artifacts a replayed directory has: its session's uploads, its JUnit report and its
/// `maestro.log`, read and redacted as a live run reads them.
fn saved_uploads(replayed: &mut Replayed, dir: &Path) -> Vec<Pending> {
    let debug = replayed.debug.clone();
    let mut out = Vec::new();
    for c in replayed.session.take_uploads() {
        if let Some(item) = candidate_item(&replayed.session, debug.as_deref(), &c) {
            out.push(Pending::Upload(item));
        }
    }
    let report = [dir.join("report.xml"), dir.join("run/report.xml")]
        .into_iter()
        .chain(debug.as_ref().map(|d| d.join("report.xml")))
        .find(|p| p.is_file());
    if let Some(report) = report
        && let Ok(bytes) = fs::read(&report)
    {
        let redactor = replayed.session.redactor();
        match upload::prepare_text(&bytes, redactor) {
            Ok(text) => out.push(Pending::Upload(Item {
                class: ArtifactClass::Junit,
                path: "report.xml".into(),
                content_type: "application/xml",
                flow: None,
                step: None,
                source: Source::Bytes(text.into()),
            })),
            Err(reason) => out.push(Pending::Refused(refused_done(
                ArtifactClass::Junit,
                "report.xml",
                None,
                reason,
            ))),
        }
    }
    if let Some(log) = debug.as_ref().map(|d| d.join("maestro.log"))
        && log.is_file()
    {
        out.push(Pending::Upload(Item {
            class: ArtifactClass::MaestroLog,
            path: "maestro.log".into(),
            content_type: "text/plain",
            flow: None,
            step: None,
            source: Source::Text(log, Arc::new(replayed.session.redactor().clone())),
        }));
    }
    out
}

/// Events from a recorded run, sent as they were recorded, except live console output: after the
/// fact it is parsed debug-log evidence, not live (I4).
fn imported(events: &mut [EventEnvelope]) {
    for e in events {
        if e.src.kind == SourceKind::LiveConsole {
            e.src.kind = SourceKind::DebugLog;
        }
    }
}

/// An `artifact.uploaded` or `artifact.rejected` event for an upload's outcome.
fn artifact_event(seq: u64, at: ClockSample, done: Done) -> EventEnvelope {
    let subject = Subject {
        flow_execution_id: done.flow,
        ..Subject::default()
    };
    let body = observer::artifact_body(done);
    EventEnvelope::new(
        seq,
        at,
        subject,
        Provenance::new(SourceKind::AgentObserved),
        &body,
    )
}

impl Published {
    /// One line for the terminal.
    pub(crate) fn describe(&self) -> String {
        format!(
            "published run {}: {} events, {} artifacts uploaded, {} not uploaded",
            self.run, self.events, self.uploaded, self.not_uploaded
        )
    }
}

fn summary(run: Uuid, events: usize, dones: &[Done]) -> Published {
    let uploaded = dones
        .iter()
        .filter(|d| matches!(d.outcome, Outcome::Uploaded { .. }))
        .count();
    Published {
        run,
        events,
        uploaded,
        not_uploaded: dones.len() - uploaded,
    }
}

/// One publish's transport and uploader: the events go out as the transport takes them.
struct Sender {
    shared: Arc<Shared>,
    transport: JoinHandle<transport::End>,
    uploader: Uploader,
    run: Uuid,
    start: Instant,
    /// The sequence number of the next event.
    next: u64,
    /// Keeps the transport's telemetry channel open until the publish ends.
    _telemetry: mpsc::Sender<Telemetry>,
}

impl Sender {
    /// Starts sending `events` (already sequenced from 1) for `run`.
    fn start(
        url: &str,
        run: Uuid,
        ci: CiContext,
        events: Vec<EventEnvelope>,
        work: &Path,
        start: Instant,
    ) -> anyhow::Result<Self> {
        let next = events.last().map_or(1, |e| e.seq + 1);
        let mut outbox = Outbox::with_spool(&work.join("spool.bin"), run, Budget::default());
        outbox.push(events);
        let shared = Arc::new(Shared {
            outbox: Mutex::new(outbox),
            ..Shared::default()
        });
        let cfg = transport::Config::from_env(url.to_owned(), run, ci, |k| std::env::var(k).ok());
        let uploader = Uploader::new(upload::Config {
            base: crate::net::http_base(url),
            credential: Arc::clone(&shared.credential),
            agent_run_id: run,
            rate: std::env::var("FLEET_UPLOAD_RATE")
                .ok()
                .and_then(|v| v.parse().ok())
                .filter(|r| *r > 0)
                .unwrap_or(upload::DEFAULT_RATE),
            retry_delay: Duration::from_secs(1),
            tls: Arc::clone(&cfg.tls),
        })
        .context("building the upload client")?;
        let (telemetry, rx) = mpsc::channel(1);
        let transport = tokio::spawn(transport::run(cfg, Arc::clone(&shared), rx, move || {
            clock_at(start)
        }));
        Ok(Self {
            shared,
            transport,
            uploader,
            run,
            start,
            next,
            _telemetry: telemetry,
        })
    }

    /// The sequence number of the next event.
    fn next_seq(&self) -> u64 {
        self.next
    }

    /// The clock of this publish.
    fn now(&self) -> ClockSample {
        clock_at(self.start)
    }

    /// Uploads `pending` and waits for the results, within the upload budget. Refused artifacts
    /// come back as they are.
    async fn upload(&self, pending: Vec<Pending>) -> Vec<Done> {
        let mut dones = Vec::new();
        for p in pending {
            match p {
                Pending::Upload(item) => self.uploader.submit(item),
                Pending::Refused(done) => dones.push(done),
            }
        }
        dones.extend(
            self.uploader
                .finish(budget("FLEET_UPLOAD_TIMEOUT", DEFAULT_UPLOAD_BUDGET))
                .await,
        );
        dones
    }

    /// Sends what is left, ends the run, and waits for the server to take every event.
    async fn finish(self, events: Vec<EventEnvelope>, budget: Duration) -> anyhow::Result<()> {
        {
            let mut outbox = self
                .shared
                .outbox
                .lock()
                .map_err(|_| anyhow!("the outbox lock is poisoned"))?;
            outbox.push(events);
            outbox.seal();
        }
        self.shared.finishing.store(true, Ordering::SeqCst);
        self.shared.wake.notify_one();
        let run = self.run;
        match tokio::time::timeout(budget, self.transport).await {
            Ok(Ok(transport::End::Flushed)) => Ok(()),
            Ok(Ok(transport::End::Refused(why))) => bail!("the server refused run {run}: {why}"),
            Ok(Err(e)) => bail!("sending run {run}: {e}"),
            Err(_) => bail!(
                "the server did not take every event of run {run} within {}s",
                budget.as_secs()
            ),
        }
    }
}

fn budget(var: &str, default: Duration) -> Duration {
    std::env::var(var)
        .ok()
        .and_then(|s| s.parse().ok())
        .map_or(default, Duration::from_secs)
}

/// The clock of a publish that started at `start`: wall time, and milliseconds since then.
fn clock_at(start: Instant) -> ClockSample {
    ClockSample {
        wall: OffsetDateTime::now_utc(),
        mono_ms: u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_bundles_own_artifact_names_are_files() {
        let root = Path::new("/bundle");
        assert_eq!(
            artifact_file(root, "artifacts/000012"),
            Some(root.join("artifacts").join("000012"))
        );
        for bad in [
            "artifacts/",
            "artifacts/../manifest.json",
            "artifacts/1/../2",
            "manifest.json",
            "artifacts/x",
        ] {
            assert_eq!(artifact_file(root, bad), None, "{bad}");
        }
    }

    #[test]
    fn live_console_output_is_not_published_as_live() {
        let at = ClockSample {
            wall: OffsetDateTime::UNIX_EPOCH,
            mono_ms: 0,
        };
        let mut events = vec![EventEnvelope::new(
            1,
            at,
            Subject::default(),
            Provenance::new(SourceKind::LiveConsole),
            &EventBody::JournalGap(JournalGap {
                from_seq: 0,
                to_seq: 0,
                reason: String::new(),
            }),
        )];
        imported(&mut events);
        assert_eq!(events[0].src.kind, SourceKind::DebugLog);
    }

    #[test]
    fn a_captured_run_publishes_its_screenshot_commands_log_and_report() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/maestro/2.12.1/fail");
        let mut replayed = inspect::replay(&dir, Box::new(Uuid::now_v7)).unwrap();
        let mut classes: Vec<&str> = saved_uploads(&mut replayed, &dir)
            .iter()
            .map(|p| match p {
                Pending::Upload(item) => item.class.as_str(),
                Pending::Refused(done) => done.class.as_str(),
            })
            .collect();
        classes.sort_unstable();
        for want in ["commands", "failure_screenshot", "junit", "maestro_log"] {
            assert!(classes.contains(&want), "{want} in {classes:?}");
        }
    }
}

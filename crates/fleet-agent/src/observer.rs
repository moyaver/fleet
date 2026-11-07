// SPDX-License-Identifier: AGPL-3.0-only
//! The observer process (`fleet-agent __observe`; research 02 §15).
//!
//! It turns the supervisor's frames, the session `maestro.log` and the flow bundles into journal
//! events, keeps them in the outbox (spooled to disk and written to `journal.jsonl`, up to its cap;
//! see [`crate::journal`]), streams them to the server if one is configured, and samples metrics.
//! Everything here is best-effort: the supervisor never depends on it, and it finishes within a
//! bounded time.
//!
//! Configuration (environment): `FLEET_SERVER` (`ws(s)://` or `http(s)://` base URL; unset =
//! offline), `FLEET_TOKEN`, `FLEET_FLUSH_TIMEOUT` (seconds, default 20), plus the CI variables in
//! [`crate::ci`].

use std::{
    fs::File,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use fleet_core::redact::Redactor;
use fleet_maestro::{ReportState, stdout::strip_ansi};
use fleet_protocol::{
    artifact::ArtifactClass,
    event::{ArtifactRejected, ArtifactUploaded, EventBody, EvidenceSources},
    frame::Telemetry,
    journal::ClockSample,
};
use time::OffsetDateTime;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::{
    ipc::{Control, Frame, Stream},
    journal::Writer,
    metrics::Sampler,
    outbox::{Budget, Outbox},
    session::{Observation, Session},
    transport::{self, Shared},
    upload::{self, Outcome, Source, Uploader},
    watch::{BundleScanner, LogTailer},
};

/// Fault injection for tests (research 02 §14): `observer_panic`, `observer_abort`,
/// `observer_hang`, `observer_exit`.
const FAULT_ENV: &str = "FLEET_FAULT";
/// Environment variable carrying the supervisor's PID, set by the supervisor when it spawns us.
pub(crate) const SUPERVISOR_PID_ENV: &str = "FLEET_SUPERVISOR_PID";
/// Where the supervisor asks the observer to write its own log.
pub(crate) const LOG_ENV: &str = "FLEET_OBSERVER_LOG";

const POLL: Duration = Duration::from_millis(250);
const SAMPLE: Duration = Duration::from_secs(5);
const MAX_LINE: usize = 64 * 1024;

/// Runs the observer until the run is finalized or the supervisor goes away.
///
/// # Errors
/// Returns I/O errors from the control pipe or the runtime.
pub(crate) fn run() -> io::Result<()> {
    // Same process group as the supervisor: a CI Ctrl-C reaches us too, and we must outlive it
    // long enough to finalize. The supervisor decides when we stop.
    for sig in [
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGHUP,
    ] {
        let _ =
            signal_hook::flag::register(sig, Arc::new(std::sync::atomic::AtomicBool::new(false)));
    }
    match std::env::var(FAULT_ENV).as_deref() {
        Ok("observer_panic") => panic!("injected observer panic"),
        Ok("observer_abort") => std::process::abort(),
        Ok("observer_exit") => return Err(io::Error::other("injected observer exit")),
        Ok("observer_hang") => loop {
            std::thread::sleep(Duration::from_secs(3600));
        },
        _ => {}
    }
    if let Some(path) = std::env::var_os(LOG_ENV)
        && let Ok(file) = File::create(path)
    {
        let _ = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(std::sync::Mutex::new(file))
            .try_init();
    }
    let _ = rustls::crypto::ring::default_provider().install_default();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let mut out = io::stdout().lock();
    writeln!(out, "ready")?;
    out.flush()?;
    let maestro = rt.block_on(observe())?;
    // Before "finalized", which the supervisor waits for: the version the session log named.
    if let Some(version) = maestro {
        writeln!(out, "maestro-version {version}")?;
    }
    writeln!(out, "finalized")?;
    out.flush()
}

/// Splits a byte stream into lines, capped at [`MAX_LINE`].
#[derive(Debug, Default)]
struct Lines {
    buf: Vec<u8>,
}

impl Lines {
    fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        while let Some(i) = self.buf.iter().position(|b| *b == b'\n' || *b == b'\r') {
            let line: Vec<u8> = self.buf.drain(..=i).collect();
            out.push(clean(&line));
        }
        if self.buf.len() > MAX_LINE {
            out.push(clean(&std::mem::take(&mut self.buf)));
        }
        out.retain(|l| !l.is_empty());
        out
    }
}

fn clean(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    strip_ansi(text.trim_end_matches(['\r', '\n'])).into_owned()
}

/// Everything that exists once Maestro has started.
struct Live {
    session: Session,
    tailer: Option<LogTailer>,
    scanner: Option<BundleScanner>,
    report: Option<PathBuf>,
    /// When Maestro was spawned: a report older than this (less [`MTIME_TOLERANCE`]) is not ours.
    started: SystemTime,
    pgid: Option<u32>,
    stdout: Lines,
    sampler: Sampler,
    shared: Arc<Shared>,
    journal: Option<Writer>,
    telemetry: Option<mpsc::Sender<Telemetry>>,
    transport: Option<tokio::task::JoinHandle<transport::End>>,
    tseq: u64,
    dropped_bytes: u64,
    devices: mpsc::Receiver<Vec<(String, fleet_protocol::event::AdbState)>>,
    /// Uploads artifacts (PLAN §6.6); `None` offline or with `FLEET_UPLOAD_ARTIFACTS=off`.
    uploader: Option<Uploader>,
    /// Maestro's debug output directory: where the files to upload are.
    debug: Option<PathBuf>,
}

/// Observes the run until it ends, and returns the Maestro version its session log named, if any.
#[expect(
    clippy::too_many_lines,
    reason = "one select loop over the observer's frames, which owns the run's state"
)]
async fn observe() -> io::Result<Option<String>> {
    let (frames_tx, mut frames) = mpsc::channel::<Frame>(1024);
    std::thread::spawn(move || {
        let mut input = io::stdin().lock();
        while let Ok(Some(f)) = Frame::read_from(&mut input) {
            if frames_tx.blocking_send(f).is_err() {
                return;
            }
        }
    });
    let t0 = Instant::now();
    let clock = move || ClockSample {
        wall: OffsetDateTime::now_utc(),
        mono_ms: u64::try_from(t0.elapsed().as_millis()).unwrap_or(u64::MAX),
    };
    let mut live: Option<Live> = None;
    let mut poll = tokio::time::interval(POLL);
    let mut sample = tokio::time::interval(SAMPLE);
    loop {
        tokio::select! {
            frame = frames.recv() => match frame {
                Some(Frame::Control(Control::ChildStarted { pid, pgid, argv, started_at_ms, debug_dir, report, run_dir, plan, evidence, journal_max_bytes })) => {
                    let mut l = start(&argv, pid, pgid, debug_dir, report, run_dir.as_deref().map(Path::new), evidence.map(|e| *e), UNIX_EPOCH + Duration::from_millis(started_at_ms), clock(), clock, journal_max_bytes);
                    if let Some(plan) = plan {
                        l.session.observe(clock(), Observation::Plan(plan));
                    }
                    flush(&mut l);
                    live = Some(l);
                }
                Some(Frame::Output(Stream::Stdout, bytes)) => {
                    if let Some(l) = live.as_mut() {
                        let lines = l.stdout.push(&bytes);
                        if !lines.is_empty() {
                            // Maestro logs a flow's start before it prints its result. Catch up
                            // on the log first, so a late-starting observer never reads a result
                            // ahead of the start it belongs to (which would split one flow in two).
                            watch(l, false, &clock);
                        }
                        for line in lines {
                            l.session.observe(clock(), Observation::Stdout(line));
                        }
                        flush(l);
                    }
                }
                Some(Frame::Output(Stream::Stderr, _)) => {}
                Some(Frame::Control(Control::Gap { bytes, .. })) => {
                    if let Some(l) = live.as_mut() {
                        l.dropped_bytes += bytes;
                    }
                }
                Some(Frame::Control(Control::Signal { signal })) => {
                    if let Some(l) = live.as_mut() {
                        l.session.observe(clock(), Observation::Signal(signal));
                        flush(l);
                    }
                }
                Some(Frame::Control(Control::ChildExited { code, signal, output_bytes_dropped })) => {
                    let maestro = match live.take() {
                        Some(l) => finalize(l, (code, signal), output_bytes_dropped, clock).await,
                        None => None,
                    };
                    return Ok(maestro);
                }
                None => {
                    // The supervisor is gone while Maestro may still run: stop Maestro (research 02 §1.5).
                    if let Some(mut l) = live.take() {
                        if let Some(pgid) = l.pgid.and_then(|p| i32::try_from(p).ok()) {
                            let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pgid), nix::sys::signal::Signal::SIGTERM);
                        }
                        l.session.observe(clock(), Observation::Signal(15));
                        return Ok(finalize(l, (None, None), 0, clock).await);
                    }
                    return Ok(None);
                }
            },
            _ = poll.tick() => {
                if let Some(l) = live.as_mut() {
                    watch(l, false, &clock);
                    pump_uploads(l, &clock);
                    flush(l);
                }
            }
            list = async {
                match live.as_mut() {
                    Some(l) => l.devices.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                if let (Some(l), Some(list)) = (live.as_mut(), list) {
                    l.session.observe(clock(), Observation::Devices(list));
                    flush(l);
                }
            }
            _ = sample.tick() => {
                if let Some(l) = live.as_mut() {
                    let s = l.sampler.sample(clock());
                    l.tseq += 1;
                    if let Some(t) = &l.telemetry {
                        let _ = t.try_send(Telemetry { tseq_from: l.tseq, tseq_to: l.tseq, dropped_since_last: 0, samples: vec![s] });
                    }
                }
            }
        }
    }
}

fn configure_workspace(session: &mut Session, argv: &[String]) {
    let cwd = std::env::current_dir().ok();
    let git_root = |path: &Path| {
        path.ancestors()
            .find(|p| p.join(".git").exists())
            .map(Path::to_path_buf)
    };
    let workspace = std::env::var_os("GITHUB_WORKSPACE")
        .map(PathBuf::from)
        .or_else(|| {
            fleet_maestro::invocation::parse(argv)
                .positionals
                .iter()
                .map(Path::new)
                .filter(|p| p.is_absolute())
                .find_map(git_root)
        })
        .or_else(|| cwd.as_deref().and_then(git_root));
    session.set_workspace(workspace, cwd);
}

#[expect(
    clippy::too_many_arguments,
    reason = "everything ChildStarted carries, unpacked once"
)]
fn start(
    argv: &[String],
    pid: u32,
    group: Option<u32>,
    debug_dir: Option<String>,
    report: Option<String>,
    run_dir: Option<&Path>,
    evidence: Option<EvidenceSources>,
    started: SystemTime,
    at: ClockSample,
    clock: impl Fn() -> ClockSample + Send + Sync + 'static,
    journal_max_bytes: u64,
) -> Live {
    let agent_run_id = Uuid::now_v7();
    let redactor = Redactor::for_run(
        argv,
        std::env::vars()
            .collect::<Vec<_>>()
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str())),
    );
    let mut session = Session::new(argv, evidence, redactor, Box::new(Uuid::now_v7), at);
    configure_workspace(&mut session, argv);
    let outbox = run_dir.map_or_else(Outbox::default, |d| {
        Outbox::with_spool(&d.join("spool.bin"), agent_run_id, Budget::default())
    });
    let shared = Arc::new(Shared {
        outbox: std::sync::Mutex::new(outbox),
        ..Shared::default()
    });
    let journal =
        run_dir.and_then(|d| Writer::create(&d.join("journal.jsonl"), journal_max_bytes).ok());
    let mut uploader = None;
    let (telemetry, transport) = match server_url() {
        Some(url) => {
            let (tx, rx) = mpsc::channel(64);
            uploader = uploads_enabled().then(|| {
                Uploader::new(upload::Config {
                    base: crate::net::http_base(&url),
                    credential: Arc::clone(&shared.credential),
                    agent_run_id,
                    rate: std::env::var("FLEET_UPLOAD_RATE")
                        .ok()
                        .and_then(|v| v.parse().ok())
                        .filter(|r| *r > 0)
                        .unwrap_or(upload::DEFAULT_RATE),
                    retry_delay: Duration::from_secs(1),
                    tls: crate::net::tls(std::env::var("FLEET_CA_FILE").ok().as_deref()),
                })
            });
            let cfg = transport::Config::from_env(
                url,
                agent_run_id,
                crate::ci::context(|k| std::env::var(k).ok()),
                |k| std::env::var(k).ok(),
            );
            (
                Some(tx),
                Some(tokio::spawn(transport::run(
                    cfg,
                    Arc::clone(&shared),
                    rx,
                    clock,
                ))),
            )
        }
        None => (None, None),
    };
    let (devices_tx, devices) = mpsc::channel(16);
    tokio::spawn(crate::adb::track(crate::adb::server_addr(), devices_tx));
    let debug = debug_dir.map(PathBuf::from);
    Live {
        uploader: uploader.flatten(),
        debug: debug.clone(),
        session,
        tailer: debug
            .as_ref()
            .map(|d| LogTailer::new(d.join("maestro.log"))),
        scanner: debug.map(BundleScanner::new),
        report: report.map(PathBuf::from),
        started,
        pgid: group,
        stdout: Lines::default(),
        sampler: Sampler::new(
            Some(pid),
            std::env::var(SUPERVISOR_PID_ENV)
                .ok()
                .and_then(|v| v.parse().ok()),
        ),
        shared,
        journal,
        telemetry,
        transport,
        tseq: 0,
        dropped_bytes: 0,
        devices,
    }
}

/// Artifact uploads are on unless `FLEET_UPLOAD_ARTIFACTS` says `0`, `false`, `no` or `off`.
fn uploads_enabled() -> bool {
    !std::env::var("FLEET_UPLOAD_ARTIFACTS").is_ok_and(|v| {
        ["0", "false", "no", "off"].contains(&v.trim().to_ascii_lowercase().as_str())
    })
}

/// Queues the files the session found worth uploading, and journals the uploads that finished.
/// Never blocks and never fails: an upload problem is a journal entry and a log line (I2).
fn pump_uploads(l: &mut Live, clock: &impl Fn() -> ClockSample) {
    let candidates = l.session.take_uploads();
    let Some(up) = l.uploader.as_ref() else {
        return;
    };
    for c in candidates {
        let Some(file) = l.debug.as_ref().map(|d| d.join(&c.rel)) else {
            continue;
        };
        let label = l.session.label(&c.rel);
        let (content_type, source) = match c.class {
            ArtifactClass::FailureScreenshot => {
                let ext = file
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(str::to_ascii_lowercase);
                let ct = match ext.as_deref() {
                    Some("png") => "image/png",
                    Some("jpg" | "jpeg") => "image/jpeg",
                    Some("webp") => "image/webp",
                    _ => continue,
                };
                (ct, Source::File(file))
            }
            // The window of the device log around the failed step (issue #75).
            ArtifactClass::DeviceLogcat => {
                let Some(failure) = c.failure else { continue };
                (
                    "text/plain",
                    Source::Logcat(upload::Logcat {
                        path: file,
                        failure,
                        redactor: Arc::new(l.session.redactor().clone()),
                    }),
                )
            }
            // The flow's commands carry resolved `inputText`: redacted like a log.
            _ => (
                "application/json",
                Source::Text(file, Arc::new(l.session.redactor().clone())),
            ),
        };
        up.submit(upload::Item {
            class: c.class,
            path: label,
            content_type,
            flow: c.flow,
            step: c.step,
            source,
        });
    }
    journal_uploads(l, clock);
}

/// Records finished uploads as `artifact.uploaded` / `artifact.rejected`.
fn journal_uploads(l: &mut Live, clock: &impl Fn() -> ClockSample) {
    let Some(up) = l.uploader.as_ref() else {
        return;
    };
    for done in up.take_done() {
        record(&mut l.session, clock(), done);
    }
}

fn record(session: &mut Session, at: ClockSample, done: upload::Done) {
    let class = done.class.as_str().to_owned();
    let path = done.path;
    let body = match done.outcome {
        Outcome::Uploaded { artifact_id, size } => EventBody::ArtifactUploaded(ArtifactUploaded {
            class,
            path,
            artifact_id,
            size,
        }),
        Outcome::Rejected(reason) => {
            if reason == upload::NOT_REDACTABLE {
                session.warn(
                    at,
                    "artifact_not_uploaded",
                    &format!("{path} was not uploaded: it cannot be redacted reliably"),
                );
            }
            EventBody::ArtifactRejected(ArtifactRejected {
                class,
                path,
                reason,
            })
        }
        Outcome::Failed(why) => {
            tracing::warn!(%path, %why, "artifact upload failed; the run is unaffected");
            EventBody::ArtifactRejected(ArtifactRejected {
                class,
                path,
                reason: "upload_failed".into(),
            })
        }
    };
    session.artifact_result(at, done.flow, &body);
}

/// The run is over: queues the report and the log for upload. Their results are journaled by
/// [`await_uploads`].
fn submit_uploads(l: &mut Live, report: Option<&[u8]>, clock: &impl Fn() -> ClockSample) {
    pump_uploads(l, clock);
    let Some(up) = l.uploader.as_ref() else {
        return;
    };
    if let Some(bytes) = report {
        match upload::prepare_text(bytes, l.session.redactor()) {
            Ok(text) => up.submit(upload::Item {
                class: ArtifactClass::Junit,
                path: l
                    .report
                    .as_ref()
                    .and_then(|p| p.file_name())
                    .map_or_else(|| "report.xml".into(), |n| n.to_string_lossy().into_owned()),
                content_type: "application/xml",
                flow: None,
                step: None,
                source: Source::Bytes(text.into()),
            }),
            Err(reason) => record(
                &mut l.session,
                clock(),
                upload::Done {
                    class: ArtifactClass::Junit,
                    path: "report.xml".into(),
                    flow: None,
                    outcome: Outcome::Rejected(reason.into()),
                },
            ),
        }
    }
    if let Some(log) = l.debug.as_ref().map(|d| d.join("maestro.log"))
        && log.is_file()
        && let Some(up) = l.uploader.as_ref()
    {
        up.submit(upload::Item {
            class: ArtifactClass::MaestroLog,
            path: "maestro.log".into(),
            content_type: "text/plain",
            flow: None,
            step: None,
            source: Source::Text(log, Arc::new(l.session.redactor().clone())),
        });
    }
}

/// Waits up to `budget` for every upload, then journals how each one ended.
async fn await_uploads(l: &mut Live, budget: Duration, clock: &impl Fn() -> ClockSample) {
    let Some(up) = l.uploader.as_ref() else {
        return;
    };
    for done in up.finish(budget).await {
        record(&mut l.session, clock(), done);
    }
}

/// `FLEET_SERVER` as the agent WebSocket URL.
pub(crate) fn server_url() -> Option<String> {
    let base = std::env::var("FLEET_SERVER")
        .ok()
        .filter(|s| !s.trim().is_empty())?;
    let base = base.trim_end_matches('/');
    let ws = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        base.to_owned()
    };
    Some(if ws.contains("/api/") {
        ws
    } else {
        format!("{ws}/api/v1/agent/ws")
    })
}

fn watch(l: &mut Live, final_pass: bool, clock: &impl Fn() -> ClockSample) {
    if let Some(t) = l.tailer.as_mut() {
        for r in t.poll() {
            l.session.observe(clock(), Observation::Log(r));
        }
        if final_pass && let Some(r) = t.finish() {
            l.session.observe(clock(), Observation::Log(r));
        }
    }
    if let Some(s) = l.scanner.as_mut() {
        for o in s.scan(final_pass) {
            l.session.observe(clock(), o);
        }
    }
}

/// Moves new session events into the outbox and the local journal.
fn flush(l: &mut Live) {
    let events = l.session.drain();
    if events.is_empty() {
        return;
    }
    if let Some(j) = l.journal.as_mut() {
        for e in &events {
            j.append(e);
        }
    }
    if let Ok(mut o) = l.shared.outbox.lock() {
        o.push(events);
    }
    l.shared.wake.notify_one();
}

/// Slack for filesystems with coarse modification times.
const MTIME_TOLERANCE: Duration = Duration::from_secs(2);

/// What reading the JUnit report found.
#[derive(Debug, PartialEq, Eq)]
enum ReportRead {
    /// Written during this run.
    Fresh(Vec<u8>),
    /// There, but last modified before the run started.
    Stale,
    /// Not there, or unreadable.
    Missing,
}

/// Reads `path`, accepting it only if modified at or after `started` (less the tolerance).
fn read_report(path: &Path, started: SystemTime) -> ReportRead {
    let Ok(bytes) = std::fs::read(path) else {
        return ReportRead::Missing;
    };
    let modified = std::fs::metadata(path).and_then(|m| m.modified());
    match modified {
        Ok(m) if m + MTIME_TOLERANCE < started => ReportRead::Stale,
        // Unknown mtime: can't prove it stale, and the report parser still validates it.
        _ => ReportRead::Fresh(bytes),
    }
}

/// Appends the job summary to `$GITHUB_STEP_SUMMARY`, when the job has one. A file that cannot be
/// written is reported to the supervisor, which prints one notice; the run is unaffected (I1, I2).
fn write_job_summary(session: &Session) {
    let Some(path) = std::env::var_os("GITHUB_STEP_SUMMARY").filter(|p| !p.is_empty()) else {
        return;
    };
    let dashboard = server_url().map(|url| format!("{}/", crate::net::http_base(&url)));
    let text =
        crate::inspect::job_summary(&crate::inspect::flow_results(session), dashboard.as_deref());
    let written = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut file| file.write_all(text.as_bytes()));
    if let Err(e) = written {
        let _ = writeln!(
            io::stdout().lock(),
            "step-summary-failed {}: {e}",
            path.display()
        );
    }
}

/// Ends the run's session and flushes it. Returns the Maestro version its session log named.
async fn finalize(
    mut l: Live,
    (code, signal): (Option<i32>, Option<i32>),
    output_bytes_dropped: u64,
    clock: impl Fn() -> ClockSample,
) -> Option<String> {
    // Everything after Maestro exits (uploads, then the transport's flush) shares one budget, so
    // an unreachable server can't stretch the end of the job (I2).
    let started = Instant::now();
    let configured_budget = std::env::var("FLEET_FLUSH_TIMEOUT")
        .ok()
        .and_then(|s| s.parse().ok())
        .map_or(Duration::from_secs(20), Duration::from_secs);
    // A server that never answered the WebSocket upgrade gets a short budget. One that answered
    // gets the whole budget, however slowly it welcomes (a busy server is not an unreachable one).
    let flush_budget = |answered: bool| {
        if answered {
            configured_budget
        } else {
            configured_budget.min(Duration::from_secs(3))
        }
    };
    let upload_budget = std::env::var("FLEET_UPLOAD_TIMEOUT")
        .ok()
        .and_then(|s| s.parse().ok())
        .map_or(
            flush_budget(l.shared.connected.load(Ordering::SeqCst)) / 2,
            Duration::from_secs,
        );
    // Let Maestro's last writes land, then take a final look.
    tokio::time::sleep(POLL).await;
    watch(&mut l, true, &clock);
    // I3: a report left by an earlier run in the same workspace is not this run's evidence.
    let report = match l.report.as_deref().map(|p| (p, read_report(p, l.started))) {
        Some((_, ReportRead::Fresh(bytes))) => Some(bytes),
        Some((p, ReportRead::Stale)) => {
            l.session.warn(
                clock(),
                "report_stale",
                &format!(
                    "{} was last written before this run started; ignored",
                    p.display()
                ),
            );
            None
        }
        Some((_, ReportRead::Missing)) | None => None,
    };
    submit_uploads(&mut l, report.as_deref(), &clock);
    l.session.observe(
        clock(),
        Observation::Exited {
            code,
            signal,
            report: match (&report, &l.report) {
                (Some(_), _) => ReportState::Present,
                (None, Some(_)) => ReportState::Absent,
                (None, None) => ReportState::NotRequested,
            },
        },
    );
    l.session.observe(clock(), Observation::Report(report));
    // The journal goes out first: its tail is sent while the uploads run, so a slow upload can't
    // hold back the flows' results (uploads can't be replayed from the spool, events can).
    flush(&mut l);
    // Upload results follow the tail, and the run end comes last: the server refuses events
    // after it.
    await_uploads(&mut l, upload_budget, &clock).await;
    // Gap sums are a subset of the authoritative total: never under-report or double-count.
    l.session
        .end(clock(), l.dropped_bytes.max(output_bytes_dropped));
    write_job_summary(&l.session);
    flush(&mut l);
    if let Some(t) = l.journal.as_ref().and_then(Writer::truncation) {
        // Written before `finalized`, so the supervisor reads it with its end-of-run warnings
        // (issue #37). Last, because `run.ended` can be the event that crosses the cap. The reason
        // comes last and may hold spaces (`io: <kind>`).
        let _ = writeln!(
            io::stdout().lock(),
            "journal-truncated {} {}",
            t.first_missing_seq,
            t.reason
        );
    }
    let maestro = l.session.maestro().map(|b| b.version.clone());
    if let Ok(mut o) = l.shared.outbox.lock() {
        // Nothing more is coming: a gap for dropped events is sequenced now, before the flush.
        o.seal();
        if o.dropped() > 0 {
            tracing::warn!(
                dropped = o.dropped(),
                "events were dropped because the outbox was full; a journal.gap records them"
            );
        }
    }
    l.shared.finishing.store(true, Ordering::SeqCst);
    l.shared.wake.notify_one();
    if let Some(handle) = l.transport.take() {
        // What is left of the shared budget, but never less than a few seconds.
        let budget = flush_budget(l.shared.connected.load(Ordering::SeqCst))
            .saturating_sub(started.elapsed())
            .max(Duration::from_secs(3));
        match tokio::time::timeout(budget, handle).await {
            Ok(Ok(end)) => tracing::info!(?end, "transport finished"),
            Ok(Err(e)) => tracing::warn!(error = %e, "transport task failed"),
            Err(_) => tracing::warn!("transport did not flush in time; the spool keeps the events"),
        }
    }
    maestro
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report_with_age(dir: &Path, age: Duration) -> PathBuf {
        let p = dir.join("report.xml");
        let f = File::create(&p).unwrap();
        f.set_modified(SystemTime::now() - age).unwrap();
        p
    }

    #[test]
    fn a_report_from_before_the_run_is_stale_and_a_fresh_one_is_used() {
        let dir = std::env::temp_dir().join(format!("fleet-report-{}", Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let started = SystemTime::now() - Duration::from_secs(60);
        let p = report_with_age(&dir, Duration::from_secs(3600));
        assert_eq!(read_report(&p, started), ReportRead::Stale);
        // Within the tolerance for coarse timestamps.
        let p = report_with_age(&dir, Duration::from_secs(61));
        assert!(matches!(read_report(&p, started), ReportRead::Fresh(_)));
        let p = report_with_age(&dir, Duration::ZERO);
        assert!(matches!(read_report(&p, started), ReportRead::Fresh(_)));
        assert_eq!(
            read_report(&dir.join("none.xml"), started),
            ReportRead::Missing
        );
    }
}

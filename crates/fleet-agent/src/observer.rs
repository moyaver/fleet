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
    ffi::OsStr,
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
    frame::{CiContext, DeviceSample, Telemetry, Unavailable},
    journal::ClockSample,
};
use time::OffsetDateTime;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::{
    bundle,
    device::{Probes, Target},
    emulator::Emulators,
    ipc::{Control, Frame, Stream},
    journal::Writer,
    layout,
    metrics::{Pressure, SAMPLE_TICK, Sampler, Throttle},
    outbox::{Budget, Outbox},
    session::{Candidate, DrivenDevice, Observation, Session},
    transport::{self, Shared},
    upload::{self, Outcome, Source, Uploader},
    watch::{self, BundleScanner, Located, LogTailer},
};

/// Fault injection for tests (research 02 §14): `observer_panic`, `observer_abort`,
/// `observer_hang`, `observer_exit`.
const FAULT_ENV: &str = "FLEET_FAULT";
/// Environment variable carrying the supervisor's PID, set by the supervisor when it spawns us.
pub(crate) const SUPERVISOR_PID_ENV: &str = "FLEET_SUPERVISOR_PID";
/// Where the supervisor asks the observer to write its own log.
pub(crate) const LOG_ENV: &str = "FLEET_OBSERVER_LOG";

const POLL: Duration = Duration::from_millis(250);
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

/// Where Maestro writes this run's session log and flow bundles, for the command that runs. Each
/// is looked up until Maestro has created it (see [`Located`]).
struct Layout {
    log: Located,
    bundles: Located,
}

/// Everything that exists once Maestro has started.
struct Live {
    session: Session,
    /// The command's process: the Maestro JVM (the launcher execs it), which holds its session log open.
    pid: u32,
    /// Where Maestro writes; `None` for a command that is not `maestro test`.
    layout: Option<Layout>,
    /// The session log, once its directory is known.
    tailer: Option<LogTailer>,
    log_file: Option<PathBuf>,
    scanner: Option<BundleScanner>,
    report: Option<PathBuf>,
    /// The HTML report the command asks for (`--format html` or `html-detailed`): its `--output`,
    /// or Maestro's default `report.html` in the working directory (issue #43).
    html: Option<PathBuf>,
    /// When Maestro was spawned: a report older than this (less [`MTIME_TOLERANCE`]) is not ours.
    started: SystemTime,
    pgid: Option<u32>,
    stdout: Lines,
    sampler: Sampler,
    /// When host and process metrics are taken, and whether the host is under pressure (PLAN §6.5).
    throttle: Throttle,
    shared: Arc<Shared>,
    journal: Option<Writer>,
    telemetry: Option<mpsc::Sender<Telemetry>>,
    transport: Option<tokio::task::JoinHandle<transport::End>>,
    tseq: u64,
    dropped_bytes: u64,
    devices: mpsc::Receiver<Vec<(String, fleet_protocol::event::AdbState)>>,
    /// Device probes for the devices Maestro drives (PLAN §6.5); used only when telemetry is sent.
    probes: Probes,
    /// The emulator processes behind those devices (PLAN §6.5); rescanned when adb's list changes.
    emulators: Emulators,
    /// Device samples waiting for the next telemetry frame.
    pending_devices: Vec<DeviceSample>,
    /// Uploads artifacts (PLAN §6.6); `None` offline or with `FLEET_UPLOAD_ARTIFACTS=off`.
    uploader: Option<Uploader>,
    /// Maestro's flow bundles directory: where the files to upload are, once it is known.
    bundles: Option<PathBuf>,
    /// The run directory: where the journal is, and where an offline bundle goes.
    run_dir: Option<PathBuf>,
    /// The artifacts an offline run would have uploaded, for its bundle (issue #83). `None` when
    /// the run has a server, or no run directory.
    bundle: Option<Vec<upload::Item>>,
    /// This agent run, and its CI identity: what the bundle's manifest records.
    agent_run_id: Uuid,
    ci: CiContext,
}

/// Observes the run until it ends, and returns the Maestro version its session log named, if any.
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
    let mut sample = tokio::time::interval(SAMPLE_TICK);
    loop {
        tokio::select! {
            frame = frames.recv() => match frame {
                Some(Frame::Control(Control::ChildStarted { pid, pgid, argv, started_at_ms, report, run_dir, plan, evidence, journal_max_bytes })) => {
                    let mut l = start(&argv, pid, pgid, report, run_dir.as_deref().map(Path::new), evidence.map(|e| *e), UNIX_EPOCH + Duration::from_millis(started_at_ms), clock(), clock, journal_max_bytes);
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
                    l.emulators.rescan();
                    flush(l);
                }
            }
            _ = sample.tick() => {
                if let Some(l) = live.as_mut() {
                    sample_tick(l, Instant::now(), &clock);
                }
            }
        }
    }
}

/// One sampling tick of a live run: the host and process sample when one is due, the probes
/// started and finished since the last tick, and the device samples queued with their emulator
/// readings. The loop runs it on every sampling interval.
fn sample_tick(l: &mut Live, now: Instant, clock: &impl Fn() -> ClockSample) {
    // Host and process metrics are taken when due: every 5 s, or every 15 s while the
    // host is under pressure, unless the server's welcome set other intervals (PLAN §6.5).
    // Device probes keep their own interval.
    let host = if l.throttle.due(now, l.shared.cadence()) {
        let s = l.sampler.sample(clock());
        l.throttle.observe(now, Pressure::from(&s));
        l.tseq += 1;
        l.emulators.refresh();
        Some(s)
    } else {
        None
    };
    let driven = l.session.driven_devices();
    let finished = l.probes.take();
    l.pending_devices.extend(
        finished
            .into_iter()
            .map(|d| with_emulator(d, &driven, &l.emulators)),
    );
    if l.telemetry.is_some() {
        let at = clock();
        let targets: Vec<Target> = driven
            .iter()
            .map(|d| Target {
                adb_state: l.session.adb_state(&d.serial),
                serial: d.serial.clone(),
                device_session_id: d.device_session_id,
                shard: d.shard,
            })
            .collect();
        l.probes
            .start_due(now, &at, &targets, !l.throttle.throttled());
        // A flow that failed on a device since the last tick: read its activity now.
        let failed: Vec<Target> = l
            .session
            .take_failed_devices()
            .into_iter()
            .map(|d| Target {
                adb_state: l.session.adb_state(&d.serial),
                serial: d.serial,
                device_session_id: d.device_session_id,
                shard: d.shard,
            })
            .collect();
        l.probes.request_activity(&at, &failed);
    }
    // Device samples wait for the next host sample's frame.
    if let Some(s) = host {
        let devices = std::mem::take(&mut l.pending_devices);
        if let Some(t) = &l.telemetry {
            let _ = t.try_send(Telemetry {
                tseq_from: l.tseq,
                tseq_to: l.tseq,
                dropped_since_last: 0,
                samples: vec![s],
                devices,
            });
        }
    }
}

/// Where Maestro writes this run's files, for the command as given. Only a `maestro test` the agent
/// recognised (`recognised`) has one. Maestro's own environment decides its state directory and
/// `user.home` (see [`crate::layout`]), so they are read from the same variables.
fn watched_layout(argv: &[String], recognised: bool, started: SystemTime) -> Option<Layout> {
    if !recognised {
        return None;
    }
    let home = layout::user_home_here();
    let state = fleet_maestro::invocation::state_tests_dir(
        std::env::var_os("XDG_STATE_HOME").as_deref(),
        home.as_deref(),
    );
    let cwd = std::env::current_dir().ok();
    let workspace = layout::workspace_test_dir(argv, cwd.as_deref());
    let layout = fleet_maestro::invocation::output_layout(
        &fleet_maestro::invocation::parse(argv),
        state.as_deref(),
        home.as_deref(),
        workspace.as_deref(),
    )?;
    Some(Layout {
        log: Located::new(layout.log, started),
        bundles: Located::new(layout.bundles, started),
    })
}

/// The flows of the workspace a `maestro test` run reads: the directory it names, else the one its
/// `--config` lies in, else the current directory. A run that names flow files alone reads no
/// workspace, so its catalog is empty (issue #85).
fn catalog_of(argv: &[String], cwd: Option<&Path>) -> fleet_protocol::frame::FlowCatalog {
    let inv = fleet_maestro::invocation::parse(argv);
    if inv.test_index.is_none() {
        return fleet_protocol::frame::FlowCatalog::default();
    }
    let resolve = |p: &str| match cwd {
        Some(cwd) if Path::new(p).is_relative() => cwd.join(p),
        _ => PathBuf::from(p),
    };
    let workspace = match &inv.config {
        Some(config) => resolve(config).parent().map(Path::to_path_buf),
        None if inv.positionals.is_empty() => cwd.map(Path::to_path_buf),
        None => inv
            .positionals
            .iter()
            .map(|p| resolve(p))
            .find(|p| p.is_dir()),
    };
    let Some(workspace) = workspace else {
        return fleet_protocol::frame::FlowCatalog::default();
    };
    let repo_root = std::env::var_os("GITHUB_WORKSPACE")
        .map(PathBuf::from)
        .filter(|root| workspace.starts_with(root))
        .unwrap_or_else(|| crate::catalog::repo_root_of(&workspace));
    crate::catalog::discover(&workspace, &repo_root)
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
#[expect(
    clippy::too_many_lines,
    reason = "one straight-line assembly of the run's state: the sources, transport and devices"
)]
fn start(
    argv: &[String],
    pid: u32,
    group: Option<u32>,
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
    let layout = watched_layout(argv, evidence.is_some(), started);
    let cwd = std::env::current_dir().ok();
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
        run_dir.and_then(|d| Writer::create(&d.join(bundle::JOURNAL), journal_max_bytes).ok());
    let ci = crate::ci::context(|k| std::env::var(k).ok());
    let mut uploader = None;
    let (telemetry, transport) = match server_url() {
        Some(url) => {
            let (tx, rx) = mpsc::channel(64);
            // No credential, no upload: the uploader would only wait for one (fork pull requests).
            let can_upload =
                uploads_enabled() && crate::net::has_credential(|k| std::env::var(k).ok());
            uploader = can_upload.then(|| {
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
            let mut cfg = transport::Config::from_env(url, agent_run_id, ci.clone(), |k| {
                std::env::var(k).ok()
            });
            cfg.catalog = catalog_of(argv, cwd.as_deref());
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
    let mut emulators = Emulators::new();
    emulators.rescan();
    Live {
        uploader: uploader.flatten(),
        bundles: None,
        // Offline (no server) with a run directory: the run keeps a bundle (issue #83). Artifacts
        // are left out when `FLEET_UPLOAD_ARTIFACTS` turns them off, as they are for a live run.
        bundle: (server_url().is_none() && run_dir.is_some() && uploads_enabled()).then(Vec::new),
        run_dir: run_dir.map(Path::to_path_buf),
        agent_run_id,
        ci,
        session,
        pid,
        layout,
        tailer: None,
        log_file: None,
        scanner: None,
        report: report.map(PathBuf::from),
        html: fleet_maestro::invocation::requested_html_report(&fleet_maestro::invocation::parse(
            argv,
        ))
        .map(PathBuf::from),
        started,
        pgid: group,
        stdout: Lines::default(),
        sampler: Sampler::new(
            Some(pid),
            std::env::var(SUPERVISOR_PID_ENV)
                .ok()
                .and_then(|v| v.parse().ok()),
        ),
        throttle: Throttle::default(),
        shared,
        journal,
        telemetry,
        transport,
        tseq: 0,
        dropped_bytes: 0,
        devices,
        probes: Probes::new(crate::adb::server_addr()),
        emulators,
        pending_devices: Vec::new(),
    }
}

/// Adds the emulator process's readings to a device sample: CPU, resident memory and the AVD name,
/// from the `qemu-system-*` process that maps to the sample's serial (PLAN §6.5). Every emulator
/// reading the sample lacks has its reason: a physical device is `not_an_emulator`, and an emulator
/// with no process (or none readable) is `no_emulator_process`.
fn with_emulator(
    mut sample: DeviceSample,
    driven: &[DrivenDevice],
    emulators: &Emulators,
) -> DeviceSample {
    let Some(serial) = driven
        .iter()
        .find(|d| d.device_session_id == sample.device_session_id)
        .map(|d| d.serial.as_str())
    else {
        // The session is no longer driven, so no process can be named for it.
        sample.unavailable.emu_cpu_pct = Some(Unavailable::NoEmulatorProcess);
        sample.unavailable.emu_rss_bytes = Some(Unavailable::NoEmulatorProcess);
        sample.unavailable.avd_name = Some(Unavailable::NoEmulatorProcess);
        return sample;
    };
    if !serial.starts_with("emulator-") {
        sample.unavailable.emu_cpu_pct = Some(Unavailable::NotAnEmulator);
        sample.unavailable.emu_rss_bytes = Some(Unavailable::NotAnEmulator);
        sample.unavailable.avd_name = Some(Unavailable::NotAnEmulator);
        return sample;
    }
    let Some(reading) = emulators.reading(serial) else {
        sample.unavailable.emu_cpu_pct = Some(Unavailable::NoEmulatorProcess);
        sample.unavailable.emu_rss_bytes = Some(Unavailable::NoEmulatorProcess);
        sample.unavailable.avd_name = Some(Unavailable::NoEmulatorProcess);
        return sample;
    };
    sample.emu_cpu_pct = reading.cpu_pct;
    sample.emu_rss_bytes = reading.rss_bytes;
    sample.avd_name = reading.avd;
    sample.unavailable.emu_cpu_pct = reading
        .cpu_pct
        .is_none()
        .then_some(Unavailable::NoEmulatorProcess);
    sample.unavailable.emu_rss_bytes = reading
        .rss_bytes
        .is_none()
        .then_some(Unavailable::NoEmulatorProcess);
    sample.unavailable.avd_name = sample.avd_name.is_none().then_some(Unavailable::NoAvdName);
    sample
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
    for c in l.session.take_uploads() {
        if let Some(item) = candidate_item(&l.session, l.bundles.as_deref(), &c) {
            enqueue(l, item);
        }
    }
    journal_uploads(l, clock);
}

/// The upload for a file the session found, or `None` when it is not one to upload. The bytes are
/// read and redacted when the upload starts, or when the bundle is written (`upload::materialize`).
pub(crate) fn candidate_item(
    session: &Session,
    bundles: Option<&Path>,
    c: &Candidate,
) -> Option<upload::Item> {
    let file = bundles?.join(&c.rel);
    let path = session.label(&c.rel);
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
                _ => return None,
            };
            (ct, Source::File(file))
        }
        // The window of the device log around the failed step (issue #75).
        ArtifactClass::DeviceLogcat => {
            let failure = c.failure?;
            (
                "text/plain",
                Source::Logcat(upload::Logcat {
                    path: file,
                    failure,
                    redactor: Arc::new(session.redactor().clone()),
                }),
            )
        }
        // The flow's commands carry resolved `inputText`: redacted like a log.
        _ => (
            "application/json",
            Source::Text(file, Arc::new(session.redactor().clone())),
        ),
    };
    Some(upload::Item {
        class: c.class,
        path,
        content_type,
        flow: c.flow,
        step: c.step,
        source,
    })
}

/// Hands an artifact to the uploader when the run has a server, or keeps it for the offline
/// bundle (issue #83). Without either, the artifact is not kept, as before.
fn enqueue(l: &mut Live, item: upload::Item) {
    if let Some(up) = l.uploader.as_ref() {
        up.submit(item);
    } else if let Some(bundle) = l.bundle.as_mut() {
        bundle.push(item);
    }
}

/// Journals the uploads that finished.
fn journal_uploads(l: &mut Live, clock: &impl Fn() -> ClockSample) {
    let Some(up) = l.uploader.as_ref() else {
        return;
    };
    for done in up.take_done() {
        record(&mut l.session, clock(), done);
    }
}

/// The event an upload's outcome is journaled as: `artifact.uploaded` or `artifact.rejected`.
pub(crate) fn artifact_body(done: upload::Done) -> EventBody {
    let class = done.class.as_str().to_owned();
    let path = done.path;
    match done.outcome {
        Outcome::Uploaded { artifact_id, size } => EventBody::ArtifactUploaded(ArtifactUploaded {
            class,
            path,
            artifact_id,
            size,
        }),
        Outcome::Rejected(reason) => EventBody::ArtifactRejected(ArtifactRejected {
            class,
            path,
            reason,
        }),
        Outcome::Failed(why) => {
            tracing::warn!(%path, %why, "artifact upload failed; the run is unaffected");
            EventBody::ArtifactRejected(ArtifactRejected {
                class,
                path,
                reason: "upload_failed".into(),
            })
        }
    }
}

fn record(session: &mut Session, at: ClockSample, done: upload::Done) {
    let flow = done.flow;
    if done.outcome == Outcome::Rejected(upload::NOT_REDACTABLE.into()) {
        session.warn(
            at,
            "artifact_not_uploaded",
            &format!(
                "{} was not uploaded: it cannot be redacted reliably",
                done.path
            ),
        );
    }
    let body = artifact_body(done);
    session.artifact_result(at, flow, &body);
}

/// The run is over: queues the report and the log for upload (or for the bundle). Their results
/// are journaled by [`await_uploads`].
fn submit_uploads(l: &mut Live, report: Option<&[u8]>, clock: &impl Fn() -> ClockSample) {
    pump_uploads(l, clock);
    if let Some(bytes) = report {
        match upload::prepare_text(bytes, l.session.redactor()) {
            Ok(text) => {
                let path = l
                    .report
                    .as_ref()
                    .and_then(|p| p.file_name())
                    .map_or_else(|| "report.xml".into(), |n| n.to_string_lossy().into_owned());
                enqueue(
                    l,
                    upload::Item {
                        class: ArtifactClass::Junit,
                        path,
                        content_type: "application/xml",
                        flow: None,
                        step: None,
                        source: Source::Bytes(text.into()),
                    },
                );
            }
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
    if let Some(path) = l.html.clone() {
        match written_during(&path, l.started) {
            Some(true) => {
                let label = path.file_name().map_or_else(
                    || "report.html".into(),
                    |n| n.to_string_lossy().into_owned(),
                );
                // Text, so it is redacted like a log (I6); one too big to redact is refused.
                let redactor = Arc::new(l.session.redactor().clone());
                enqueue(
                    l,
                    upload::Item {
                        class: ArtifactClass::HtmlReport,
                        path: label,
                        content_type: "text/html",
                        flow: None,
                        step: None,
                        source: Source::Text(path, redactor),
                    },
                );
            }
            Some(false) => l.session.warn(
                clock(),
                "report_stale",
                &format!(
                    "{} was last written before this run started; ignored",
                    path.display()
                ),
            ),
            None => {}
        }
    }
    if let Some(log) = l.log_file.clone()
        && log.is_file()
    {
        let redactor = Arc::new(l.session.redactor().clone());
        enqueue(
            l,
            upload::Item {
                class: ArtifactClass::MaestroLog,
                path: "maestro.log".into(),
                content_type: "text/plain",
                flow: None,
                step: None,
                source: Source::Text(log, redactor),
            },
        );
    }
}

/// Writes the offline bundle, when the run is offline and has a run directory.
async fn write_offline_bundle(l: &mut Live) {
    if let (Some(items), Some(run_dir)) = (l.bundle.take(), l.run_dir.clone()) {
        write_bundle(items, &run_dir, l.agent_run_id, l.ci.clone()).await;
    }
}

/// Writes the offline bundle and reports the outcome on stdout, where the supervisor reads it. A
/// failure leaves the run as it is (I2).
async fn write_bundle(items: Vec<upload::Item>, run_dir: &Path, agent_run_id: Uuid, ci: CiContext) {
    let outcome = bundle::write(
        run_dir,
        &run_dir.join(bundle::JOURNAL),
        items,
        agent_run_id,
        ci,
    )
    .await;
    let line = match outcome {
        Ok(written) => format!("bundle-written {}", written.path.display()),
        Err(why) => format!("bundle-failed {why}"),
    };
    let _ = writeln!(io::stdout().lock(), "{line}");
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
    attach(l, Instant::now());
    for o in watch::observations(l.tailer.as_mut(), l.scanner.as_mut(), final_pass) {
        l.session.observe(clock(), o);
    }
}

/// Starts following the session log and the flow bundles as soon as Maestro has created their
/// directories. Until then there is nothing to read, and the next poll looks again.
fn attach(l: &mut Live, now: Instant) {
    let pid = l.pid;
    let Some(layout) = l.layout.as_mut() else {
        return;
    };
    if l.tailer.is_none()
        && let Some(dir) = layout.log.log_dir(Some(pid), now)
    {
        let file = dir.join("maestro.log");
        l.tailer = Some(LogTailer::new(file.clone()));
        l.log_file = Some(file);
    }
    // A stamped bundles directory has the session log's directory name.
    let stamp = l
        .log_file
        .as_deref()
        .and_then(Path::parent)
        .and_then(Path::file_name)
        .map(OsStr::to_owned);
    if l.scanner.is_none()
        && let Some(dir) = layout.bundles.bundles_dir(stamp.as_deref())
    {
        let dir = dir.to_path_buf();
        l.scanner = Some(BundleScanner::new(dir.clone(), l.started));
        l.bundles = Some(dir);
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

/// Whether `path` was written during this run (I3: a file an earlier run left is not this run's
/// evidence). `None` when there is no such file.
fn written_during(path: &Path, started: SystemTime) -> Option<bool> {
    let modified = std::fs::metadata(path).ok()?.modified();
    Some(match modified {
        Ok(m) => m + MTIME_TOLERANCE >= started,
        // Unknown mtime: can't prove it stale.
        Err(_) => true,
    })
}

/// Appends the job summary to `$GITHUB_STEP_SUMMARY`, when the job has one. A file that cannot be
/// written is reported to the supervisor, which prints one notice; the run is unaffected (I1, I2).
fn write_job_summary(session: &Session) {
    let Some(path) = std::env::var_os("GITHUB_STEP_SUMMARY").filter(|p| !p.is_empty()) else {
        return;
    };
    let dashboard = server_url().map(|url| format!("{}/", crate::net::http_base(&url)));
    // Markdown, but a runner still reads a line that starts with `::` in a log (T13).
    let summary =
        crate::inspect::job_summary(&crate::inspect::flow_results(session), dashboard.as_deref());
    let text = crate::ci::neutralize_workflow_commands(&summary);
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
    // Device samples still waiting for a host frame (the host is throttled) go out now, in a frame
    // without host samples; `tseq_to` repeats the last host sample, so the server's ack is unchanged.
    let devices = std::mem::take(&mut l.pending_devices);
    if let (false, Some(t)) = (devices.is_empty(), &l.telemetry) {
        let _ = t.try_send(Telemetry {
            tseq_from: l.tseq,
            tseq_to: l.tseq,
            dropped_since_last: 0,
            samples: Vec::new(),
            devices,
        });
    }
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
    // An offline run keeps what it would have sent (issue #83). Written after the last flush, so
    // the journal in the bundle holds every event, the run's end included.
    write_offline_bundle(&mut l).await;
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
    use fleet_protocol::frame::DeviceUnavailable;

    use super::*;
    use crate::device::{API_LEVEL, PROBE};

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

    #[test]
    fn an_html_report_is_found_and_counts_only_if_this_run_wrote_it() {
        let dir = std::env::temp_dir().join(format!("fleet-html-{}", Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let started = SystemTime::now() - Duration::from_secs(60);
        let p = dir.join("r.html");
        assert_eq!(
            written_during(&p, started),
            None,
            "not written: nothing to upload"
        );
        File::create(&p)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(3600))
            .unwrap();
        assert_eq!(
            written_during(&p, started),
            Some(false),
            "an earlier run's file"
        );
        File::create(&p)
            .unwrap()
            .set_modified(SystemTime::now())
            .unwrap();
        assert_eq!(written_during(&p, started), Some(true));
    }

    /// A device sample of `session`, with its probe readings set and every emulator reading unknown.
    fn device_sample(session: Uuid) -> DeviceSample {
        DeviceSample {
            model: None,
            os_version: None,
            at: ClockSample {
                wall: OffsetDateTime::UNIX_EPOCH,
                mono_ms: 0,
            },
            device_session_id: session,
            adb_state: Some(fleet_protocol::event::AdbState::Device),
            boot_completed: Some(true),
            guest_cpu_pct: None,
            guest_mem_available_bytes: None,
            emu_cpu_pct: None,
            emu_rss_bytes: None,
            avd_name: None,
            api_level: Some(37),
            foreground_activity: None,
            shard: Some(1),
            unavailable: DeviceUnavailable::default(),
        }
    }

    /// The sampling tick queues an emulator's device sample with its process's readings: the probe
    /// reads the device over adb, `with_emulator` adds the qemu process's CPU, memory and AVD name,
    /// and the next host sample's frame carries the sample to the server.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_sampling_tick_sends_an_emulators_sample_with_its_process_readings() {
        const FIRST: &[u8] = include_bytes!("../../../fixtures/adb/android-37.1/probe.raw");
        const API: &[u8] = include_bytes!("../../../fixtures/adb/android-37.1/api-level.raw");
        fn clock() -> ClockSample {
            ClockSample {
                wall: OffsetDateTime::UNIX_EPOCH,
                mono_ms: 0,
            }
        }
        let _emulator = crate::emulator::fake::qemu(5593, Some("Probe_Device"));
        let (addr, _) = crate::adb::fake::devices(
            "emulator-5593",
            vec![(PROBE, &FIRST[4..]), (API_LEVEL, &API[4..])],
        )
        .await;
        let mut l = start(
            &["maestro".into(), "test".into()],
            std::process::id(),
            None,
            None,
            None,
            None,
            SystemTime::now(),
            clock(),
            clock,
            1 << 20,
        );
        l.probes = Probes::with_timeout(addr, Duration::from_secs(5));
        let (tx, mut frames) = mpsc::channel(8);
        l.telemetry = Some(tx);
        // Maestro selects the emulator for its shard, so the session drives it.
        l.session.observe(
            clock(),
            Observation::Log(fleet_maestro::log::Record {
                unix_ms: None,
                level: "INFO".into(),
                logger: "maestro.cli.command.TestCommand".into(),
                message: "[shard 1] Selected device emulator-5593 using port 5593 with execution plan ExecutionPlan(flowsToRun=[/a/pass.yaml], sequence=FlowSequence(flows=[], continueOnFailure=true))".into(),
                continuation: Vec::new(),
            }),
        );

        let now = Instant::now();
        // The first tick starts the probe; its host sample goes out with no device samples yet.
        sample_tick(&mut l, now, &clock);
        l.probes.finished_one().await;
        // The next tick takes the finished probe and sends it with the host sample.
        sample_tick(&mut l, now + crate::metrics::SAMPLE, &clock);

        assert!(frames.try_recv().is_ok_and(|f| f.devices.is_empty()));
        let frame = frames.try_recv().expect("the second host sample's frame");
        let [sample] = frame.devices.as_slice() else {
            panic!("one device sample, got {:?}", frame.devices);
        };
        assert_eq!(sample.avd_name.as_deref(), Some("Probe_Device"));
        assert!(
            sample.emu_cpu_pct.is_some(),
            "the emulator process was read"
        );
        assert!(sample.emu_rss_bytes.is_some_and(|b| b > 0));
        assert_eq!(
            (
                sample.unavailable.emu_cpu_pct,
                sample.unavailable.emu_rss_bytes,
                sample.unavailable.avd_name,
            ),
            (None, None, None),
            "every emulator reading is set, so none has a reason"
        );
    }

    /// An emulator serial gets its process's CPU, memory and AVD name, and a physical device gets
    /// none of them: each unknown emulator reading says why.
    #[cfg(unix)]
    #[test]
    fn with_emulator_adds_the_process_readings_to_an_emulator_serial_only() {
        let _emulator = crate::emulator::fake::qemu(5599, Some("Test_Device"));
        let mut emulators = Emulators::new();
        emulators.rescan();
        emulators.refresh();
        let (emu, physical) = (Uuid::from_u128(1), Uuid::from_u128(2));
        let driven = [
            DrivenDevice {
                serial: "emulator-5599".into(),
                device_session_id: emu,
                shard: 1,
            },
            DrivenDevice {
                serial: "R58M12345".into(),
                device_session_id: physical,
                shard: 2,
            },
        ];

        let on_emulator = with_emulator(device_sample(emu), &driven, &emulators);
        assert_eq!(on_emulator.avd_name.as_deref(), Some("Test_Device"));
        assert!(on_emulator.emu_rss_bytes.is_some_and(|b| b > 0));
        assert!(on_emulator.emu_cpu_pct.is_some(), "the process was read");
        assert_eq!(
            (
                on_emulator.unavailable.emu_cpu_pct,
                on_emulator.unavailable.emu_rss_bytes,
                on_emulator.unavailable.avd_name,
            ),
            (None, None, None),
            "a reading that is set has no reason"
        );

        let on_device = with_emulator(device_sample(physical), &driven, &emulators);
        assert_eq!(
            (
                on_device.emu_cpu_pct,
                on_device.emu_rss_bytes,
                on_device.avd_name.clone()
            ),
            (None, None, None),
            "a physical device has no emulator process"
        );
        assert_eq!(
            (
                on_device.unavailable.emu_cpu_pct,
                on_device.unavailable.emu_rss_bytes,
                on_device.unavailable.avd_name,
                on_device.boot_completed,
                on_device.shard,
            ),
            (
                Some(Unavailable::NotAnEmulator),
                Some(Unavailable::NotAnEmulator),
                Some(Unavailable::NotAnEmulator),
                Some(true),
                Some(1),
            ),
            "not an emulator, and the probe's own readings are kept"
        );
    }

    /// An emulator serial that no process maps to says so; one whose process names no AVD says that.
    #[cfg(unix)]
    #[test]
    fn with_emulator_says_why_an_emulator_reading_is_missing() {
        let no_process = Emulators::new();
        let session = Uuid::from_u128(3);
        let driven = [DrivenDevice {
            serial: "emulator-5598".into(),
            device_session_id: session,
            shard: 1,
        }];
        let s = with_emulator(device_sample(session), &driven, &no_process);
        assert_eq!(
            (s.emu_cpu_pct, s.emu_rss_bytes, s.avd_name.clone()),
            (None, None, None)
        );
        assert_eq!(
            (
                s.unavailable.emu_cpu_pct,
                s.unavailable.emu_rss_bytes,
                s.unavailable.avd_name,
            ),
            (
                Some(Unavailable::NoEmulatorProcess),
                Some(Unavailable::NoEmulatorProcess),
                Some(Unavailable::NoEmulatorProcess),
            ),
        );

        let _no_avd = crate::emulator::fake::qemu(5597, None);
        let mut emulators = Emulators::new();
        emulators.rescan();
        emulators.refresh();
        let driven = [DrivenDevice {
            serial: "emulator-5597".into(),
            device_session_id: session,
            shard: 1,
        }];
        let s = with_emulator(device_sample(session), &driven, &emulators);
        assert!(s.emu_rss_bytes.is_some(), "the process is found");
        assert_eq!(
            (s.avd_name, s.unavailable.avd_name),
            (None, Some(Unavailable::NoAvdName)),
            "its command line names no AVD"
        );
    }

    #[test]
    fn a_run_reads_the_workspace_its_command_names_and_no_catalog_for_a_flow_file() {
        let dir =
            std::env::temp_dir().join(format!("fleet-observer-catalog-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(dir.join("flows")).unwrap();
        std::fs::write(dir.join("config.yaml"), "flows:\n  - flows/*.yaml\n").unwrap();
        std::fs::write(
            dir.join("flows/login.yaml"),
            "name: login\ntags:\n  - smoke\n---\n- back\n",
        )
        .unwrap();
        let argv =
            |args: &[&str]| -> Vec<String> { args.iter().map(|a| (*a).to_owned()).collect() };
        let dir_arg = dir.to_string_lossy().into_owned();

        let by_dir = catalog_of(&argv(&["maestro", "test", &dir_arg]), None);
        assert_eq!(by_dir.flows.len(), 1, "{by_dir:?}");
        assert_eq!(by_dir.flows[0].tags, ["smoke"]);

        let file_arg = dir.join("flows/login.yaml").to_string_lossy().into_owned();
        assert!(
            catalog_of(&argv(&["maestro", "test", &file_arg]), None).is_empty(),
            "a flow file names no workspace"
        );
        assert!(
            catalog_of(&argv(&["bash", "-c", "true"]), None).is_empty(),
            "not a maestro test"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

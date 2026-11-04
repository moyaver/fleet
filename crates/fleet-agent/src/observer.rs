// SPDX-License-Identifier: AGPL-3.0-only
//! The observer process (`fleet-agent __observe`; research 02 §15).
//!
//! It turns the supervisor's frames, the session `maestro.log` and the flow bundles into journal
//! events, keeps them in the outbox (spooled to disk and written to `journal.jsonl`), streams
//! them to the server if one is configured, and samples metrics. Everything here is best-effort:
//! the supervisor never depends on it, and it finishes within a bounded time.
//!
//! Configuration (environment): `FLEET_SERVER` (`ws(s)://` or `http(s)://` base URL; unset =
//! offline), `FLEET_TOKEN`, `FLEET_FLUSH_TIMEOUT` (seconds, default 20), plus the CI variables in
//! [`crate::ci`].

use std::{
    fs::File,
    io::{self, BufWriter, Write},
    path::{Path, PathBuf},
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};

use fleet_core::redact::Redactor;
use fleet_maestro::stdout::strip_ansi;
use fleet_protocol::{frame::Telemetry, journal::ClockSample};
use time::OffsetDateTime;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::{
    ipc::{Control, Frame, Stream},
    metrics::Sampler,
    outbox::{Outbox, SPOOL_LIMIT},
    session::{Observation, Session},
    transport::{self, Shared},
    watch::{BundleScanner, LogTailer},
};

/// Fault injection for tests (research 02 §14): `observer_panic`, `observer_abort`,
/// `observer_hang`, `observer_exit`.
const FAULT_ENV: &str = "FLEET_FAULT";
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
    rt.block_on(observe())?;
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
    pgid: Option<u32>,
    stdout: Lines,
    sampler: Sampler,
    shared: Arc<Shared>,
    journal: Option<BufWriter<File>>,
    telemetry: Option<mpsc::Sender<Telemetry>>,
    transport: Option<tokio::task::JoinHandle<transport::End>>,
    tseq: u64,
    dropped_bytes: u64,
    devices: mpsc::Receiver<Vec<(String, fleet_protocol::event::AdbState)>>,
}

async fn observe() -> io::Result<()> {
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
                Some(Frame::Control(Control::ChildStarted { pid, pgid, argv, debug_dir, report, run_dir, plan, .. })) => {
                    let mut l = start(&argv, pid, pgid, debug_dir, report, run_dir.as_deref().map(Path::new), clock(), clock);
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
                Some(Frame::Control(Control::ChildExited { code, signal })) => {
                    if let Some(l) = live.take() {
                        finalize(l, code, signal, clock).await;
                    }
                    return Ok(());
                }
                None => {
                    // The supervisor is gone while Maestro may still run: stop Maestro (research 02 §1.5).
                    if let Some(mut l) = live.take() {
                        if let Some(pgid) = l.pgid.and_then(|p| i32::try_from(p).ok()) {
                            let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pgid), nix::sys::signal::Signal::SIGTERM);
                        }
                        l.session.observe(clock(), Observation::Signal(15));
                        finalize(l, None, None, clock).await;
                    }
                    return Ok(());
                }
            },
            _ = poll.tick() => {
                if let Some(l) = live.as_mut() {
                    watch(l, false, &clock);
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
    at: ClockSample,
    clock: impl Fn() -> ClockSample + Send + Sync + 'static,
) -> Live {
    let agent_run_id = Uuid::now_v7();
    let redactor = Redactor::for_run(
        argv,
        std::env::vars()
            .collect::<Vec<_>>()
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str())),
    );
    let session = Session::new(argv, redactor, Box::new(Uuid::now_v7), at);
    let outbox = run_dir.map_or_else(Outbox::default, |d| {
        Outbox::with_spool(&d.join("spool.bin"), agent_run_id, SPOOL_LIMIT)
    });
    let shared = Arc::new(Shared {
        outbox: std::sync::Mutex::new(outbox),
        ..Shared::default()
    });
    let journal = run_dir
        .and_then(|d| File::create(d.join("journal.jsonl")).ok())
        .map(BufWriter::new);
    let (telemetry, transport) = match server_url() {
        Some(url) => {
            let (tx, rx) = mpsc::channel(64);
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
        session,
        tailer: debug
            .as_ref()
            .map(|d| LogTailer::new(d.join("maestro.log"))),
        scanner: debug.map(BundleScanner::new),
        report: report.map(PathBuf::from),
        pgid: group,
        stdout: Lines::default(),
        sampler: Sampler::new(Some(pid)),
        shared,
        journal,
        telemetry,
        transport,
        tseq: 0,
        dropped_bytes: 0,
        devices,
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
            if let Ok(line) = serde_json::to_string(e) {
                let _ = writeln!(j, "{line}");
            }
        }
        let _ = j.flush();
    }
    if let Ok(mut o) = l.shared.outbox.lock() {
        o.push(events);
    }
    l.shared.wake.notify_one();
}

async fn finalize(
    mut l: Live,
    code: Option<i32>,
    signal: Option<i32>,
    clock: impl Fn() -> ClockSample,
) {
    // Let Maestro's last writes land, then take a final look.
    tokio::time::sleep(POLL).await;
    watch(&mut l, true, &clock);
    let report = l.report.as_ref().and_then(|p| std::fs::read(p).ok());
    l.session.observe(
        clock(),
        Observation::Exited {
            code,
            signal,
            report_present: report.is_some(),
        },
    );
    l.session.observe(clock(), Observation::Report(report));
    l.session.end(clock(), l.dropped_bytes);
    flush(&mut l);
    l.shared.finishing.store(true, Ordering::SeqCst);
    l.shared.wake.notify_one();
    if let Some(handle) = l.transport.take() {
        let budget = std::env::var("FLEET_FLUSH_TIMEOUT")
            .ok()
            .and_then(|s| s.parse().ok())
            .map_or(Duration::from_secs(20), Duration::from_secs);
        match tokio::time::timeout(budget, handle).await {
            Ok(Ok(end)) => tracing::info!(?end, "transport finished"),
            Ok(Err(e)) => tracing::warn!(error = %e, "transport task failed"),
            Err(_) => tracing::warn!("transport did not flush in time; the spool keeps the events"),
        }
    }
}

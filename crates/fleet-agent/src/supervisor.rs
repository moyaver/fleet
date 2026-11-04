// SPDX-License-Identifier: AGPL-3.0-only
//! The supervisor: the process CI waits on (research 02 §1, invariant I1).
//!
//! It uses only `std` threads, never touches the network, and never lets telemetry decide its
//! exit status. Maestro's output is written to our own stdout/stderr first; the observer gets a
//! copy through a bounded channel, and copies that don't fit are dropped and counted.

use std::{
    ffi::OsString,
    io::{self, BufRead, BufReader, IsTerminal, Read, Write},
    process::{Child, ChildStdin, Command, ExitStatus, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use nix::{
    sys::signal::{Signal, kill, killpg},
    unistd::Pid,
};
use signal_hook::{
    consts::{SIGHUP, SIGINT, SIGQUIT, SIGTERM},
    iterator::Signals,
};

use crate::ipc::{Control, Frame, Stream};

/// How the supervisor ends: the child's exit code, or the signal that killed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// Exit with this code.
    Code(i32),
    /// Die by this signal, as the child did.
    Signal(i32),
}

/// Supervisor settings.
#[derive(Debug, Clone)]
pub(crate) struct Options {
    /// Time from the first INT/TERM until the child's group is killed.
    pub(crate) grace: Duration,
    /// How long to wait for the observer to finish after the child exits.
    pub(crate) finalize_timeout: Duration,
    /// Run without an observer at all.
    pub(crate) no_observer: bool,
    /// Add the agent's flags to `maestro test` when the user didn't set them.
    pub(crate) rewrite: bool,
    /// The agent's directory for this run (default: under `$RUNNER_TEMP` or the temp dir).
    pub(crate) run_dir: Option<std::path::PathBuf>,
    /// A Fleet plan manifest and the shard of it to run.
    pub(crate) plan: Option<(std::path::PathBuf, u32)>,
}

const CHUNK: usize = 64 * 1024;
/// Bounded copy queue to the observer, in chunks (~16 MiB at most).
const QUEUE: usize = 256;
/// After the child exits, how long to keep draining pipes that a grandchild may hold open.
const DRAIN_AFTER_EXIT: Duration = Duration::from_secs(2);

/// Runs `command` under supervision and returns how the supervisor must end.
#[must_use]
pub(crate) fn run(command: &[OsString], opts: &Options) -> Outcome {
    let prepared = match crate::prepare::prepare(
        command,
        opts.rewrite,
        opts.run_dir.as_deref(),
        opts.plan.as_ref().map(|(p, s)| (p.as_path(), *s)),
        |k| std::env::var_os(k).is_some(),
    ) {
        Ok(p) => p,
        Err(e) => {
            notice(&e.0);
            return Outcome::Code(2);
        }
    };
    let Some((program, args)) = prepared.argv.split_first() else {
        notice("nothing to run: use `fleet-agent run -- maestro test …`");
        return Outcome::Code(2);
    };
    let warnings = Warnings::default();

    // Register handlers before spawning so no signal slips between spawn and registration.
    let signals = Signals::new([SIGINT, SIGTERM, SIGHUP, SIGQUIT]);

    let mut observer = if opts.no_observer {
        None
    } else {
        Observer::spawn(&warnings, prepared.run_dir.as_deref())
    };

    let own_group = !io::stdin().is_terminal();
    let mut child = match spawn_child(program, args, own_group, &prepared.env) {
        Ok(c) => c,
        Err(code) => {
            finish_observer(observer.as_mut(), Outcome::Code(code), 0, opts, &warnings);
            return Outcome::Code(code);
        }
    };
    let pid = child.id();
    let target = Target {
        pid,
        group: own_group,
    };

    if let Some(o) = &observer {
        o.send(Frame::Control(Control::ChildStarted {
            pid,
            pgid: own_group.then_some(pid),
            argv: prepared
                .argv
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect(),
            started_at_ms: now_ms(),
            debug_dir: prepared.debug_dir.clone(),
            report: prepared.report.clone(),
            run_dir: prepared
                .run_dir
                .as_ref()
                .map(|d| d.to_string_lossy().into_owned()),
            plan: prepared.plan.clone(),
        }));
    }

    let exited = Arc::new(AtomicBool::new(false));
    match signals {
        Ok(signals) => spawn_signal_thread(
            signals,
            target,
            opts.grace,
            Arc::clone(&exited),
            observer.as_ref().and_then(Observer::sender),
        ),
        Err(e) => warnings.push(format!("signal forwarding unavailable: {e}")),
    }

    let last_stderr_byte = Arc::new(Mutex::new(b'\n'));
    // Authoritative count of output bytes not copied to the observer (I5), both streams.
    let dropped_total = Arc::new(AtomicU64::new(0));
    let pumps = [
        spawn_pump(
            &mut child,
            Stream::Stdout,
            observer.as_ref().and_then(Observer::sender),
            None,
            Arc::clone(&dropped_total),
        ),
        spawn_pump(
            &mut child,
            Stream::Stderr,
            observer.as_ref().and_then(Observer::sender),
            Some(Arc::clone(&last_stderr_byte)),
            Arc::clone(&dropped_total),
        ),
    ];

    let status = wait(&mut child);
    exited.store(true, Ordering::SeqCst);
    let deadline = Instant::now() + DRAIN_AFTER_EXIT;
    for pump in pumps.into_iter().flatten() {
        // A grandchild can keep the pipe open; don't let that hold up the exit.
        while !pump.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
    }

    let outcome = outcome(status);
    // Read after the pumps stop. A pump still held open by a grandchild past the drain deadline
    // may drop more later; those bytes are not part of the run's total.
    let dropped = dropped_total.load(Ordering::SeqCst);
    finish_observer(observer.as_mut(), outcome, dropped, opts, &warnings);
    warnings.flush(&last_stderr_byte);
    outcome
}

/// Starts the command. On failure, returns the shell's exit code for it: 126 when it isn't
/// executable, 127 otherwise.
fn spawn_child(
    program: &OsString,
    args: &[OsString],
    own_group: bool,
    env: &[(String, String)],
) -> Result<Child, i32> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if own_group {
        // setpgid(0, 0) in the child: signals reach the JVM and its helpers (research 02 §1.3).
        std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    }
    cmd.spawn().map_err(|e| {
        notice(&format!(
            "could not start {}: {e}",
            program.to_string_lossy()
        ));
        if e.kind() == io::ErrorKind::PermissionDenied {
            126
        } else {
            127
        }
    })
}

fn finish_observer(
    observer: Option<&mut Observer>,
    outcome: Outcome,
    output_bytes_dropped: u64,
    opts: &Options,
    warnings: &Warnings,
) {
    let Some(o) = observer else { return };
    let (code, signal) = match outcome {
        Outcome::Code(c) => (Some(c), None),
        Outcome::Signal(s) => (None, Some(s)),
    };
    o.finish(
        Control::ChildExited {
            code,
            signal,
            output_bytes_dropped,
        },
        opts.finalize_timeout,
        warnings,
    );
}

/// Where forwarded signals go.
#[derive(Clone, Copy)]
struct Target {
    pid: u32,
    group: bool,
}

impl Target {
    fn signal(self, sig: Signal) {
        let Ok(pid) = i32::try_from(self.pid).map(Pid::from_raw) else {
            return;
        };
        // ESRCH after the child has gone is expected and harmless.
        let _ = if self.group {
            killpg(pid, sig)
        } else {
            kill(pid, sig)
        };
    }
}

fn spawn_signal_thread(
    mut signals: Signals,
    target: Target,
    grace: Duration,
    exited: Arc<AtomicBool>,
    observer: Option<SyncSender<Frame>>,
) {
    thread::spawn(move || {
        let mut grace_started = false;
        for sig in signals.forever() {
            if exited.load(Ordering::SeqCst) {
                continue;
            }
            if let Ok(s) = Signal::try_from(sig) {
                target.signal(s);
            }
            if let Some(tx) = &observer {
                let _ = tx.try_send(Frame::Control(Control::Signal { signal: sig }));
            }
            if !grace_started && (sig == SIGINT || sig == SIGTERM) {
                grace_started = true;
                let exited = Arc::clone(&exited);
                thread::spawn(move || {
                    thread::sleep(grace);
                    if !exited.load(Ordering::SeqCst) {
                        target.signal(Signal::SIGKILL);
                    }
                });
            }
        }
    });
}

/// Copies one child stream to our own stream, byte for byte, then offers a copy to the observer.
fn spawn_pump(
    child: &mut Child,
    stream: Stream,
    observer: Option<SyncSender<Frame>>,
    last_byte: Option<Arc<Mutex<u8>>>,
    dropped_total: Arc<AtomicU64>,
) -> Option<thread::JoinHandle<()>> {
    let source: Box<dyn Read + Send> = match stream {
        Stream::Stdout => Box::new(child.stdout.take()?),
        Stream::Stderr => Box::new(child.stderr.take()?),
    };
    Some(thread::spawn(move || match stream {
        Stream::Stdout => pump(
            source,
            io::stdout(),
            stream,
            observer.as_ref(),
            last_byte.as_deref(),
            &dropped_total,
        ),
        Stream::Stderr => pump(
            source,
            io::stderr(),
            stream,
            observer.as_ref(),
            last_byte.as_deref(),
            &dropped_total,
        ),
    }))
}

/// Writes every chunk to `sink` (flushed, so partial lines appear at once), then offers a copy.
///
/// Every byte whose copy is dropped is added to `dropped_total`, so the count survives a gap
/// notice that never fits before EOF (I5). `Control::Gap` only serves live accounting.
fn pump(
    mut source: impl Read,
    mut sink: impl Write,
    stream: Stream,
    observer: Option<&SyncSender<Frame>>,
    last_byte: Option<&Mutex<u8>>,
    dropped_total: &AtomicU64,
) {
    let mut buf = vec![0u8; CHUNK];
    let mut sink_ok = true;
    let mut dropped: u64 = 0;
    loop {
        let n = match source.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return,
        };
        let chunk = &buf[..n];
        if sink_ok {
            // A closed CI log (EPIPE) must not stop us draining, or Maestro would block.
            sink_ok = sink.write_all(chunk).and_then(|()| sink.flush()).is_ok();
        }
        if let Some(lock) = last_byte
            && let Ok(mut b) = lock.lock()
        {
            *b = chunk[n - 1];
        }
        let Some(tx) = observer else { continue };
        if dropped > 0 {
            if tx
                .try_send(Frame::Control(Control::Gap {
                    stream,
                    bytes: dropped,
                }))
                .is_err()
            {
                dropped += n as u64;
                dropped_total.fetch_add(n as u64, Ordering::SeqCst);
                continue;
            }
            dropped = 0;
        }
        match tx.try_send(Frame::Output(stream, chunk.to_vec())) {
            Ok(()) | Err(TrySendError::Disconnected(_)) => {}
            Err(TrySendError::Full(_)) => {
                dropped += n as u64;
                dropped_total.fetch_add(n as u64, Ordering::SeqCst);
            }
        }
    }
}

fn wait(child: &mut Child) -> Option<ExitStatus> {
    loop {
        match child.wait() {
            Ok(s) => return Some(s),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    }
}

fn outcome(status: Option<ExitStatus>) -> Outcome {
    use std::os::unix::process::ExitStatusExt;
    match status {
        Some(s) => match (s.code(), s.signal()) {
            (Some(code), _) => Outcome::Code(code),
            (None, Some(sig)) => Outcome::Signal(sig),
            (None, None) => Outcome::Code(1),
        },
        None => Outcome::Code(1),
    }
}

/// One `[fleet-agent]` line on stderr. Only used before the child produced any output.
fn notice(message: &str) {
    let _ = writeln!(io::stderr().lock(), "[fleet-agent] {message}");
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// One-line notices for the end of the run, printed at a line boundary on stderr.
#[derive(Default, Clone)]
struct Warnings(Arc<Mutex<Vec<String>>>);

impl Warnings {
    fn push(&self, w: String) {
        if let Ok(mut v) = self.0.lock() {
            v.push(w);
        }
    }

    fn flush(&self, last_stderr_byte: &Mutex<u8>) {
        let Ok(v) = self.0.lock() else { return };
        if v.is_empty() {
            return;
        }
        let mut err = io::stderr().lock();
        if last_stderr_byte.lock().is_ok_and(|b| *b != b'\n') {
            let _ = err.write_all(b"\n");
        }
        for w in v.iter() {
            let _ = writeln!(err, "[fleet-agent] {w}");
        }
    }
}

/// The observer child: same binary, `__observe`, fed frames on its stdin.
struct Observer {
    child: Child,
    tx: Option<SyncSender<Frame>>,
    writer: Option<thread::JoinHandle<()>>,
    control: Receiver<String>,
}

impl Observer {
    fn spawn(warnings: &Warnings, run_dir: Option<&std::path::Path>) -> Option<Self> {
        let exe = match std::env::current_exe() {
            Ok(e) => e,
            Err(e) => {
                warnings.push(format!("telemetry off: cannot find own executable: {e}"));
                return None;
            }
        };
        let mut cmd = Command::new(exe);
        if let Some(d) = run_dir {
            cmd.env(crate::observer::LOG_ENV, d.join("observer.log"));
        }
        let mut child = match cmd
            .env(
                crate::observer::SUPERVISOR_PID_ENV,
                std::process::id().to_string(),
            )
            .arg("__observe")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                warnings.push(format!("telemetry off: observer did not start: {e}"));
                return None;
            }
        };
        let stdin = child.stdin.take()?;
        let stdout = child.stdout.take()?;
        let (tx, rx) = mpsc::sync_channel::<Frame>(QUEUE);
        let writer = thread::spawn(move || write_frames(stdin, &rx));
        let (ctl_tx, control) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if ctl_tx.send(line).is_err() {
                    return;
                }
            }
        });
        Some(Self {
            child,
            tx: Some(tx),
            writer: Some(writer),
            control,
        })
    }

    fn sender(&self) -> Option<SyncSender<Frame>> {
        self.tx.clone()
    }

    fn send(&self, frame: Frame) {
        if let Some(tx) = &self.tx {
            let _ = tx.try_send(frame);
        }
    }

    /// Sends the final frame, closes the pipe and waits for `finalized` up to `timeout`.
    fn finish(&mut self, last: Control, timeout: Duration, warnings: &Warnings) {
        if let Some(tx) = self.tx.take() {
            // Blocking here is bounded: the writer drops frames once the observer is gone.
            let _ = tx.send_timeout_compat(Frame::Control(last), timeout);
        }
        let deadline = Instant::now() + timeout;
        let mut finalized = false;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match self.control.recv_timeout(left) {
                Ok(line) if line == "finalized" => {
                    finalized = true;
                    break;
                }
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
            }
        }
        if !finalized {
            let _ = self.child.kill();
            warnings.push(if self.exited() {
                "telemetry stopped early: the observer exited".to_owned()
            } else {
                format!(
                    "telemetry incomplete: the observer did not finish within {}s",
                    timeout.as_secs()
                )
            });
        }
        // Never wait on the observer without a deadline (it may hold no pipe we can close).
        let reap_by = Instant::now() + Duration::from_secs(1);
        while !self.exited() && Instant::now() < reap_by {
            thread::sleep(Duration::from_millis(10));
        }
        if !self.exited() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        if let Some(w) = self.writer.take()
            && w.is_finished()
        {
            let _ = w.join();
        }
    }

    fn exited(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }
}

/// `SyncSender::send` with a deadline, so a hung observer can't hold up the exit.
trait SendTimeout {
    fn send_timeout_compat(&self, frame: Frame, timeout: Duration) -> Result<(), ()>;
}

impl SendTimeout for SyncSender<Frame> {
    fn send_timeout_compat(&self, mut frame: Frame, timeout: Duration) -> Result<(), ()> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.try_send(frame) {
                Ok(()) => return Ok(()),

                Err(TrySendError::Full(f)) if Instant::now() < deadline => {
                    frame = f;
                    thread::sleep(Duration::from_millis(5));
                }
                Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => return Err(()),
            }
        }
    }
}

fn write_frames(mut stdin: ChildStdin, rx: &Receiver<Frame>) {
    let mut alive = true;
    for frame in rx {
        if alive && frame.write_to(&mut stdin).is_err() {
            // Keep draining so senders never see a full queue because of a dead observer.
            alive = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pump_passes_through_and_counts_gaps() {
        let (tx, rx) = mpsc::sync_channel::<Frame>(1);
        // 3 chunks into a queue of 1: the first copy fits, the rest are dropped and counted.
        let data = vec![b'x'; CHUNK * 3];
        let mut sink = Vec::new();
        pump(
            data.as_slice(),
            &mut sink,
            Stream::Stderr,
            Some(&tx),
            None,
            &AtomicU64::new(0),
        );
        assert_eq!(
            sink, data,
            "the sink gets everything, even when copies are dropped"
        );
        drop(tx);
        let frames: Vec<Frame> = rx.into_iter().collect();
        assert_eq!(frames.len(), 1);
        assert!(matches!(&frames[0], Frame::Output(Stream::Stderr, b) if b.len() == CHUNK));
    }

    #[test]
    fn pump_counts_bytes_dropped_at_eof() {
        // Queue of 1 that nobody reads: chunk 1 is queued, chunks 2 and 3 (the last) are dropped
        // and no later chunk exists to carry a Gap, yet the total must include them.
        let (tx, rx) = mpsc::sync_channel::<Frame>(1);
        let total = AtomicU64::new(0);
        let data = vec![b'x'; CHUNK * 3 - 5];
        pump(
            data.as_slice(),
            io::sink(),
            Stream::Stdout,
            Some(&tx),
            None,
            &total,
        );
        drop(tx);
        let frames: Vec<Frame> = rx.into_iter().collect();
        assert!(
            frames.iter().all(|f| matches!(f, Frame::Output(..))),
            "{frames:?}"
        );
        assert_eq!(total.load(Ordering::SeqCst), (CHUNK * 2 - 5) as u64);
    }

    #[test]
    fn pump_reports_the_gap_once_the_queue_frees_up() {
        let (tx, rx) = mpsc::sync_channel::<Frame>(1);
        let reader = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            rx.into_iter().collect::<Vec<_>>()
        });
        let slow = SlowReader {
            chunks: 3,
            delay: Duration::from_millis(100),
        };
        pump(
            slow,
            io::sink(),
            Stream::Stderr,
            Some(&tx),
            None,
            &AtomicU64::new(0),
        );
        drop(tx);
        let frames = reader.join().unwrap();
        let output: usize = frames
            .iter()
            .filter(|f| matches!(f, Frame::Output(..)))
            .count();
        assert!(output >= 2, "{frames:?}");
    }

    struct SlowReader {
        chunks: usize,
        delay: Duration,
    }

    impl Read for SlowReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.chunks == 0 {
                return Ok(0);
            }
            self.chunks -= 1;
            thread::sleep(self.delay);
            buf[0] = b'.';
            Ok(1)
        }
    }

    #[test]
    fn outcome_maps_exit_status() {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(
            outcome(Some(ExitStatus::from_raw(3 << 8))),
            Outcome::Code(3)
        );
        assert_eq!(outcome(Some(ExitStatus::from_raw(15))), Outcome::Signal(15));
        assert_eq!(outcome(None), Outcome::Code(1));
    }
}

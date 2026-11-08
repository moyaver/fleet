// SPDX-License-Identifier: AGPL-3.0-only
//! Following Maestro's files while it runs (evidence lanes A and C; research 08 Q2, Q6).
//!
//! Polling, not filesystem notifications: research 08 measured every log line within one 50 ms
//! poll, a scan of a debug directory is a handful of `stat`s, and polling behaves the same on
//! every runner and filesystem.

use std::{
    collections::{BTreeMap, HashSet},
    ffi::OsStr,
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant, SystemTime},
};

use fleet_maestro::{invocation::OutputRoot, log};

use crate::session::Observation;

/// Whether `name` is a `yyyy-MM-dd_HHmmss` directory name: the run directory Maestro creates
/// (research 01 §4.1).
fn is_run_stamp(name: &str) -> bool {
    name.len() == 17
        && name.char_indices().all(|(i, c)| match i {
            4 | 7 => c == '-',
            10 => c == '_',
            _ => c.is_ascii_digit(),
        })
}

/// Maestro's session log, in each run directory.
const SESSION_LOG: &str = "maestro.log";

/// How often a stamped directory is looked for again while several candidates exist and none is
/// known to be this run's.
const PROBE_EVERY: Duration = Duration::from_millis(250);

/// Whether `path` was created at or after `started`. A file system without birth times falls back
/// to the modification time.
fn created_since(path: &Path, started: SystemTime) -> bool {
    std::fs::metadata(path)
        .and_then(|m| m.created().or_else(|_| m.modified()))
        .is_ok_and(|born| born >= started)
}

/// Whether process `pid` holds `file` open, or `None` where that cannot be told. Linux reads
/// `/proc/<pid>/fd`; other Unix systems ask `lsof`. Maestro's JVM holds its session log open for the
/// whole run: its log4j file appender is configured once (`maestro-client`, `LogConfig`), and `lsof`
/// on a running Maestro 2.12.1 JVM lists the log file.
pub(crate) fn holds_open(pid: u32, file: &Path) -> Option<bool> {
    let want = std::fs::canonicalize(file).ok()?;
    if cfg!(target_os = "linux") {
        let fds = std::fs::read_dir(format!("/proc/{pid}/fd")).ok()?;
        return Some(
            fds.flatten()
                .any(|fd| std::fs::read_link(fd.path()).is_ok_and(|p| p == want)),
        );
    }
    if cfg!(unix) {
        let out = std::process::Command::new("lsof")
            .args(["-nP", "-p", &pid.to_string(), "-Fn"])
            .stderr(Stdio::null())
            .output()
            .ok()?;
        // Names of the open files come as `n<path>` lines.
        return Some(
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter_map(|l| l.strip_prefix('n'))
                .any(|p| Path::new(p) == want.as_path()),
        );
    }
    None
}

/// The stamped directories in `base` that were created since `started` and hold a session log,
/// oldest name first.
fn stamped_dirs(base: &Path, started: SystemTime) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(base) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| e.file_name().to_str().is_some_and(is_run_stamp))
        .map(|e| e.path())
        .filter(|p| p.join(SESSION_LOG).is_file() && created_since(p, started))
        .collect();
    dirs.sort();
    dirs
}

/// The candidate that is this run's. With several, it is the one this run's process holds its
/// session log open in. With one, it is taken unless the process is known not to hold it. With no
/// way to ask (`pid` is `None`, or the system cannot tell), a lone candidate is taken and several
/// are not: a guess between runs is never made (issue #148).
fn choose(candidates: &[PathBuf], pid: Option<u32>) -> Option<PathBuf> {
    let held = |dir: &PathBuf| pid.and_then(|p| holds_open(p, &dir.join(SESSION_LOG)));
    match candidates {
        [] => None,
        [only] => (held(only) != Some(false)).then(|| only.clone()),
        many => many.iter().find(|d| held(d) == Some(true)).cloned(),
    }
}

/// One of a run's output directories, found once Maestro has made it.
///
/// A flat directory is known from the start. A stamped one is a `yyyy-MM-dd_HHmmss` directory
/// Maestro creates under its base for this run. The session log's is found by the process: a
/// candidate is this run's when the run's own JVM holds its `maestro.log` open (see [`choose`]).
/// The flow bundles of a stamped layout sit in a directory with the same stamp, under their own
/// base, so they are found from the log's directory name.
#[derive(Debug)]
pub(crate) struct Located {
    root: OutputRoot,
    started: SystemTime,
    dir: Option<PathBuf>,
    probe_after: Option<Instant>,
}

impl Located {
    pub(crate) fn new(root: OutputRoot, started: SystemTime) -> Self {
        let dir = match &root {
            OutputRoot::Flat(dir) => Some(dir.clone()),
            OutputRoot::Stamped(_) => None,
        };
        Self {
            root,
            started,
            dir,
            probe_after: None,
        }
    }

    /// The session log's directory, or `None` until this run's stamped directory is found. `pid` is
    /// the run's process; `now` paces the look while candidates exist but none is this run's.
    pub(crate) fn log_dir(&mut self, pid: Option<u32>, now: Instant) -> Option<&Path> {
        if self.dir.is_none()
            && let OutputRoot::Stamped(base) = &self.root
            && self.probe_after.is_none_or(|t| now >= t)
        {
            let candidates = stamped_dirs(base, self.started);
            let found = choose(&candidates, pid);
            if found.is_none() && !candidates.is_empty() {
                self.probe_after = Some(now + PROBE_EVERY);
            }
            self.dir = found;
        }
        self.dir.as_deref()
    }

    /// The flow bundles' directory: a flat one, or the stamped one named `stamp` (the session log's
    /// directory name) under the base. `None` until it exists.
    pub(crate) fn bundles_dir(&mut self, stamp: Option<&OsStr>) -> Option<&Path> {
        if self.dir.is_none()
            && let (OutputRoot::Stamped(base), Some(stamp)) = (&self.root, stamp)
        {
            let dir = base.join(stamp);
            self.dir = dir.is_dir().then_some(dir);
        }
        self.dir.as_deref()
    }
}

/// Reads lines appended to the session `maestro.log`.
#[derive(Debug)]
pub(crate) struct LogTailer {
    path: PathBuf,
    offset: u64,
    partial: Vec<u8>,
    assembler: log::Assembler,
}

impl LogTailer {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self {
            path,
            offset: 0,
            partial: Vec::new(),
            assembler: log::Assembler::default(),
        }
    }

    /// Records completed since the last poll.
    pub(crate) fn poll(&mut self) -> Vec<log::Record> {
        let mut out = Vec::new();
        let Ok(mut f) = File::open(&self.path) else {
            return out;
        };
        if f.metadata().is_ok_and(|m| m.len() < self.offset) {
            // Truncated or replaced: start over.
            self.offset = 0;
            self.partial.clear();
        }
        if f.seek(SeekFrom::Start(self.offset)).is_err() {
            return out;
        }
        let mut buf = Vec::new();
        let Ok(n) = f.read_to_end(&mut buf) else {
            return out;
        };
        self.offset += n as u64;
        self.partial.extend_from_slice(&buf);
        while let Some(i) = self.partial.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.partial.drain(..=i).collect();
            if let Some(r) = self.assembler.push(&String::from_utf8_lossy(&line)) {
                out.push(r);
            }
        }
        out
    }

    /// The last record, at the end of the run.
    pub(crate) fn finish(&mut self) -> Option<log::Record> {
        self.assembler.finish()
    }
}

/// Longest crash report read. Maestro's is about a kilobyte; the rest is not needed.
const MAX_CRASH_REPORT: u64 = 64 * 1024;

/// Longest wait between two looks at a bundle root while nothing new shows up.
const MAX_LOOK_INTERVAL: Duration = Duration::from_secs(1);

/// Shortest wait between two looks, while new flow files keep appearing.
const MIN_LOOK_INTERVAL: Duration = Duration::from_millis(50);

/// Whether `dir` holds Maestro's flow files: its `manifest.json` or `commands.json`, or its `logs`
/// or `screenshots` folder. A directory with none of them is not a flow, whatever its name.
fn is_flow_dir(dir: &Path) -> bool {
    ["manifest.json", "commands.json"]
        .iter()
        .any(|f| dir.join(f).is_file())
        || dir.join("logs").is_dir()
        || dir.join("screenshots").is_dir()
}

/// Reports flow bundle files as they appear, in one root Maestro writes flow folders into.
///
/// The root can be shared: a flattened debug directory with no `--debug-output` is the home
/// directory. So the root is read only for what the run created: a directory counts once it was
/// created since the run started and shows Maestro's flow files, and everything else in the root
/// is classified once and never read again. The root is looked at on a backoff (every 50 ms while
/// new files appear, then up to once a second), so an idle run reads it rarely (I10, privacy).
#[derive(Debug)]
pub(crate) struct BundleScanner {
    root: PathBuf,
    started: SystemTime,
    /// Flow folders, by name: read on every look.
    flows: BTreeMap<String, PathBuf>,
    /// Directories the run created that do not show flow files yet.
    pending: BTreeMap<String, PathBuf>,
    /// Entries that are not the run's: classified once.
    other: HashSet<String>,
    /// Flow folders whose `commands.json` was already reported.
    commands_read: HashSet<String>,
    /// Flow folders whose `logs/crash-report.txt` was already reported.
    crash_read: HashSet<String>,
    /// Names already reported, so nothing is reported twice.
    seen: HashSet<String>,
    interval: Duration,
    next_look: Option<Instant>,
    /// How many times the root was listed, for the test that bounds the work.
    #[cfg(test)]
    pub(crate) listings: usize,
}

impl BundleScanner {
    pub(crate) fn new(root: PathBuf, started: SystemTime) -> Self {
        Self {
            root,
            started,
            flows: BTreeMap::new(),
            pending: BTreeMap::new(),
            other: HashSet::new(),
            commands_read: HashSet::new(),
            crash_read: HashSet::new(),
            seen: HashSet::new(),
            interval: MIN_LOOK_INTERVAL,
            next_look: None,
            #[cfg(test)]
            listings: 0,
        }
    }

    /// New observations. `commands.json` is read once its `manifest.json` exists (the flow's
    /// last file), or at the end of the run (`final_pass`), when a cut-short flow has none.
    pub(crate) fn scan(&mut self, final_pass: bool) -> Vec<Observation> {
        self.scan_at(Instant::now(), final_pass)
    }

    fn scan_at(&mut self, now: Instant, final_pass: bool) -> Vec<Observation> {
        if !final_pass && self.next_look.is_some_and(|t| now < t) {
            return Vec::new();
        }
        self.look();
        let mut out = Vec::new();
        let flows: Vec<(String, PathBuf)> = self
            .flows
            .iter()
            .map(|(n, p)| (n.clone(), p.clone()))
            .collect();
        for (name, path) in flows {
            self.report(&name, None, &mut out);
            for sub in ["screenshots", "screen-hierarchy"] {
                let Ok(files) = std::fs::read_dir(path.join(sub)) else {
                    continue;
                };
                let mut files: Vec<String> = files
                    .flatten()
                    .map(|f| f.file_name().to_string_lossy().into_owned())
                    .collect();
                files.sort();
                for f in files.into_iter().filter(|f| {
                    !Path::new(f)
                        .extension()
                        .is_some_and(|e| e.eq_ignore_ascii_case("tmp"))
                }) {
                    self.report(&format!("{name}/{sub}/{f}"), None, &mut out);
                }
            }
            let manifest = path.join("manifest.json").exists();
            let commands = path.join("commands.json");
            if (manifest || final_pass)
                && commands.exists()
                && self.commands_read.insert(name.clone())
            {
                let bytes = std::fs::read(&commands).ok();
                out.push(Observation::File {
                    rel: format!("{name}/commands.json"),
                    contents: bytes,
                });
            }
            let crash = path.join("logs").join("crash-report.txt");
            if (manifest || final_pass) && crash.is_file() && self.crash_read.insert(name.clone()) {
                out.push(Observation::File {
                    rel: format!("{name}/logs/crash-report.txt"),
                    contents: read_head(&crash),
                });
            }
            if manifest {
                self.report(&format!("{name}/manifest.json"), None, &mut out);
            }
        }
        self.interval = if out.is_empty() {
            (self.interval * 2).min(MAX_LOOK_INTERVAL)
        } else {
            MIN_LOOK_INTERVAL
        };
        self.next_look = Some(now + self.interval);
        out
    }

    /// Lists the root once: classifies its new entries and promotes the pending directories that
    /// now show flow files.
    fn look(&mut self) {
        #[cfg(test)]
        {
            self.listings += 1;
        }
        if let Ok(entries) = std::fs::read_dir(&self.root) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if self.other.contains(&name)
                    || self.pending.contains_key(&name)
                    || self.flows.contains_key(&name)
                {
                    continue;
                }
                let path = e.path();
                if e.file_type().is_ok_and(|t| t.is_dir()) && created_since(&path, self.started) {
                    self.pending.insert(name, path);
                } else {
                    self.other.insert(name);
                }
            }
        }
        let ready: Vec<String> = self
            .pending
            .iter()
            .filter(|(_, p)| is_flow_dir(p))
            .map(|(n, _)| n.clone())
            .collect();
        for name in ready {
            if let Some(path) = self.pending.remove(&name) {
                self.flows.insert(name, path);
            }
        }
    }

    fn report(&mut self, rel: &str, contents: Option<Vec<u8>>, out: &mut Vec<Observation>) {
        if self.seen.insert(rel.to_owned()) {
            out.push(Observation::File {
                rel: rel.to_owned(),
                contents,
            });
        }
    }
}

/// What one pass over Maestro's files reports, in the order the session must see it: the log's
/// records, then the bundle files (issue #148).
///
/// The tailer holds back the newest record until a header follows it. A `commands.json` is lined
/// up against the log's steps when it is read, so the held-back record is released first, whenever
/// this pass reads one (or it is the final pass). Otherwise a flow's last step would be missing
/// from the log when its file settles it. A continuation line that comes after a released record
/// is not attached to it.
pub(crate) fn observations(
    mut tailer: Option<&mut LogTailer>,
    scanner: Option<&mut BundleScanner>,
    final_pass: bool,
) -> Vec<Observation> {
    let mut out = Vec::new();
    if let Some(t) = tailer.as_mut() {
        out.extend(t.poll().into_iter().map(Observation::Log));
    }
    let files = scanner.map(|s| s.scan(final_pass)).unwrap_or_default();
    let reads_commands = files
        .iter()
        .any(|o| matches!(o, Observation::File { rel, .. } if rel.ends_with("/commands.json")));
    if (final_pass || reads_commands)
        && let Some(r) = tailer.and_then(LogTailer::finish)
    {
        out.push(Observation::Log(r));
    }
    out.extend(files);
    out
}

/// The first [`MAX_CRASH_REPORT`] bytes of `path`, or `None` when it cannot be read.
fn read_head(path: &Path) -> Option<Vec<u8>> {
    let mut buf = Vec::new();
    File::open(path)
        .ok()?
        .take(MAX_CRASH_REPORT)
        .read_to_end(&mut buf)
        .ok()?;
    Some(buf)
}

#[cfg(test)]
mod tests {
    use std::{io::Write, time::UNIX_EPOCH};

    use fleet_protocol::journal::{ClockSample, SourceKind};

    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("fleet-watch-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn the_tailer_returns_complete_records_as_they_grow() {
        let d = tmp("tail");
        let path = d.join("maestro.log");
        let mut t = LogTailer::new(path.clone());
        assert!(t.poll().is_empty(), "no file yet");
        let mut f = File::create(&path).unwrap();
        f.write_all(b"MFLT1|1|t|INFO|x.A|one\nMFLT1|2|t|INFO|x.A|tw")
            .unwrap();
        assert!(
            t.poll().is_empty(),
            "first record isn't complete until the next header"
        );
        f.write_all(b"o\nstack line\nMFLT1|3|t|INFO|x.A|three\n")
            .unwrap();
        let r = t.poll();
        assert_eq!(
            r.iter().map(|r| r.message.as_str()).collect::<Vec<_>>(),
            ["one", "two"]
        );
        assert_eq!(r[1].continuation, ["stack line"]);
        assert_eq!(t.finish().map(|r| r.message), Some("three".into()));
        std::fs::remove_dir_all(&d).ok();
    }

    /// A look `secs` seconds after the scanner's start: the backoff never delays a test past its
    /// own clock.
    fn at(start: Instant, secs: u64) -> Instant {
        start + Duration::from_secs(secs)
    }

    #[test]
    fn the_scanner_reports_once_and_reads_commands_after_the_manifest() {
        let d = tmp("scan");
        let flow = d.join("login");
        std::fs::create_dir_all(flow.join("screenshots")).unwrap();
        let mut s = BundleScanner::new(d.clone(), UNIX_EPOCH);
        let t = Instant::now();
        let rels = |obs: Vec<Observation>| {
            obs.into_iter()
                .map(|o| match o {
                    Observation::File { rel, .. } => rel,
                    _ => String::new(),
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(rels(s.scan_at(at(t, 0), false)), ["login"]);
        std::fs::write(flow.join("screenshots/step-003-x.png.tmp"), b"").unwrap();
        std::fs::write(flow.join("commands.json"), b"[]").unwrap();
        assert!(
            s.scan_at(at(t, 1), false).is_empty(),
            "no .tmp, and commands waits for the manifest"
        );
        std::fs::write(flow.join("screenshots/step-003-x.png"), b"").unwrap();
        std::fs::write(flow.join("manifest.json"), b"{}").unwrap();
        assert_eq!(
            rels(s.scan_at(at(t, 2), false)),
            [
                "login/screenshots/step-003-x.png",
                "login/commands.json",
                "login/manifest.json"
            ]
        );
        assert!(s.scan_at(at(t, 3), true).is_empty(), "nothing twice");
        std::fs::remove_dir_all(&d).ok();
    }

    /// A crash report is read once, when its flow's manifest is there, and never twice.
    #[test]
    fn the_scanner_reads_a_crash_report_once_after_the_manifest() {
        let d = tmp("crash");
        let flow = d.join("crash");
        std::fs::create_dir_all(flow.join("logs")).unwrap();
        let mut s = BundleScanner::new(d.clone(), UNIX_EPOCH);
        let t = Instant::now();
        let crash_seen = |obs: &[Observation]| {
            obs.iter().any(|o| {
                matches!(o, Observation::File { rel, contents: Some(c) }
                    if rel == "crash/logs/crash-report.txt" && c == b"boom")
            })
        };
        std::fs::write(flow.join("logs/crash-report.txt"), b"boom").unwrap();
        let before = s.scan_at(at(t, 0), false);
        assert!(!crash_seen(&before), "no manifest yet");
        std::fs::write(flow.join("manifest.json"), b"{}").unwrap();
        assert!(crash_seen(&s.scan_at(at(t, 1), false)));
        assert!(!crash_seen(&s.scan_at(at(t, 2), true)), "nothing twice");
        std::fs::remove_dir_all(&d).ok();
    }

    /// One machine-pattern record of `maestro.log`, as Maestro writes it.
    fn mflt(logger: &str, message: &str) -> String {
        format!("MFLT1|1|main|INFO|{logger}|{message}\n")
    }

    /// The newest record of a log is held back until a header follows it. A `commands.json` that the
    /// same pass reads is lined up against the log, so that record must reach the session first.
    /// Otherwise the flow's last step settles against a log one step short (issue #148).
    #[test]
    fn the_newest_log_record_reaches_the_session_before_the_commands_file_that_settles_against_it()
    {
        const SUITE: &str = "maestro.cli.runner.TestSuiteInteractor";
        const CONSOLE: &str = "maestro.cli.runner.CliConsoleListener";
        let d = tmp("order");
        let log = d.join("maestro.log");
        let mut text = mflt(SUITE, " Running flow pass");
        text.push_str(&mflt(CONSOLE, "Tap on \"a\" RUNNING"));
        text.push_str(&mflt(CONSOLE, "Tap on \"a\" COMPLETED"));
        // The last step is skipped: its only line is the held-back record, and it has no RUNNING line.
        text.push_str(&mflt(CONSOLE, "Tap on \"b\" SKIPPED"));
        std::fs::write(&log, text).unwrap();
        let flow = d.join("pass");
        std::fs::create_dir_all(&flow).unwrap();
        let tap = |n: u32, status: &str| {
            format!(
                r#"{{"command":{{"tapOnElement":{{}}}},"metadata":{{"status":"{status}","sequenceNumber":{n},"depth":0}}}}"#
            )
        };
        let commands = format!("[{},{}]", tap(0, "COMPLETED"), tap(1, "SKIPPED"));
        std::fs::write(flow.join("commands.json"), commands).unwrap();
        std::fs::write(flow.join("manifest.json"), b"{}").unwrap();

        let at = ClockSample {
            wall: time::OffsetDateTime::UNIX_EPOCH,
            mono_ms: 0,
        };
        let mut n = 0u128;
        let ids: crate::session::IdGen = Box::new(move || {
            n += 1;
            uuid::Uuid::from_u128(n)
        });
        let mut session = crate::session::Session::new(
            &["maestro".into(), "test".into()],
            None,
            fleet_core::redact::Redactor::new([]),
            ids,
            at,
        );
        let mut tailer = LogTailer::new(log);
        let mut scanner = BundleScanner::new(d.clone(), UNIX_EPOCH);
        for o in observations(Some(&mut tailer), Some(&mut scanner), false) {
            session.observe(at, o);
        }
        let settled: Vec<u32> = session
            .drain()
            .into_iter()
            .filter(|e| e.kind == "step.ended" && e.src.kind == SourceKind::ArtifactFile)
            .filter_map(|e| e.subject.step.map(|r| r.index))
            .collect();
        assert_eq!(settled, [0, 1], "both steps settle from the file");
        std::fs::remove_dir_all(&d).ok();
    }

    /// Two runs in one tree: each run takes the directory its own process holds the session log
    /// open in, never the other's, however the directories were created (issue #148). The test
    /// process stands in for the Maestro JVM by holding one log open.
    #[test]
    fn a_run_takes_only_the_directory_its_own_process_holds_open() {
        let d = tmp("identity");
        let (ours, theirs) = (d.join("2026-11-08_101112"), d.join("2026-11-08_101113"));
        for dir in [&ours, &theirs] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let held = File::create(ours.join(SESSION_LOG)).unwrap();
        File::create(theirs.join(SESSION_LOG)).unwrap();
        let now = Instant::now();
        let me = std::process::id();

        let mut l = Located::new(OutputRoot::Stamped(d.clone()), UNIX_EPOCH);
        assert_eq!(
            l.log_dir(Some(me), now),
            Some(ours.as_path()),
            "the directory this process holds"
        );
        let mut unknown = Located::new(OutputRoot::Stamped(d.clone()), UNIX_EPOCH);
        assert_eq!(
            unknown.log_dir(None, now),
            None,
            "two candidates and no process to ask: no guess"
        );
        drop(held);
        std::fs::remove_dir_all(&d).ok();
    }

    /// A lone candidate that this run's process provably does not hold is another run's: it is not
    /// taken, and the look is paced rather than repeated on every poll.
    #[test]
    fn a_lone_directory_the_process_does_not_hold_is_not_this_runs() {
        let d = tmp("notmine");
        let other = d.join("2026-11-08_101112");
        std::fs::create_dir_all(&other).unwrap();
        File::create(other.join(SESSION_LOG)).unwrap();
        let t = Instant::now();
        let mut l = Located::new(OutputRoot::Stamped(d.clone()), UNIX_EPOCH);
        assert_eq!(l.log_dir(Some(std::process::id()), t), None);
        assert_eq!(
            l.log_dir(Some(std::process::id()), t + Duration::from_millis(10)),
            None,
            "paced: no second look this soon"
        );
        std::fs::remove_dir_all(&d).ok();
    }

    /// The run's directory is one created since the run started, never an older one. A lone
    /// candidate that no process can be asked about is taken.
    #[test]
    fn the_run_directory_is_the_one_created_for_this_run() {
        let d = tmp("stamp");
        std::fs::create_dir_all(d.join("2020-01-01_000000")).unwrap();
        File::create(d.join("2020-01-01_000000").join(SESSION_LOG)).unwrap();
        std::fs::create_dir_all(d.join("not-a-run")).unwrap();
        let started = SystemTime::now();
        let t = Instant::now();
        let mut l = Located::new(OutputRoot::Stamped(d.clone()), started);
        assert_eq!(l.log_dir(None, t), None, "an older run is not this one");
        let mine = d.join("2026-11-08_101112");
        std::fs::create_dir_all(&mine).unwrap();
        File::create(mine.join(SESSION_LOG)).unwrap();
        let mut l = Located::new(OutputRoot::Stamped(d.clone()), started);
        assert_eq!(l.log_dir(None, t), Some(mine.as_path()));
        let mut missing = Located::new(OutputRoot::Stamped(d.join("missing")), started);
        assert_eq!(missing.log_dir(None, t), None);
        std::fs::remove_dir_all(&d).ok();
    }

    /// The bundles of a stamped layout sit under their own base, in the directory with the log's
    /// stamp; a flat layout is known from the start.
    #[test]
    fn the_bundles_follow_the_logs_stamp() {
        let d = tmp("bundles");
        let base = d.join("T");
        std::fs::create_dir_all(base.join("2026-11-08_101112")).unwrap();
        let mut b = Located::new(OutputRoot::Stamped(base.clone()), UNIX_EPOCH);
        assert_eq!(b.bundles_dir(None), None, "no log yet, no stamp");
        assert_eq!(
            b.bundles_dir(Some(OsStr::new("2026-11-08_101112"))),
            Some(base.join("2026-11-08_101112").as_path())
        );
        let mut flat = Located::new(OutputRoot::Flat(d.clone()), UNIX_EPOCH);
        assert_eq!(flat.bundles_dir(None), Some(d.as_path()));
        std::fs::remove_dir_all(&d).ok();
    }

    /// The shared root of a flattened debug directory with no `--debug-output` (the home directory)
    /// is listed on a backoff, not on every poll, and only the run's own flow folders are reported.
    /// The root here holds 2000 directories from before the run (issue #148, I10).
    #[test]
    fn a_shared_root_is_listed_on_a_backoff_and_only_the_runs_flows_are_reported() {
        let home = tmp("home");
        for i in 0..2000 {
            std::fs::create_dir_all(home.join(format!("old-{i}"))).unwrap();
        }
        let started = SystemTime::now();
        let mut s = BundleScanner::new(home.clone(), started);
        let t = Instant::now();
        let polls = 200u64;
        let mut reported = Vec::new();
        for poll in 0..polls {
            if poll == 100 {
                // The run's flow appears after five simulated seconds.
                let flow = home.join("pass");
                std::fs::create_dir_all(flow.join("logs")).unwrap();
                std::fs::write(flow.join("manifest.json"), b"{}").unwrap();
            }
            let now = t + Duration::from_millis(50 * poll);
            for o in s.scan_at(now, false) {
                if let Observation::File { rel, .. } = o {
                    reported.push(rel);
                }
            }
        }
        // Old scanner: 200 listings. Here the backoff alone gives about 13 over ten seconds, and a
        // few more while new files appear.
        assert!(
            s.listings <= 20,
            "the root was listed {} times over {polls} polls",
            s.listings
        );
        assert!(
            reported.iter().any(|r| r == "pass/manifest.json"),
            "the run's flow is reported: {reported:?}"
        );
        assert!(
            reported
                .iter()
                .all(|r| r == "pass" || r.starts_with("pass/")),
            "no directory from before the run is reported: {reported:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Each captured layout (fixtures/maestro/2.12.1) has its session log and its flow bundles
    /// where the layout model says Maestro wrote them, so the live watcher finds the same files.
    /// A capture's home directory is its `run/home` (the `JAVA_OPTS` of its flatten captures name
    /// it as `user.home`), and its workspace config is read relative to its `run` directory.
    #[test]
    fn the_captured_layouts_are_found_where_the_layout_model_puts_them() {
        use fleet_maestro::invocation::{output_layout, parse};
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/maestro/2.12.1");
        let scenarios = [
            "paths-none",
            "single-flow-no-format",
            "paths-test-output-dir",
            "paths-debug-output",
            "paths-debug-and-test-output-same",
            "html-test-output-dir",
            "html-no-debug-flags",
            "html-report-default-path",
            "paths-debug-and-test-output-apart",
            "paths-flatten-alone",
            "paths-flatten-test-output-dir",
            "paths-flatten-debug-and-test-output",
            "paths-flatten-config",
            "paths-config-explicit",
            "paths-config-overridden-by-flag",
            "paths-config-dir-input",
            "paths-config-file-beside",
        ];
        for scenario in scenarios {
            let capture = fixtures.join(scenario);
            let text = std::fs::read_to_string(capture.join("meta.json")).unwrap();
            let meta: serde_json::Value = serde_json::from_str(&text).unwrap();
            let capture_str = capture.to_string_lossy().into_owned();
            let argv: Vec<String> = meta["command"]
                .as_array()
                .unwrap()
                .iter()
                .map(|a| a.as_str().unwrap().replace("<CAPTURE>", &capture_str))
                .collect();
            // The capture ran with XDG_STATE_HOME=<CAPTURE>/state, so Maestro's state directory is
            // `state/maestro`, and its tests live under `tests`.
            let run = capture.join("run");
            let state = capture.join("state/maestro/tests");
            let home = run.join("home");
            let workspace = crate::layout::workspace_test_dir(&argv, Some(&run));
            let layout = output_layout(
                &parse(&argv),
                Some(&state),
                Some(&home),
                workspace.as_deref(),
            )
            .unwrap_or_else(|| panic!("{scenario}: a layout"));
            let mut log = Located::new(layout.log, UNIX_EPOCH);
            let log_dir = log
                .log_dir(None, Instant::now())
                .unwrap_or_else(|| panic!("{scenario}: the log directory"))
                .to_path_buf();
            assert!(
                log_dir.join("maestro.log").is_file(),
                "{scenario}: the session log is at {}",
                log_dir.display()
            );
            // A relative test output directory is relative to Maestro's working directory, the run.
            let bundles_root = match layout.bundles {
                OutputRoot::Stamped(base) => OutputRoot::Stamped(run.join(base)),
                flat @ OutputRoot::Flat(_) => flat,
            };
            let mut bundles = Located::new(bundles_root, UNIX_EPOCH);
            let bundle_dir = bundles
                .bundles_dir(log_dir.file_name())
                .unwrap_or_else(|| panic!("{scenario}: the bundle directory"))
                .to_path_buf();
            let has_commands = std::fs::read_dir(&bundle_dir)
                .unwrap()
                .flatten()
                .any(|e| e.path().join("commands.json").is_file());
            assert!(
                has_commands,
                "{scenario}: a flow bundle with commands.json is in {}",
                bundle_dir.display()
            );
        }
    }
}

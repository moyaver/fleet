// SPDX-License-Identifier: AGPL-3.0-only
//! Following Maestro's files while it runs (evidence lanes A and C; research 08 Q2, Q6).
//!
//! Polling, not filesystem notifications: research 08 measured every log line within one 50 ms
//! poll, a scan of a debug directory is a handful of `stat`s, and polling behaves the same on
//! every runner and filesystem.

use std::{
    collections::HashSet,
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

use fleet_maestro::log;

use crate::session::Observation;

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

/// Reports flow bundle files as they appear.
#[derive(Debug)]
pub(crate) struct BundleScanner {
    root: PathBuf,
    seen: HashSet<String>,
    /// Flow folders whose `commands.json` was already reported.
    commands_read: HashSet<String>,
    /// Flow folders whose `logs/crash-report.txt` was already reported.
    crash_read: HashSet<String>,
}

impl BundleScanner {
    pub(crate) fn new(root: PathBuf) -> Self {
        Self {
            root,
            seen: HashSet::new(),
            commands_read: HashSet::new(),
            crash_read: HashSet::new(),
        }
    }

    /// New observations. `commands.json` is read once its `manifest.json` exists (the flow's
    /// last file), or at the end of the run (`final_pass`), when a cut-short flow has none.
    pub(crate) fn scan(&mut self, final_pass: bool) -> Vec<Observation> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return out;
        };
        let mut dirs: Vec<(String, PathBuf)> = entries
            .flatten()
            .filter(|e| e.path().is_dir())
            .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
            .collect();
        dirs.sort();
        for (name, path) in dirs {
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
        out
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
    use std::io::Write;

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

    #[test]
    fn the_scanner_reports_once_and_reads_commands_after_the_manifest() {
        let d = tmp("scan");
        let flow = d.join("login");
        std::fs::create_dir_all(flow.join("screenshots")).unwrap();
        let mut s = BundleScanner::new(d.clone());
        let rels = |obs: Vec<Observation>| {
            obs.into_iter()
                .map(|o| match o {
                    Observation::File { rel, .. } => rel,
                    _ => String::new(),
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(rels(s.scan(false)), ["login"]);
        std::fs::write(flow.join("screenshots/step-003-x.png.tmp"), b"").unwrap();
        std::fs::write(flow.join("commands.json"), b"[]").unwrap();
        assert!(
            s.scan(false).is_empty(),
            "no .tmp, and commands waits for the manifest"
        );
        std::fs::write(flow.join("screenshots/step-003-x.png"), b"").unwrap();
        std::fs::write(flow.join("manifest.json"), b"{}").unwrap();
        assert_eq!(
            rels(s.scan(false)),
            [
                "login/screenshots/step-003-x.png",
                "login/commands.json",
                "login/manifest.json"
            ]
        );
        assert!(s.scan(true).is_empty(), "nothing twice");
        std::fs::remove_dir_all(&d).ok();
    }

    /// A crash report is read once, when its flow's manifest is there, and never twice.
    #[test]
    fn the_scanner_reads_a_crash_report_once_after_the_manifest() {
        let d = tmp("crash");
        let flow = d.join("crash");
        std::fs::create_dir_all(flow.join("logs")).unwrap();
        let mut s = BundleScanner::new(d.clone());
        let crash_seen = |obs: &[Observation]| {
            obs.iter().any(|o| {
                matches!(o, Observation::File { rel, contents: Some(c) }
                    if rel == "crash/logs/crash-report.txt" && c == b"boom")
            })
        };
        std::fs::write(flow.join("logs/crash-report.txt"), b"boom").unwrap();
        let before = s.scan(false);
        assert!(!crash_seen(&before), "no manifest yet");
        std::fs::write(flow.join("manifest.json"), b"{}").unwrap();
        assert!(crash_seen(&s.scan(false)));
        assert!(!crash_seen(&s.scan(true)), "nothing twice");
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
        let mut scanner = BundleScanner::new(d.clone());
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
}

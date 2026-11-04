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

/// Reports flow bundle files as they appear.
#[derive(Debug)]
pub(crate) struct BundleScanner {
    root: PathBuf,
    seen: HashSet<String>,
    /// Flow folders whose `commands.json` was already reported.
    commands_read: HashSet<String>,
}

impl BundleScanner {
    pub(crate) fn new(root: PathBuf) -> Self {
        Self {
            root,
            seen: HashSet::new(),
            commands_read: HashSet::new(),
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

#[cfg(test)]
mod tests {
    use std::io::Write;

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
}

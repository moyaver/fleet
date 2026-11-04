// SPDX-License-Identifier: AGPL-3.0-only
//! `cargo xtask fixtures capture | import`: record real Maestro runs and turn them into
//! fixtures under `fixtures/maestro/<version>/<scenario>/` (PLAN §12 M1, research 01 §15).
//!
//! `capture` runs a command and records, against one monotonic clock:
//! byte-exact stdout/stderr, the arrival time of every stdout/stderr line, every file created or
//! grown under the capture directory (polled), every line appended to any `maestro.log` *as the
//! tailer saw it*, process-tree snapshots, an optional signal, and the exit status.
//! `XDG_STATE_HOME` points into the capture so Maestro's session tree lands there too.

use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    os::unix::process::ExitStatusExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, bail};
use serde_json::{Value, json};

const USAGE: &str = "usage:
  cargo xtask fixtures capture --out <dir> [--env K=V]... [--signal INT|TERM|KILL]
                               [--signal-after-ms N | --signal-on <text>] -- <command>...
      `{RUN}` in the command is replaced by <dir>/run.
  cargo xtask fixtures import <capture-dir> <version>/<scenario>
                              [--redact <value>]... [--replace <from>=<to>]...";

/// Environment the agent itself injects (PLAN §6.3); captures mirror it unless overridden.
const DEFAULT_ENV: &[(&str, &str)] = &[
    ("MAESTRO_CLI_NO_ANALYTICS", "1"),
    ("MAESTRO_DISABLE_UPDATE_CHECK", "true"),
    ("MAESTRO_CLI_ANALYSIS_NOTIFICATION_DISABLED", "true"),
];

const POLL: Duration = Duration::from_millis(50);
const PROC_EVERY: Duration = Duration::from_secs(2);

/// Text files above this are cut to head + tail on import, to keep the repo small.
const MAX_TEXT: usize = 512 * 1024;
/// Device logcat only shows the format; keep a head and tail of it.
const MAX_LOGCAT: usize = 32 * 1024;
/// Binary files above this are left out on import (a note records them).
const MAX_BINARY: u64 = 1024 * 1024;

pub(crate) fn run(root: &Path, args: &[&str]) -> anyhow::Result<()> {
    match args {
        ["capture", rest @ ..] => capture(&CaptureArgs::parse(rest)?),
        ["import", capture_dir, dest, rest @ ..] => {
            let mut rules = parse_import_rules(rest)?;
            rules.push((root.to_string_lossy().into_owned(), "<REPO>".to_owned()));
            import(
                Path::new(capture_dir),
                &root.join("fixtures/maestro").join(dest),
                rules,
            )
        }
        _ => bail!(USAGE),
    }
}

#[derive(Debug, PartialEq)]
enum Trigger {
    AfterMs(u64),
    OnText(String),
}

#[derive(Debug, PartialEq)]
struct CaptureArgs {
    out: PathBuf,
    env: Vec<(String, String)>,
    signal: Option<(String, Trigger)>,
    command: Vec<String>,
}

impl CaptureArgs {
    fn parse(args: &[&str]) -> anyhow::Result<Self> {
        let split = args.iter().position(|a| *a == "--").context(USAGE)?;
        let (opts, command) = (&args[..split], &args[split + 1..]);
        if command.is_empty() {
            bail!(USAGE);
        }
        let (mut out, mut env, mut signal, mut trigger) = (None, Vec::new(), None, None);
        let mut it = opts.iter();
        while let Some(opt) = it.next() {
            let mut value = || {
                it.next()
                    .copied()
                    .with_context(|| format!("{opt} needs a value"))
            };
            match *opt {
                "--out" => out = Some(PathBuf::from(value()?)),
                "--env" => {
                    let (k, v) = value()?.split_once('=').context("--env takes K=V")?;
                    env.push((k.to_owned(), v.to_owned()));
                }
                "--signal" => {
                    let s = value()?;
                    if !["INT", "TERM", "KILL"].contains(&s) {
                        bail!("--signal must be INT, TERM or KILL");
                    }
                    signal = Some(s.to_owned());
                }
                "--signal-after-ms" => trigger = Some(Trigger::AfterMs(value()?.parse()?)),
                "--signal-on" => trigger = Some(Trigger::OnText(value()?.to_owned())),
                other => bail!("unknown option {other}\n{USAGE}"),
            }
        }
        let signal = match (signal, trigger) {
            (Some(s), Some(t)) => Some((s, t)),
            (None, None) => None,
            _ => bail!("--signal needs --signal-after-ms or --signal-on, and the other way round"),
        };
        let out = out.context("--out is required")?;
        let run_dir = out.join("run");
        let command = command
            .iter()
            .map(|a| a.replace("{RUN}", &run_dir.to_string_lossy()))
            .collect();
        Ok(Self {
            out,
            env,
            signal,
            command,
        })
    }
}

/// `--redact V` becomes `(V, "<REDACTED>")`; `--replace A=B` becomes `(A, B)`.
fn parse_import_rules(args: &[&str]) -> anyhow::Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    let mut it = args.iter();
    while let Some(opt) = it.next() {
        let value = it.next().with_context(|| format!("{opt} needs a value"))?;
        match *opt {
            "--redact" => out.push(((*value).to_owned(), "<REDACTED>".to_owned())),
            "--replace" => {
                let (from, to) = value.split_once('=').context("--replace takes FROM=TO")?;
                out.push((from.to_owned(), to.to_owned()));
            }
            other => bail!("unknown option {other}\n{USAGE}"),
        }
    }
    Ok(out)
}

/// One timeline entry, stamped with milliseconds since the command started.
fn event(t0: Instant, src: &str, mut body: Value) -> Value {
    body["t_ms"] = json!(u64::try_from(t0.elapsed().as_millis()).unwrap_or(u64::MAX));
    body["src"] = json!(src);
    body
}

#[expect(
    clippy::too_many_lines,
    reason = "one linear capture pipeline sharing one clock and channel"
)]
fn capture(args: &CaptureArgs) -> anyhow::Result<()> {
    if args.out.exists() {
        bail!(
            "{} already exists; captures always start empty",
            args.out.display()
        );
    }
    let run_dir = args.out.join("run");
    let state = args.out.join("state");
    fs::create_dir_all(&run_dir)?;
    fs::create_dir_all(&state)?;

    let mut cmd = Command::new(&args.command[0]);
    cmd.args(&args.command[1..])
        .env("XDG_STATE_HOME", &state)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in DEFAULT_ENV {
        cmd.env(k, v);
    }
    for (k, v) in &args.env {
        cmd.env(k, v);
    }

    let t0 = Instant::now();
    let started = time_now();
    let mut child = cmd
        .spawn()
        .with_context(|| format!("starting {}", args.command[0]))?;
    let pid = child.id();
    let (tx, rx) = mpsc::channel::<Value>();
    let done = Arc::new(AtomicBool::new(false));
    let fire = Arc::new(AtomicBool::new(false));

    let mut readers = Vec::new();
    for (name, pipe) in [
        (
            "stdout",
            child
                .stdout
                .take()
                .map(|p| Box::new(p) as Box<dyn Read + Send>),
        ),
        (
            "stderr",
            child
                .stderr
                .take()
                .map(|p| Box::new(p) as Box<dyn Read + Send>),
        ),
    ] {
        let pipe = pipe.context("pipe")?;
        let mut raw = fs::File::create(args.out.join(format!("{name}.bin")))?;
        let (tx, fire) = (tx.clone(), Arc::clone(&fire));
        let on_text = match &args.signal {
            Some((_, Trigger::OnText(t))) => Some(t.clone()),
            _ => None,
        };
        readers.push(thread::spawn(move || -> anyhow::Result<()> {
            let mut reader = BufReader::new(pipe);
            let mut line = Vec::new();
            while reader.read_until(b'\n', &mut line)? > 0 {
                raw.write_all(&line)?;
                let text = String::from_utf8_lossy(&line).into_owned();
                if on_text.as_deref().is_some_and(|t| text.contains(t)) {
                    fire.store(true, Ordering::SeqCst);
                }
                let _ = tx.send(event(
                    t0,
                    name,
                    json!({ "bytes": line.len(), "line": text }),
                ));
                line.clear();
            }
            Ok(())
        }));
    }

    let poller = {
        let (tx, done, fire) = (tx.clone(), Arc::clone(&done), Arc::clone(&fire));
        let roots = vec![args.out.join("run"), args.out.join("state")];
        let base = args.out.clone();
        let on_text = match &args.signal {
            Some((_, Trigger::OnText(t))) => Some(t.clone()),
            _ => None,
        };
        thread::spawn(move || {
            let mut watcher = Watcher::new(base);
            loop {
                let finished = done.load(Ordering::SeqCst);
                for ev in watcher.poll(&roots) {
                    if let (Some(t), Some(line)) = (&on_text, ev["line"].as_str())
                        && line.contains(t.as_str())
                    {
                        fire.store(true, Ordering::SeqCst);
                    }
                    let src = if ev.get("line").is_some() {
                        "log"
                    } else {
                        "fs"
                    };
                    let _ = tx.send(event(t0, src, ev));
                }
                if finished {
                    break;
                }
                thread::sleep(POLL);
            }
        })
    };

    let procs = {
        let (tx, done) = (tx.clone(), Arc::clone(&done));
        thread::spawn(move || {
            while !done.load(Ordering::SeqCst) {
                if let Some(tree) = process_tree(pid) {
                    let _ = tx.send(event(t0, "proc", json!({ "tree": tree })));
                }
                thread::sleep(PROC_EVERY);
            }
        })
    };

    let signaller = args.signal.as_ref().map(|(sig, trigger)| {
        let (tx, done, fire) = (tx.clone(), Arc::clone(&done), Arc::clone(&fire));
        let (sig, after) = (
            sig.clone(),
            match trigger {
                Trigger::AfterMs(ms) => Some(Duration::from_millis(*ms)),
                Trigger::OnText(_) => None,
            },
        );
        thread::spawn(move || {
            while !done.load(Ordering::SeqCst) {
                let due = after.map_or_else(|| fire.load(Ordering::SeqCst), |d| t0.elapsed() >= d);
                if due {
                    let ok = Command::new("kill")
                        .args([format!("-{sig}"), pid.to_string()])
                        .status();
                    let _ = tx.send(event(
                        t0,
                        "signal",
                        json!({ "signal": sig, "pid": pid, "ok": ok.is_ok_and(|s| s.success()) }),
                    ));
                    return;
                }
                thread::sleep(POLL);
            }
        })
    });
    drop(tx);

    let writer = {
        let path = args.out.join("events.jsonl");
        thread::spawn(move || -> anyhow::Result<()> {
            let mut f = fs::File::create(path)?;
            for ev in rx {
                writeln!(f, "{ev}")?;
            }
            Ok(())
        })
    };

    let status = child.wait()?;
    let duration_ms = t0.elapsed().as_millis();
    for r in readers {
        r.join().map_err(|_| anyhow::anyhow!("reader panicked"))??;
    }
    // Give late writers (shutdown hooks, report generation) a moment, then take a final pass.
    thread::sleep(Duration::from_millis(500));
    done.store(true, Ordering::SeqCst);
    poller
        .join()
        .map_err(|_| anyhow::anyhow!("poller panicked"))?;
    procs
        .join()
        .map_err(|_| anyhow::anyhow!("proc sampler panicked"))?;
    if let Some(s) = signaller {
        s.join()
            .map_err(|_| anyhow::anyhow!("signaller panicked"))?;
    }
    writer
        .join()
        .map_err(|_| anyhow::anyhow!("writer panicked"))??;

    let exit =
        json!({ "code": status.code(), "signal": status.signal(), "duration_ms": duration_ms });
    fs::write(
        args.out.join("exit.json"),
        serde_json::to_string_pretty(&exit)? + "\n",
    )?;
    let meta = json!({
        "command": args.command,
        "env": args.env.iter().map(|(k, v)| json!({ "name": k, "value": v })).collect::<Vec<_>>(),
        "default_env": DEFAULT_ENV.iter().map(|(k, v)| json!({ "name": k, "value": v })).collect::<Vec<_>>(),
        "signal": args.signal.as_ref().map(|(s, t)| json!({ "signal": s, "trigger": format!("{t:?}") })),
        "started_at_unix_ms": started,
        "host": { "os": std::env::consts::OS, "arch": std::env::consts::ARCH },
    });
    fs::write(
        args.out.join("meta.json"),
        serde_json::to_string_pretty(&meta)? + "\n",
    )?;
    println!("captured {} → exit {exit}", args.out.display());
    Ok(())
}

fn time_now() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
}

/// Polls directory trees: reports new files/dirs, growth, and new lines in any `maestro.log`.
struct Watcher {
    base: PathBuf,
    sizes: BTreeMap<PathBuf, u64>,
    log_offsets: BTreeMap<PathBuf, (u64, Vec<u8>)>,
}

impl Watcher {
    fn new(base: PathBuf) -> Self {
        Self {
            base,
            sizes: BTreeMap::new(),
            log_offsets: BTreeMap::new(),
        }
    }

    fn rel(&self, p: &Path) -> String {
        p.strip_prefix(&self.base)
            .unwrap_or(p)
            .to_string_lossy()
            .into_owned()
    }

    fn poll(&mut self, roots: &[PathBuf]) -> Vec<Value> {
        let mut events = Vec::new();
        let mut stack: Vec<PathBuf> = roots.to_vec();
        while let Some(dir) = stack.pop() {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let Ok(meta) = entry.metadata() else { continue };
                if meta.is_dir() {
                    if !self.sizes.contains_key(&path) {
                        self.sizes.insert(path.clone(), 0);
                        events.push(json!({ "event": "dir", "path": self.rel(&path) }));
                    }
                    stack.push(path);
                    continue;
                }
                let size = meta.len();
                match self.sizes.insert(path.clone(), size) {
                    None => events
                        .push(json!({ "event": "created", "path": self.rel(&path), "size": size })),
                    Some(old) if old != size => events
                        .push(json!({ "event": "size", "path": self.rel(&path), "size": size })),
                    Some(_) => {}
                }
                if path.file_name().is_some_and(|n| n == "maestro.log") {
                    events.extend(self.tail(&path));
                }
            }
        }
        events
    }

    /// New complete lines appended to `path` since the last poll.
    fn tail(&mut self, path: &Path) -> Vec<Value> {
        let rel = self.rel(path);
        let (offset, pending) = self.log_offsets.entry(path.to_path_buf()).or_default();
        let Ok(mut f) = fs::File::open(path) else {
            return Vec::new();
        };
        if f.seek(SeekFrom::Start(*offset)).is_err() {
            return Vec::new();
        }
        let mut buf = Vec::new();
        let Ok(n) = f.read_to_end(&mut buf) else {
            return Vec::new();
        };
        *offset += n as u64;
        pending.extend_from_slice(&buf);
        let mut out = Vec::new();
        while let Some(i) = pending.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = pending.drain(..=i).collect();
            out.push(json!({ "file": rel, "line": String::from_utf8_lossy(&line) }));
        }
        out
    }
}

/// `ps` snapshot of `root` and its descendants: pid, ppid, pgid, rss (KiB), %cpu, command.
fn process_tree(root: u32) -> Option<Vec<Value>> {
    let out = Command::new("ps")
        .args(["-A", "-o", "pid=,ppid=,pgid=,rss=,pcpu=,command="])
        .output()
        .ok()?;
    let rows: Vec<(u32, u32, Value)> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            let pid: u32 = f.next()?.parse().ok()?;
            let parent: u32 = f.next()?.parse().ok()?;
            let group = f.next()?;
            let rss = f.next()?;
            let cpu = f.next()?;
            let cmd: Vec<&str> = f.collect();
            Some((pid, parent, json!({ "pid": pid, "ppid": parent, "pgid": group, "rss_kib": rss, "cpu": cpu, "command": cmd.join(" ") })))
        })
        .collect();
    let mut keep = vec![root];
    let mut i = 0;
    while i < keep.len() {
        let parent = keep[i];
        keep.extend(rows.iter().filter(|r| r.1 == parent).map(|r| r.0));
        i += 1;
    }
    let tree: Vec<Value> = rows
        .into_iter()
        .filter(|r| keep.contains(&r.0))
        .map(|r| r.2)
        .collect();
    (!tree.is_empty()).then_some(tree)
}

/// Copies a capture into `dest`, normalizing machine-specific text and cutting large files.
fn import(capture: &Path, dest: &Path, extra: Vec<(String, String)>) -> anyhow::Result<()> {
    if dest.exists() {
        bail!("{} already exists", dest.display());
    }
    let capture = capture
        .canonicalize()
        .with_context(|| format!("{}", capture.display()))?;
    let rules = replacements(&capture, extra);
    let mut skipped = Vec::new();
    copy_tree(&capture, &capture, dest, &rules, &mut skipped)?;
    if !skipped.is_empty() {
        fs::write(dest.join("SKIPPED.txt"), skipped.join("\n") + "\n")?;
    }
    println!("imported {} → {}", capture.display(), dest.display());
    Ok(())
}

/// Replacement pairs, longest first so a path wins over the user name inside it.
fn replacements(capture: &Path, extra: Vec<(String, String)>) -> Vec<(String, String)> {
    let mut rules = extra;
    rules.push((capture.to_string_lossy().into_owned(), "<CAPTURE>".into()));
    // macOS resolves /tmp through /private; Maestro may print either form.
    if let Ok(stripped) = capture.strip_prefix("/private") {
        rules.push((format!("/{}", stripped.display()), "<CAPTURE>".into()));
    }
    if let Some(home) = std::env::var_os("HOME") {
        rules.push((home.to_string_lossy().into_owned(), "<HOME>".into()));
    }
    if let Some(user) = std::env::var_os("USER").filter(|u| u.len() > 2) {
        rules.push((user.to_string_lossy().into_owned(), "<USER>".into()));
    }
    if let Ok(out) = Command::new("hostname").output() {
        let host = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        if host.len() > 2 {
            rules.push((host.trim_end_matches(".local").to_owned(), "<HOST>".into()));
        }
    }
    rules.retain(|(from, _)| !from.is_empty());
    rules.sort_by_key(|(from, _)| std::cmp::Reverse(from.len()));
    rules
}

fn copy_tree(
    base: &Path,
    dir: &Path,
    dest_root: &Path,
    rules: &[(String, String)],
    skipped: &mut Vec<String>,
) -> anyhow::Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        let rel = path.strip_prefix(base)?;
        let rel_text = normalize_text(&rel.to_string_lossy(), rules);
        let target = dest_root.join(&rel_text);
        if path.is_dir() {
            copy_tree(base, &path, dest_root, rules, skipped)?;
            continue;
        }
        let data = fs::read(&path)?;
        let out = match std::str::from_utf8(&data) {
            Ok(text) => {
                let max = if rel_text.ends_with("device-logcat.txt") {
                    MAX_LOGCAT
                } else {
                    MAX_TEXT
                };
                truncate(&normalize_text(text, rules), max).into_bytes()
            }
            Err(_) if data.len() as u64 > MAX_BINARY => {
                skipped.push(format!("{rel_text} ({} bytes)", data.len()));
                continue;
            }
            Err(_) => data,
        };
        fs::create_dir_all(target.parent().context("parent")?)?;
        fs::write(&target, out)?;
    }
    Ok(())
}

fn normalize_text(text: &str, rules: &[(String, String)]) -> String {
    rules.iter().fold(text.to_owned(), |acc, (from, to)| {
        acc.replace(from.as_str(), to)
    })
}

/// Keeps the first and last `max / 2` bytes (on char boundaries) with a marker in between.
fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let half = max / 2;
    let head_end = (0..=half)
        .rev()
        .find(|i| text.is_char_boundary(*i))
        .unwrap_or(0);
    let tail_start = (text.len() - half..text.len())
        .find(|i| text.is_char_boundary(*i))
        .unwrap_or(text.len());
    format!(
        "{}\n[... fleet fixtures import: {} bytes cut ...]\n{}",
        &text[..head_end],
        tail_start - head_end,
        &text[tail_start..]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_capture_args_and_substitutes_run_dir() {
        let a = CaptureArgs::parse(&[
            "--out",
            "/c",
            "--env",
            "A=b=c",
            "--signal",
            "TERM",
            "--signal-on",
            "Running flow",
            "--",
            "maestro",
            "test",
            "--debug-output",
            "{RUN}/debug",
        ])
        .unwrap();
        assert_eq!(a.out, PathBuf::from("/c"));
        assert_eq!(a.env, vec![("A".into(), "b=c".into())]);
        assert_eq!(
            a.signal,
            Some(("TERM".into(), Trigger::OnText("Running flow".into())))
        );
        assert_eq!(
            a.command,
            ["maestro", "test", "--debug-output", "/c/run/debug"]
        );
    }

    #[test]
    fn rejects_bad_capture_args() {
        assert!(CaptureArgs::parse(&["--out", "/c"]).is_err(), "no command");
        assert!(
            CaptureArgs::parse(&[
                "--out",
                "/c",
                "--signal",
                "HUP",
                "--signal-after-ms",
                "1",
                "--",
                "x"
            ])
            .is_err()
        );
        assert!(
            CaptureArgs::parse(&["--out", "/c", "--signal", "INT", "--", "x"]).is_err(),
            "signal without trigger"
        );
        assert!(CaptureArgs::parse(&["--", "x"]).is_err(), "no --out");
    }

    #[test]
    fn normalizes_longest_match_first() {
        let rules = vec![
            ("/Users/me/cap".to_owned(), "<CAPTURE>".to_owned()),
            ("/Users/me".to_owned(), "<HOME>".to_owned()),
            ("hunter2".to_owned(), "<REDACTED>".to_owned()),
        ];
        assert_eq!(
            normalize_text("/Users/me/cap/run x /Users/me/.maestro pw=hunter2", &rules),
            "<CAPTURE>/run x <HOME>/.maestro pw=<REDACTED>"
        );
        let mut sorted = replacements(
            Path::new("/x/capture-dir"),
            parse_import_rules(&["--redact", "s3cr3t-value", "--replace", "/x/r=<REPO>"]).unwrap(),
        );
        sorted.retain(|(_, to)| ["<CAPTURE>", "<REDACTED>", "<REPO>"].contains(&to.as_str()));
        assert_eq!(sorted[0].0, "/x/capture-dir", "longest first");
        assert!(
            sorted
                .iter()
                .any(|r| r == &("/x/r".to_owned(), "<REPO>".to_owned()))
        );
        assert!(parse_import_rules(&["--replace", "no-equals"]).is_err());
    }

    #[test]
    fn truncates_on_char_boundaries() {
        assert_eq!(truncate("short", 10), "short");
        let long = "é".repeat(100); // 200 bytes
        let cut = truncate(&long, 50);
        assert!(cut.starts_with('é') && cut.ends_with('é'), "{cut}");
        assert!(cut.contains("bytes cut"));
        assert!(cut.len() < long.len());
    }

    #[test]
    fn watcher_reports_files_growth_and_complete_log_lines() {
        let dir = std::env::temp_dir().join(format!("fleet-watch-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("run/flow")).unwrap();
        let mut w = Watcher::new(dir.clone());
        let roots = [dir.join("run")];
        assert_eq!(w.poll(&roots).len(), 1, "the flow dir");

        let log = dir.join("run/flow/maestro.log");
        fs::write(&log, "one\ntw").unwrap();
        let evs = w.poll(&roots);
        assert!(
            evs.iter()
                .any(|e| e["event"] == "created" && e["path"] == "run/flow/maestro.log")
        );
        let lines: Vec<_> = evs.iter().filter_map(|e| e["line"].as_str()).collect();
        assert_eq!(lines, ["one\n"], "partial line held back");

        fs::OpenOptions::new()
            .append(true)
            .open(&log)
            .unwrap()
            .write_all(b"o\n")
            .unwrap();
        let evs = w.poll(&roots);
        assert!(evs.iter().any(|e| e["event"] == "size"));
        let lines: Vec<_> = evs.iter().filter_map(|e| e["line"].as_str()).collect();
        assert_eq!(lines, ["two\n"]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn capture_records_output_exit_and_timeline() {
        let out = std::env::temp_dir().join(format!("fleet-cap-{}", std::process::id()));
        let _ = fs::remove_dir_all(&out);
        let args = CaptureArgs::parse(&[
            "--out", &out.to_string_lossy(), "--",
            "sh", "-c", "printf 'a\\nb'; echo err >&2; mkdir -p {RUN}/x; echo hi > {RUN}/x/maestro.log; sleep 0.3; exit 3",
        ])
        .unwrap();
        capture(&args).unwrap();
        assert_eq!(
            fs::read(out.join("stdout.bin")).unwrap(),
            b"a\nb",
            "byte-exact, no added newline"
        );
        assert_eq!(fs::read_to_string(out.join("stderr.bin")).unwrap(), "err\n");
        let exit: Value =
            serde_json::from_str(&fs::read_to_string(out.join("exit.json")).unwrap()).unwrap();
        assert_eq!(exit["code"], 3);
        let events = fs::read_to_string(out.join("events.jsonl")).unwrap();
        assert!(
            events.contains(r#""src":"log""#) && events.contains("hi"),
            "{events}"
        );
        assert!(events.contains(r#""src":"stderr""#));
        fs::remove_dir_all(&out).unwrap();
    }

    #[test]
    fn capture_sends_the_signal_on_text() {
        let out = std::env::temp_dir().join(format!("fleet-sig-{}", std::process::id()));
        let _ = fs::remove_dir_all(&out);
        let args = CaptureArgs::parse(&[
            "--out",
            &out.to_string_lossy(),
            "--signal",
            "TERM",
            "--signal-on",
            "ready",
            "--",
            "sh",
            "-c",
            "echo ready; sleep 5",
        ])
        .unwrap();
        capture(&args).unwrap();
        let exit: Value =
            serde_json::from_str(&fs::read_to_string(out.join("exit.json")).unwrap()).unwrap();
        assert_eq!(exit["signal"], 15, "{exit}");
        assert!(exit["duration_ms"].as_u64().unwrap() < 4000);
        fs::remove_dir_all(&out).unwrap();
    }
}

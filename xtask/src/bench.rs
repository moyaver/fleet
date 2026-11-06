// SPDX-License-Identifier: AGPL-3.0-only
//! `cargo xtask bench-overhead`: the agent's overhead on a suite run (I10, PLAN §12 M2 exit).
//!
//! Runs the fake Maestro's suite fixture (the stand-in the agent's integration tests replay)
//! `--runs` times without the agent and `--runs` times under `fleet-agent run` (offline, no
//! server), alternating which comes first. Reports wall time, the agent's CPU time and its peak
//! resident memory. Not part of `cargo xtask ci`: the numbers depend on the machine.
//!
//! CPU is the children's rusage after each run (the supervisor, its observer and the fake Maestro
//! it waited for) minus the same figure for the run without the agent, paired per round.

use std::{
    ffi::OsStr,
    fmt::Write as _,
    fs,
    os::unix::fs::symlink,
    path::Path,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context as _, bail};
use nix::sys::{
    resource::{UsageWho, getrusage},
    time::TimeVal,
};
use sysinfo::{Pid, Process, ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System};

const USAGE: &str =
    "usage: cargo xtask bench-overhead [--runs N] [--linger SECS] [--max-cpu-percent X]";
const DEFAULT_RUNS: usize = 5;
/// Seconds the fake Maestro lingers (sleeps) before it prints: the run length. The real suite in
/// the runner guide is 50 s; a short run would make the fixed start-up cost look like a large share.
const DEFAULT_LINGER_SECS: u64 = 10;
/// The fake Maestro's fixture: the same captured suite the agent's stream tests replay.
const FIXTURE: &str = "fixtures/maestro/2.12.1/suite";
const FAKE_MAESTRO: &str = "crates/fleet-agent/tests/it/fake-maestro.sh";
const FLOWS: &str = "examples/flows/phase0/suite";
/// Kept from the environment so the fake's `sh`, `sleep` and `cp` resolve, as in the stream tests.
const KEEP_ENV: [&str; 3] = ["PATH", "HOME", "TMPDIR"];
/// How often the wait loop checks for the child's exit; this bounds the wall-time error.
const POLL: Duration = Duration::from_millis(5);
/// How often memory is sampled. A full process refresh costs more than a poll, so it runs less
/// often. ponytail: a memory peak shorter than this is missed; sampling is the cheap option.
const MEMORY_EVERY: Duration = Duration::from_millis(20);

#[derive(Debug, PartialEq)]
struct Options {
    runs: usize,
    linger_secs: u64,
    max_cpu_percent: Option<f64>,
}

impl Options {
    fn parse(args: &[&str]) -> anyhow::Result<Self> {
        let mut opts = Self {
            runs: DEFAULT_RUNS,
            linger_secs: DEFAULT_LINGER_SECS,
            max_cpu_percent: None,
        };
        let mut rest = args.iter().copied();
        while let Some(flag) = rest.next() {
            let value = rest
                .next()
                .with_context(|| format!("{flag} needs a value\n{USAGE}"))?;
            match flag {
                "--runs" => opts.runs = value.parse().context("--runs takes a whole number")?,
                "--linger" => {
                    opts.linger_secs = value
                        .parse()
                        .context("--linger takes a whole number of seconds")?;
                }
                "--max-cpu-percent" => {
                    opts.max_cpu_percent =
                        Some(value.parse().context("--max-cpu-percent takes a number")?);
                }
                _ => bail!("unknown option {flag}\n{USAGE}"),
            }
        }
        if opts.runs == 0 {
            bail!("--runs must be at least 1\n{USAGE}");
        }
        Ok(opts)
    }
}

/// One run's measurements.
#[derive(Debug, Clone, Copy)]
struct Sample {
    wall: Duration,
    cpu: Duration,
    /// Peak resident memory of the agent's processes; zero without the agent.
    peak_rss: u64,
    exit_code: Option<i32>,
}

pub(crate) fn run(root: &Path, args: &[&str]) -> anyhow::Result<()> {
    let opts = Options::parse(args)?;
    crate::run_env(
        root,
        "cargo",
        &["build", "--locked", "--release", "-p", "fleet-agent"],
        &[],
    )?;
    let agent = root.join("target/release/fleet-agent");
    let work = root.join("target/bench-overhead");
    let _ = fs::remove_dir_all(&work);
    fs::create_dir_all(work.join("bin"))?;
    let maestro = work.join("bin/maestro");
    symlink(root.join(FAKE_MAESTRO), &maestro).context("linking the fake maestro")?;
    let fixture = root.join(FIXTURE);
    let flows = root.join(FLOWS);
    let agent_name = agent
        .file_name()
        .context("the agent binary has no file name")?;
    let linger = opts.linger_secs.to_string();

    let mut without = Vec::new();
    let mut with = Vec::new();
    // Round 0 warms the file cache and is not recorded.
    for round in 0..=opts.runs {
        let order = if round % 2 == 0 {
            [true, false]
        } else {
            [false, true]
        };
        for with_agent in order {
            let run_dir = work.join(format!("run-{round}-{with_agent}"));
            fs::create_dir_all(&run_dir)?;
            let cmd = run_command(
                &agent, &maestro, &fixture, &flows, &linger, &run_dir, with_agent,
            );
            let sample = measure(cmd, with_agent.then_some(agent_name))?;
            if round > 0 {
                if with_agent {
                    with.push(sample);
                } else {
                    without.push(sample);
                }
            }
            let _ = fs::remove_dir_all(&run_dir);
        }
    }
    let _ = fs::remove_dir_all(&work);

    let report = Report::new(&with, &without)?;
    print!("{}", report.table(opts.runs, opts.linger_secs));
    if let Some(max) = opts.max_cpu_percent
        && report.cpu_one_core_percent > max
    {
        bail!(
            "agent CPU is {:.2}% of one core, above --max-cpu-percent {max}",
            report.cpu_one_core_percent
        );
    }
    Ok(())
}

/// The command for one run: `fleet-agent run -- maestro test …`, or `maestro test …` alone.
/// The output flags are passed explicitly, so the agent has nothing to add and both runs get the
/// same argv.
fn run_command(
    agent: &Path,
    maestro: &Path,
    fixture: &Path,
    flows: &Path,
    linger: &str,
    run_dir: &Path,
    with_agent: bool,
) -> Command {
    let report = run_dir.join("report.xml");
    let debug = run_dir.join("debug");
    let argv = [
        OsStr::new("test"),
        OsStr::new("--format"),
        OsStr::new("junit"),
        OsStr::new("--output"),
        report.as_os_str(),
        OsStr::new("--flatten-debug-output"),
        OsStr::new("--debug-output"),
        debug.as_os_str(),
        flows.as_os_str(),
    ];
    let mut cmd = if with_agent {
        let mut cmd = Command::new(agent);
        cmd.args(["run", "--run-dir"])
            .arg(run_dir.join("agent"))
            .arg("--")
            .arg(maestro)
            .args(argv);
        cmd
    } else {
        let mut cmd = Command::new(maestro);
        cmd.args(argv);
        cmd
    };
    cmd.env_clear()
        .envs(
            KEEP_ENV
                .iter()
                .filter_map(|k| std::env::var_os(k).map(|v| (*k, v))),
        )
        .env("FLEET_FAKE_FIXTURE", fixture)
        .env("FLEET_FAKE_LINGER", linger)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd
}

/// Runs `cmd` to completion. With `agent_name`, also samples the memory of the child (the
/// supervisor) and of its children named like the agent (the observer).
fn measure(mut cmd: Command, agent_name: Option<&OsStr>) -> anyhow::Result<Sample> {
    let cpu_before = children_cpu()?;
    let started = Instant::now();
    let mut child = cmd.spawn().context("starting the run")?;
    let mut system =
        System::new_with_specifics(RefreshKind::nothing()).context("reading the process table")?;
    let mut peak_rss = 0;
    let mut next_memory = started;
    let status = loop {
        if let Some(status) = child.try_wait().context("waiting for the run")? {
            break status;
        }
        if let Some(name) = agent_name
            && Instant::now() >= next_memory
        {
            peak_rss = peak_rss.max(agent_memory(&mut system, child.id(), name));
            next_memory = Instant::now() + MEMORY_EVERY;
        }
        thread::sleep(POLL);
    };
    // try_wait reaped the child, so its rusage is already in the children's total.
    Ok(Sample {
        wall: started.elapsed(),
        cpu: children_cpu()?.saturating_sub(cpu_before),
        peak_rss,
        exit_code: status.code(),
    })
}

/// The CPU time of every child this process has waited for so far.
fn children_cpu() -> anyhow::Result<Duration> {
    let usage = getrusage(UsageWho::RUSAGE_CHILDREN).context("reading the children's rusage")?;
    Ok(timeval(usage.user_time()) + timeval(usage.system_time()))
}

fn timeval(t: TimeVal) -> Duration {
    let secs = u64::try_from(t.tv_sec()).unwrap_or(0);
    let micros = u64::try_from(t.tv_usec()).unwrap_or(0);
    Duration::from_secs(secs) + Duration::from_micros(micros)
}

/// Resident memory of the supervisor and of its children named `agent_name`.
fn agent_memory(system: &mut System, supervisor: u32, agent_name: &OsStr) -> u64 {
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().with_memory(),
    );
    let supervisor = Pid::from_u32(supervisor);
    system
        .processes()
        .values()
        .filter(|p| {
            p.pid() == supervisor || (p.parent() == Some(supervisor) && p.name() == agent_name)
        })
        .map(Process::memory)
        .sum()
}

/// Median and range of a set of measurements.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Summary {
    median: f64,
    min: f64,
    max: f64,
}

fn summarize(values: &[f64]) -> Option<Summary> {
    let median = median(values)?;
    let min = values.iter().copied().fold(f64::INFINITY, f64::min);
    let max = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    Some(Summary { median, min, max })
}

fn median(values: &[f64]) -> Option<f64> {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let mid = sorted.len() / 2;
    match sorted.len() {
        0 => None,
        n if n % 2 == 1 => sorted.get(mid).copied(),
        _ => Some(f64::midpoint(*sorted.get(mid - 1)?, *sorted.get(mid)?)),
    }
}

struct Report {
    wall_without: Summary,
    wall_with: Summary,
    /// Per round, the run with the agent minus the same run without it, in seconds.
    delta: Summary,
    /// Per round, the agent's CPU: the run with it minus the same run without it, in seconds.
    agent_cpu: Summary,
    /// Peak memory of the agent, per run, in bytes.
    peak_rss: Summary,
    cpu_one_core_percent: f64,
    cores: usize,
    cpu_host_percent: f64,
}

impl Report {
    #[expect(
        clippy::cast_precision_loss,
        reason = "byte and core counts are far below 2^53, where f64 stops being exact"
    )]
    fn new(with: &[Sample], without: &[Sample]) -> anyhow::Result<Self> {
        let mut delta = Vec::new();
        let mut agent_cpu = Vec::new();
        for (w, o) in with.iter().zip(without) {
            if w.exit_code != o.exit_code {
                bail!(
                    "the run exited {:?} with the agent and {:?} without it (I1)",
                    w.exit_code,
                    o.exit_code
                );
            }
            delta.push(secs(w.wall) - secs(o.wall));
            agent_cpu.push(secs(w.cpu) - secs(o.cpu));
        }
        let wall_with: Vec<f64> = with.iter().map(|s| secs(s.wall)).collect();
        let wall_without: Vec<f64> = without.iter().map(|s| secs(s.wall)).collect();
        let rss: Vec<f64> = with.iter().map(|s| s.peak_rss as f64).collect();
        let summary = |values: &[f64]| summarize(values).context("no runs were recorded");
        let cores = thread::available_parallelism().map_or(1, usize::from);
        let cpu_one_core_percent =
            summary(&agent_cpu)?.median / summary(&wall_with)?.median * 100.0;
        Ok(Self {
            wall_without: summary(&wall_without)?,
            wall_with: summary(&wall_with)?,
            delta: summary(&delta)?,
            agent_cpu: summary(&agent_cpu)?,
            peak_rss: summary(&rss)?,
            cpu_one_core_percent,
            cores,
            cpu_host_percent: cpu_one_core_percent / cores as f64,
        })
    }

    fn table(&self, runs: usize, linger_secs: u64) -> String {
        let ms = |v: f64| format!("{:.1} ms", v * 1000.0);
        let mib = |v: f64| format!("{:.1} MiB", v / (1024.0 * 1024.0));
        let row = |out: &mut String, label: &str, s: Summary, fmt: &dyn Fn(f64) -> String| {
            let _ = writeln!(
                out,
                "{label:<32}{:>12}   {} .. {}",
                fmt(s.median),
                fmt(s.min),
                fmt(s.max)
            );
        };
        let mut out = format!(
            "Agent overhead on the fake Maestro suite ({runs} runs each, {linger_secs} s per run, release build)\n\n{:<32}{:>12}   min .. max\n",
            "", "median"
        );
        row(&mut out, "wall, without agent", self.wall_without, &ms);
        row(&mut out, "wall, with agent", self.wall_with, &ms);
        row(&mut out, "duration delta (paired)", self.delta, &ms);
        row(&mut out, "agent CPU per run", self.agent_cpu, &ms);
        let _ = writeln!(
            out,
            "{:<32}{:>12}",
            "agent CPU, % of one core",
            format!("{:.2} %", self.cpu_one_core_percent)
        );
        let _ = writeln!(
            out,
            "{:<32}{:>12}",
            format!("agent CPU, % of host ({} cores)", self.cores),
            format!("{:.2} %", self.cpu_host_percent)
        );
        row(&mut out, "agent peak RSS", self.peak_rss, &mib);
        out
    }
}

fn secs(d: Duration) -> f64 {
    d.as_secs_f64()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn median_of_odd_and_even_counts() {
        assert_eq!(median(&[]), None);
        assert_eq!(median(&[3.0]), Some(3.0));
        assert_eq!(median(&[5.0, 1.0, 3.0]), Some(3.0));
        assert_eq!(median(&[4.0, 1.0, 3.0, 2.0]), Some(2.5));
    }

    #[test]
    fn summary_has_median_and_range() {
        assert_eq!(summarize(&[]), None);
        assert_eq!(
            summarize(&[0.5, -0.25, 1.0, 0.25]),
            Some(Summary {
                median: 0.375,
                min: -0.25,
                max: 1.0,
            })
        );
    }

    #[test]
    fn options_default_and_reject_bad_input() {
        assert_eq!(
            Options::parse(&[]).unwrap(),
            Options {
                runs: DEFAULT_RUNS,
                linger_secs: DEFAULT_LINGER_SECS,
                max_cpu_percent: None,
            }
        );
        let opts =
            Options::parse(&["--runs", "3", "--linger", "50", "--max-cpu-percent", "1"]).unwrap();
        assert_eq!(
            (opts.runs, opts.linger_secs, opts.max_cpu_percent),
            (3, 50, Some(1.0))
        );
        assert!(Options::parse(&["--linger", "0.5"]).is_err());
        assert!(Options::parse(&["--runs", "0"]).is_err());
        assert!(Options::parse(&["--runs"]).is_err());
        assert!(Options::parse(&["--bogus", "1"]).is_err());
    }
}

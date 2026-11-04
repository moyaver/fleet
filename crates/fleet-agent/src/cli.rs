// SPDX-License-Identifier: AGPL-3.0-only
//! Command-line entry point.

use std::{ffi::OsString, time::Duration};

use clap::{Parser, Subcommand};

use signal_hook::consts::signal;

use crate::supervisor::{self, Outcome};

#[derive(Parser)]
#[command(
    name = "fleet-agent",
    version,
    about = "Fleet CI agent: runs Maestro and streams what it observes"
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run a command (normally `maestro test …`) under supervision. The exit status is the
    /// command's own.
    Run {
        /// Seconds between the first INT/TERM and killing the command's process group.
        #[arg(long, env = "FLEET_GRACE", default_value_t = 15)]
        grace: u64,
        /// Seconds to wait for telemetry to finish after the command exits.
        #[arg(long, env = "FLEET_FINALIZE_TIMEOUT", default_value_t = 60)]
        finalize_timeout: u64,
        /// Run without telemetry.
        #[arg(long)]
        no_observer: bool,
        /// Add the agent's output flags to `maestro test` when missing (`auto`) or never (`off`).
        #[arg(long, env = "FLEET_REWRITE", default_value = "auto", value_parser = ["auto", "off"])]
        fleet_rewrite: String,
        /// The agent's directory for this run (spool, journal).
        #[arg(long, env = "FLEET_RUN_DIR")]
        run_dir: Option<std::path::PathBuf>,
        /// A Fleet plan manifest (ADR-0013); runs the flows of `--shard`.
        #[arg(long, requires = "shard")]
        plan: Option<std::path::PathBuf>,
        /// Which shard of `--plan` to run.
        #[arg(long, requires = "plan")]
        shard: Option<u32>,
        /// The command, after `--`.
        #[arg(last = true, required = true)]
        command: Vec<OsString>,
    },
    /// Read a saved run (a capture or a debug output directory) and print what Fleet makes of it.
    Inspect {
        /// The directory.
        dir: std::path::PathBuf,
        /// Print JSON instead of a table.
        #[arg(long)]
        json: bool,
        /// Also print every journal event (with --json).
        #[arg(long, requires = "json")]
        events: bool,
    },
    /// Send the events a crashed run left in its spool (use as an `if: always()` step).
    Flush {
        /// The spool file (`<run dir>/spool.bin`).
        spool: std::path::PathBuf,
        /// Seconds to wait for every event to be acked.
        #[arg(long, default_value_t = 60)]
        timeout: u64,
    },
    /// Check the setup: Maestro, adb, the server, the run directory. Exits 1 on any problem.
    Check,
    /// Print the agent, protocol and Maestro profile versions.
    Version,
    /// The observer process. Started by `run`; not for direct use.
    #[command(name = "__observe", hide = true)]
    Observe,
}

/// Parses arguments, runs, and ends the process with the right status. Never returns.
pub fn main() -> ! {
    let cli = Cli::parse();
    match cli.command {
        Cmd::Run {
            grace,
            finalize_timeout,
            no_observer,
            fleet_rewrite,
            run_dir,
            plan,
            shard,
            command,
        } => {
            let opts = supervisor::Options {
                grace: Duration::from_secs(grace),
                finalize_timeout: Duration::from_secs(finalize_timeout),
                no_observer,
                rewrite: fleet_rewrite == "auto",
                run_dir,
                plan: plan.zip(shard),
            };
            exit(supervisor::run(&command, &opts))
        }
        Cmd::Inspect { dir, json, events } => exit(Outcome::Code(inspect(&dir, json, events))),
        Cmd::Flush { spool, timeout } => exit(Outcome::Code(report(crate::commands::flush(
            &spool,
            Duration::from_secs(timeout),
        )))),
        Cmd::Check => {
            let checks = crate::commands::check();
            let text = checks.iter().fold(String::new(), |mut acc, c| {
                use std::fmt::Write as _;
                let status = if c.ok { "ok  " } else { "FAIL" };
                let _ = writeln!(acc, "{status} {:<8} {}", c.name, c.detail);
                acc
            });
            let code = i32::from(checks.iter().any(|c| !c.ok));
            exit(Outcome::Code(write_out(&text).max(code)))
        }
        Cmd::Version => exit(Outcome::Code(write_out(&crate::commands::version()))),
        Cmd::Observe => exit(Outcome::Code(i32::from(crate::observer::run().is_err()))),
    }
}

fn write_out(text: &str) -> i32 {
    use std::io::Write as _;
    i32::from(std::io::stdout().lock().write_all(text.as_bytes()).is_err())
}

fn report(result: anyhow::Result<String>) -> i32 {
    use std::io::Write as _;
    match result {
        Ok(msg) => write_out(&format!("{msg}\n")),
        Err(e) => {
            let _ = writeln!(std::io::stderr().lock(), "[fleet-agent] {e:#}");
            1
        }
    }
}

fn inspect(dir: &std::path::Path, json: bool, with_events: bool) -> i32 {
    use std::io::Write as _;
    let mut out = std::io::stdout().lock();
    match crate::inspect::inspect(dir) {
        Ok((result, events)) => {
            let text = if json {
                let mut v = serde_json::to_value(&result).unwrap_or_default();
                if with_events {
                    v["journal"] = serde_json::to_value(&events).unwrap_or_default();
                }
                serde_json::to_string_pretty(&v).unwrap_or_default() + "\n"
            } else {
                crate::inspect::table(&result)
            };
            i32::from(out.write_all(text.as_bytes()).is_err())
        }
        Err(e) => {
            let _ = writeln!(std::io::stderr().lock(), "[fleet-agent] inspect: {e:#}");
            1
        }
    }
}

/// Signals that dump core by default: re-raising them would dump *our* core, so exit 128+n.
const CORE_SIGNALS: &[i32] = &[
    signal::SIGQUIT,
    signal::SIGILL,
    signal::SIGTRAP,
    signal::SIGABRT,
    signal::SIGBUS,
    signal::SIGFPE,
    signal::SIGSEGV,
    signal::SIGSYS,
    signal::SIGXCPU,
    signal::SIGXFSZ,
];

/// Ends the process the way the child ended (research 02 §1.4).
#[expect(
    clippy::exit,
    reason = "the agent's single exit point: exit-status passthrough (I1)"
)]
fn exit(outcome: Outcome) -> ! {
    match outcome {
        Outcome::Code(code) => std::process::exit(code),
        Outcome::Signal(sig) => {
            if !CORE_SIGNALS.contains(&sig) {
                // Resets the disposition to default and raises: the parent sees WIFSIGNALED(sig).
                let _ = signal_hook::low_level::emulate_default_handler(sig);
            }
            std::process::exit(128 + sig)
        }
    }
}

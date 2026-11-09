// SPDX-License-Identifier: AGPL-3.0-only
//! `fleet-loadgen`: run simulated agents against a Fleet server (see the library docs).
#![expect(
    clippy::print_stdout,
    reason = "a terminal tool; its report is the interface"
)]

use std::{path::PathBuf, time::Duration};

use anyhow::Context;
use clap::Parser;
use fleet_loadgen::{Options, report_json, scenario_json};

/// Simulated agents against a Fleet server.
#[derive(Debug, Parser)]
#[command(name = "fleet-loadgen", version)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each flag is one independent switch of the scenario"
)]
struct Cli {
    /// Server base URL (`ws://host:port`, or `wss://` behind TLS).
    #[arg(long, default_value = "ws://127.0.0.1:8080")]
    server: String,
    /// API key or agent token.
    #[arg(long, env = "FLEET_TOKEN")]
    token: String,
    /// CI run ID the agents share.
    #[arg(long, default_value = "loadgen")]
    run_id: String,
    /// Simulated agents.
    #[arg(long, default_value_t = 50)]
    agents: usize,
    /// Devices per agent.
    #[arg(long, default_value_t = 4)]
    devices: u32,
    /// Flows per device.
    #[arg(long, default_value_t = 3)]
    flows: u32,
    /// Steps per flow (0: two, or 24 with `--wide`).
    #[arg(long, default_value_t = 0)]
    steps: u32,
    /// Distinct job keys, shared round-robin (overlapping job names).
    #[arg(long, default_value_t = 10)]
    job_keys: usize,
    /// Events per batch (the frame's byte budget applies too).
    #[arg(long, default_value_t = 32)]
    batch: usize,
    /// Drop every connection half-way and reconnect together.
    #[arg(long)]
    storm: bool,
    /// Resend and shuffle batches.
    #[arg(long)]
    duplicates: bool,
    /// Send every batch in reverse, without resends.
    #[arg(long)]
    reorder: bool,
    /// Drop this many events half-way through, under one `journal.gap` (a full agent buffer).
    #[arg(long, default_value_t = 0)]
    gap_events: u32,
    /// Milliseconds the agents' clock reads ahead of the server's (negative: behind).
    #[arg(long, default_value_t = 0, allow_hyphen_values = true)]
    clock_skew_ms: i64,
    /// Long flows: long names, a long failure message and 24 steps.
    #[arg(long)]
    wide: bool,
    /// Bytes of every failure message (0: the default). Over 192 KiB, it is sent as a gap.
    #[arg(long, default_value_t = 0)]
    failure_bytes: usize,
    /// Hostile strings: one per line of this file; every name, step and failure is one of them.
    #[arg(long)]
    corpus: Option<PathBuf>,
    /// The final `commands.json` lines up with none of the log's steps.
    #[arg(long)]
    unaligned: bool,
    /// Each flow has one step the final file does not list (it settles as unknown).
    #[arg(long)]
    missing_step: bool,
    /// Agents that go silent after their first events and hold their sockets open until the end.
    #[arg(long, default_value_t = 0)]
    quiet_agents: usize,
    /// Live sockets that subscribe to the project like a dashboard (ingest-to-live latency).
    #[arg(long, default_value_t = 0)]
    viewers: usize,
    /// The project the viewers subscribe to.
    #[arg(long, default_value = "mobile-app")]
    project: String,
    /// The viewer credential the live sockets use.
    #[arg(long, env = "FLEET_VIEWER_TOKEN", default_value = "")]
    viewer_token: String,
    /// Milliseconds the viewers keep reading after the last agent finished.
    #[arg(long, default_value_t = 3000)]
    linger_ms: u64,
    /// A PEM file of CA certificates for `wss://` (the roots are this file only).
    #[arg(long, env = "FLEET_CA_FILE")]
    ca_file: Option<PathBuf>,
    /// Milliseconds between batches.
    #[arg(long, default_value_t = 0)]
    pace_ms: u64,
    /// Seconds an agent may take before the run gives up on it.
    #[arg(long, default_value_t = 300)]
    deadline_s: u64,
    /// The commit the run tests, as the CI reports it.
    #[arg(long)]
    commit: Option<String>,
    /// Write the run's numbers to this file as JSON.
    #[arg(long)]
    report: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let c = Cli::parse();
    // `wss://` needs a process-wide provider; ring, as in the agent (both are in the graph).
    let _ = rustls::crypto::ring::default_provider().install_default();
    let corpus = match &c.corpus {
        Some(path) => fleet_loadgen::read_corpus(path)?,
        None => Vec::new(),
    };
    let opts = Options {
        server: c.server,
        token: c.token,
        run_id: c.run_id,
        agents: c.agents,
        devices: c.devices,
        flows: c.flows,
        steps: c.steps,
        job_keys: c.job_keys,
        batch: c.batch,
        storm: c.storm,
        duplicates: c.duplicates,
        reorder: c.reorder,
        gap_events: c.gap_events,
        clock_skew_ms: c.clock_skew_ms,
        wide: c.wide,
        failure_bytes: c.failure_bytes,
        corpus,
        unaligned: c.unaligned,
        missing_step: c.missing_step,
        quiet_agents: c.quiet_agents,
        viewers: c.viewers,
        project: c.project,
        viewer_token: c.viewer_token,
        linger: Duration::from_millis(c.linger_ms),
        ca_file: c.ca_file,
        pace: Duration::from_millis(c.pace_ms),
        deadline: Duration::from_secs(c.deadline_s),
        commit: c.commit,
        ..Options::default()
    };
    let scenario = scenario_json(&opts);
    let report = fleet_loadgen::run(opts).await?;
    let events: u64 = report.agents.iter().map(|a| a.events).sum();
    let connections: u32 = report.agents.iter().map(|a| a.connections).sum();
    println!(
        "{} agents, {events} events, {} acked, {connections} connections, {:.1} s",
        report.agents.len(),
        report.acked(),
        report.elapsed.as_secs_f64(),
    );
    if let Some(v) = &report.viewers {
        let latency = v.latency_ms.map_or_else(
            || "n/a".to_owned(),
            |[p50, p95, p99, max]| format!("{p50:.1} / {p95:.1} / {p99:.1} / {max:.1} ms"),
        );
        println!(
            "{} viewers, {} frames, {} latency samples for {} flow ends, p50 / p95 / p99 / max {latency}",
            v.viewers, v.frames, v.samples, v.flows_sent,
        );
    }
    if let Some(path) = &c.report {
        let json = report_json(&report, &scenario);
        std::fs::write(path, serde_json::to_string_pretty(&json)?)
            .with_context(|| format!("writing the report {}", path.display()))?;
    }
    anyhow::ensure!(report.acked() == events, "not every event was acked");
    Ok(())
}

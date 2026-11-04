// SPDX-License-Identifier: AGPL-3.0-only
//! `fleet-loadgen`: run simulated agents against a Fleet server (see the library docs).
#![expect(
    clippy::print_stdout,
    reason = "a terminal tool; its report is the interface"
)]

use std::time::Duration;

use clap::Parser;

/// Simulated agents against a Fleet server.
#[derive(Debug, Parser)]
#[command(name = "fleet-loadgen", version)]
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
    /// Distinct job keys, shared round-robin (overlapping job names).
    #[arg(long, default_value_t = 10)]
    job_keys: usize,
    /// Events per batch.
    #[arg(long, default_value_t = 32)]
    batch: usize,
    /// Drop every connection half-way and reconnect together.
    #[arg(long)]
    storm: bool,
    /// Resend and shuffle batches.
    #[arg(long)]
    duplicates: bool,
    /// Milliseconds between batches.
    #[arg(long, default_value_t = 0)]
    pace_ms: u64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let c = Cli::parse();
    // `wss://` needs a process-wide provider; ring, as in the agent (both are in the graph).
    let _ = rustls::crypto::ring::default_provider().install_default();
    let report = fleet_loadgen::run(fleet_loadgen::Options {
        server: c.server,
        token: c.token,
        run_id: c.run_id,
        agents: c.agents,
        devices: c.devices,
        flows: c.flows,
        job_keys: c.job_keys,
        batch: c.batch,
        storm: c.storm,
        duplicates: c.duplicates,
        pace: Duration::from_millis(c.pace_ms),
        deadline: Duration::from_secs(300),
        ..fleet_loadgen::Options::default()
    })
    .await?;
    let events: u64 = report.agents.iter().map(|a| a.events).sum();
    let connections: u32 = report.agents.iter().map(|a| a.connections).sum();
    println!(
        "{} agents, {events} events, {} acked, {connections} connections, {:.1} s ({:.0} events/s)",
        report.agents.len(),
        report.acked(),
        report.elapsed.as_secs_f64(),
        f64::from(u32::try_from(events).unwrap_or(u32::MAX))
            / report.elapsed.as_secs_f64().max(0.001)
    );
    anyhow::ensure!(report.acked() == events, "not every event was acked");
    Ok(())
}

// SPDX-License-Identifier: AGPL-3.0-only
//! `fleet`: the Fleet server binary. Parses arguments, installs logging, runs the library.

use anyhow::Context;
use clap::Parser;
use tracing_subscriber::{EnvFilter, filter::LevelFilter};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .with_env_var("FLEET_LOG")
        .from_env()
        .context("parsing FLEET_LOG")?;
    // Logs go to stderr: stdout carries command output (an API key, once).
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
    fleet_server::run(fleet_server::Cli::parse(), std::env::vars().collect()).await
}

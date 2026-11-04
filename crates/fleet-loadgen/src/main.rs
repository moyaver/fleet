// SPDX-License-Identifier: AGPL-3.0-only
//! `fleet-loadgen`: protocol tooling (PLAN §13). Today: `mock-server`, a stand-in for the Fleet
//! server that speaks the agent protocol (research 05 §3.3–§3.5) and writes every committed event
//! to a JSONL file, for validating agents against real Maestro runs before M3's server exists.
//! Load scenarios (many simulated agents) arrive with M3.
#![expect(
    clippy::print_stderr,
    reason = "a terminal tool; its output is the interface"
)]

use std::{
    collections::HashMap,
    io::Write,
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use clap::{Parser, Subcommand};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::{
    Message,
    handshake::server::{Request, Response},
    http::HeaderValue,
};

#[derive(Parser)]
#[command(name = "fleet-loadgen", version, about = "Fleet protocol tooling")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Accept agent connections, ack every event, append committed events to a file.
    MockServer {
        /// Address to listen on.
        #[arg(long, default_value = "127.0.0.1:7878")]
        listen: SocketAddr,
        /// Where committed events go (JSON lines); telemetry goes to `<out>.telemetry`.
        #[arg(long)]
        out: PathBuf,
    },
}

/// Per agent run: the highest committed sequence number.
#[derive(Default)]
struct State {
    committed: HashMap<String, u64>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Cmd::MockServer { listen, out } => mock_server(listen, out).await,
    }
}

async fn mock_server(listen: SocketAddr, out: PathBuf) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(listen).await?;
    eprintln!("mock server listening on ws://{}", listener.local_addr()?);
    let events = Arc::new(Mutex::new(
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&out)?,
    ));
    let telemetry = Arc::new(Mutex::new(
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(out.with_extension("telemetry"))?,
    ));
    let state = Arc::new(Mutex::new(State::default()));
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (sock, peer) = accepted?;
                let (events, telemetry, state) = (Arc::clone(&events), Arc::clone(&telemetry), Arc::clone(&state));
                tokio::spawn(async move {
                    if let Err(e) = connection(sock, &events, &telemetry, &state).await {
                        eprintln!("{peer}: {e}");
                    }
                });
            }
            _ = tokio::signal::ctrl_c() => return Ok(()),
        }
    }
}

async fn connection(
    sock: tokio::net::TcpStream,
    events: &Mutex<std::fs::File>,
    telemetry: &Mutex<std::fs::File>,
    state: &Mutex<State>,
) -> anyhow::Result<()> {
    #[expect(clippy::result_large_err, reason = "tungstenite's callback signature")]
    let echo_protocol = |_: &Request, mut resp: Response| {
        resp.headers_mut().insert(
            "Sec-WebSocket-Protocol",
            HeaderValue::from_static("fleet.v1+json"),
        );
        Ok(resp)
    };
    let mut ws = tokio_tungstenite::accept_hdr_async(sock, echo_protocol).await?;
    let mut run = String::new();
    while let Some(msg) = ws.next().await {
        let Message::Text(text) = msg? else { continue };
        let frame: Value = serde_json::from_str(text.as_str())?;
        let reply = match frame["op"].as_str().unwrap_or_default() {
            "hello" => {
                run = frame["agent_run_id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                let committed = state
                    .lock()
                    .map_err(|_| anyhow::anyhow!("lock"))?
                    .committed
                    .get(&run)
                    .copied()
                    .unwrap_or(0);
                eprintln!(
                    "agent run {run} connected (committed {committed}, job {})",
                    frame["ci"]["job_key"]
                );
                Some(
                    json!({ "op": "welcome", "protocol": 1, "connection_id": "00000000-0000-0000-0000-000000000000", "epoch": 1,
                    "committed_through": committed, "resume": if committed == 0 { "new" } else { "resumed" },
                    "limits": { "max_inflight_events": 512, "max_frame_bytes": 1_048_576, "heartbeat_interval_ms": 10_000 } }),
                )
            }
            "events" => {
                let mut st = state.lock().map_err(|_| anyhow::anyhow!("lock"))?;
                let committed = st.committed.entry(run.clone()).or_default();
                let mut file = events.lock().map_err(|_| anyhow::anyhow!("lock"))?;
                for e in frame["events"].as_array().into_iter().flatten() {
                    let seq = e["seq"].as_u64().unwrap_or(0);
                    if seq == *committed + 1 {
                        writeln!(file, "{}", json!({ "agent_run_id": run, "event": e }))?;
                        *committed = seq;
                    }
                }
                Some(json!({ "op": "ack", "through": *committed, "epoch": 1 }))
            }
            "telemetry" => {
                writeln!(
                    telemetry.lock().map_err(|_| anyhow::anyhow!("lock"))?,
                    "{text}"
                )?;
                Some(json!({ "op": "telemetry_ack", "through": frame["tseq_to"] }))
            }
            "bye" => {
                eprintln!("agent run {run} said bye");
                None
            }
            _ => None,
        };
        if let Some(r) = reply {
            ws.send(Message::text(r.to_string())).await?;
        }
    }
    Ok(())
}

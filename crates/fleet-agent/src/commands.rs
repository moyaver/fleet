// SPDX-License-Identifier: AGPL-3.0-only
//! `flush`, `check` and `version` (PLAN §6.1). Unlike `run`, `check` fails hard on problems: it
//! exists to find them before a real run.

use std::{
    fs::File,
    net::{SocketAddr, TcpStream, ToSocketAddrs},
    path::Path,
    process::Command,
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};

use anyhow::Context;
use fleet_protocol::journal::ClockSample;
use time::OffsetDateTime;

use crate::{
    outbox::{Outbox, read_spool},
    transport::{self, End, Shared},
};

/// Sends a spool left by a run whose agent died (`if: always()` post-step).
pub(crate) fn flush(spool: &Path, timeout: Duration) -> anyhow::Result<String> {
    let (agent_run_id, events) =
        read_spool(File::open(spool).with_context(|| format!("opening {}", spool.display()))?)?;
    let url = crate::observer::server_url().context("FLEET_SERVER is not set")?;
    let count = events.len();
    let mut outbox = Outbox::default();
    outbox.push(events);
    let shared = Arc::new(Shared {
        outbox: std::sync::Mutex::new(outbox),
        ..Shared::default()
    });
    shared.finishing.store(true, Ordering::SeqCst);
    let cfg = transport::Config {
        url,
        token: std::env::var("FLEET_TOKEN").ok().filter(|t| !t.is_empty()),
        agent_run_id,
        ci: crate::ci::context(|k| std::env::var(k).ok()),
    };
    let _ = rustls::crypto::ring::default_provider().install_default();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let t0 = Instant::now();
    let clock = move || ClockSample {
        wall: OffsetDateTime::now_utc(),
        mono_ms: u64::try_from(t0.elapsed().as_millis()).unwrap_or(u64::MAX),
    };
    let (_keep, rx) = tokio::sync::mpsc::channel(1);
    match rt.block_on(tokio::time::timeout(
        timeout,
        transport::run(cfg, shared, rx, clock),
    )) {
        Ok(End::Flushed) => Ok(format!("flushed {count} events of run {agent_run_id}")),
        Ok(End::Refused(why)) => anyhow::bail!("the server refused run {agent_run_id}: {why}"),
        Err(_) => anyhow::bail!("not every event was acked within {}s", timeout.as_secs()),
    }
}

/// One `check` result.
#[derive(Debug)]
pub(crate) struct Check {
    pub(crate) name: &'static str,
    pub(crate) ok: bool,
    pub(crate) detail: String,
}

/// Runs every check.
pub(crate) fn check() -> Vec<Check> {
    let mut out = Vec::new();
    let maestro = Command::new("maestro")
        .arg("--version")
        .env("MAESTRO_CLI_NO_ANALYTICS", "1")
        .output();
    out.push(match maestro {
        Ok(o) if o.status.success() => {
            let v = String::from_utf8_lossy(&o.stdout)
                .lines()
                .last()
                .unwrap_or("")
                .trim()
                .to_owned();
            let supported = v.starts_with(fleet_maestro::PROFILE);
            Check {
                name: "maestro",
                ok: true,
                detail: if supported {
                    format!("{v} (profile {})", fleet_maestro::PROFILE)
                } else {
                    format!(
                        "{v}: parsers target {}, results may degrade",
                        fleet_maestro::PROFILE
                    )
                },
            }
        }
        Ok(o) => Check {
            name: "maestro",
            ok: false,
            detail: format!("`maestro --version` exited {}", o.status),
        },
        Err(e) => Check {
            name: "maestro",
            ok: false,
            detail: format!("not on PATH: {e}"),
        },
    });
    out.push(tcp(
        "adb server",
        "127.0.0.1:5037",
        "adb server not listening (the agent never starts it; Maestro does)",
    ));
    out.push(match crate::observer::server_url() {
        None => Check {
            name: "server",
            ok: true,
            detail: "FLEET_SERVER not set: offline mode (journal and spool only)".into(),
        },
        Some(url) => {
            let host = url
                .split("://")
                .nth(1)
                .and_then(|r| r.split('/').next())
                .unwrap_or_default()
                .to_owned();
            let addr = if host.contains(':') {
                host
            } else if url.starts_with("wss") {
                format!("{host}:443")
            } else {
                format!("{host}:80")
            };
            tcp("server", &addr, "unreachable")
        }
    });
    let dir = std::env::var_os("FLEET_RUN_DIR").map_or_else(std::env::temp_dir, Into::into);
    let probe = dir.join(format!(".fleet-check-{}", std::process::id()));
    out.push(
        match std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&probe, b"")) {
            Ok(()) => {
                let _ = std::fs::remove_file(&probe);
                Check {
                    name: "run dir",
                    ok: true,
                    detail: format!("{} is writable", dir.display()),
                }
            }
            Err(e) => Check {
                name: "run dir",
                ok: false,
                detail: format!("{}: {e}", dir.display()),
            },
        },
    );
    out
}

fn tcp(name: &'static str, addr: &str, failure: &str) -> Check {
    let target: Option<SocketAddr> = addr.to_socket_addrs().ok().and_then(|mut a| a.next());
    match target.map(|a| TcpStream::connect_timeout(&a, Duration::from_secs(3))) {
        Some(Ok(_)) => Check {
            name,
            ok: true,
            detail: format!("{addr} reachable"),
        },
        Some(Err(e)) => Check {
            name,
            ok: false,
            detail: format!("{addr}: {failure} ({e})"),
        },
        None => Check {
            name,
            ok: false,
            detail: format!("{addr}: cannot resolve"),
        },
    }
}

/// `version` output.
pub(crate) fn version() -> String {
    format!(
        "{} {}\nprotocol {}\nmaestro profile {}\n",
        fleet_protocol::brand::AGENT_BIN,
        env!("CARGO_PKG_VERSION"),
        fleet_protocol::PROTOCOL_VERSION,
        fleet_maestro::PROFILE
    )
}

// SPDX-License-Identifier: AGPL-3.0-only
//! `flush`, `check` and `version` (PLAN §6.1). Unlike `run`, `check` fails hard on problems: it
//! exists to find them before a real run.

use std::{
    ffi::OsStr,
    io::Read,
    net::{SocketAddr, TcpStream, ToSocketAddrs},
    os::unix::process::CommandExt,
    path::Path,
    process::{Command, Stdio},
    sync::{Arc, atomic::Ordering, mpsc},
    thread,
    time::{Duration, Instant},
};

use anyhow::Context;
use fleet_protocol::journal::ClockSample;
use nix::{
    sys::signal::{Signal, killpg},
    unistd::Pid,
};
use time::OffsetDateTime;

use crate::{
    outbox::Outbox,
    transport::{self, End, Shared},
};

/// Sends a spool left by a run whose agent died (`if: always()` post-step).
pub(crate) fn flush(spool: &Path, timeout: Duration) -> anyhow::Result<String> {
    // The outbox reads the spool back as it sends: memory stays bounded however large it is.
    let (agent_run_id, outbox) =
        Outbox::replay(spool).with_context(|| format!("opening {}", spool.display()))?;
    let url = crate::observer::server_url().context("FLEET_SERVER is not set")?;
    let count = outbox.last_seq() - outbox.acked_through();
    let shared = Arc::new(Shared {
        outbox: std::sync::Mutex::new(outbox),
        ..Shared::default()
    });
    shared.finishing.store(true, Ordering::SeqCst);
    let cfg = transport::Config::from_env(
        url,
        agent_run_id,
        crate::ci::context(|k| std::env::var(k).ok()),
        |k| std::env::var(k).ok(),
    );
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
    out.push(
        match maestro_version(OsStr::new("maestro"), PROBE_TIMEOUT) {
            Ok(v) => {
                let supported = fleet_maestro::supports(&v);
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
            Err(detail) => Check {
                name: "maestro",
                ok: false,
                detail,
            },
        },
    );
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

/// How long `maestro --version` may take before the probe gives up.
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Asks `program` for its version with `--version`: the binary a run would start. Its output is
/// captured, never shown, and the probe is killed (with the group it started) past `timeout`.
/// The error is the reason the version is unknown.
pub(crate) fn maestro_version(program: &OsStr, timeout: Duration) -> Result<String, String> {
    let mut child = Command::new(program)
        .arg("--version")
        .env("MAESTRO_CLI_NO_ANALYTICS", "1")
        .env("MAESTRO_DISABLE_UPDATE_CHECK", "true")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        // Its own group: a timeout also kills the JVM a wrapper script started.
        .process_group(0)
        .spawn()
        .map_err(|e| format!("not on PATH: {e}"))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| "no stdout pipe".to_owned())?;
    // Read on a thread: a grandchild holding the pipe must not hold the probe past its timeout.
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut text = Vec::new();
        let _ = stdout.read_to_end(&mut text);
        let _ = tx.send(text);
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
            _ => {
                if let Ok(pid) = i32::try_from(child.id()) {
                    let _ = killpg(Pid::from_raw(pid), Signal::SIGKILL);
                }
                let _ = child.wait();
                return Err(format!(
                    "`maestro --version` did not finish within {timeout:?}"
                ));
            }
        }
    };
    if !status.success() {
        return Err(format!("`maestro --version` exited {status}"));
    }
    let text = rx.recv_timeout(Duration::from_secs(1)).unwrap_or_default();
    let version = String::from_utf8_lossy(&text)
        .lines()
        .last()
        .unwrap_or("")
        .trim()
        .to_owned();
    if version.is_empty() {
        return Err("`maestro --version` printed no version".to_owned());
    }
    Ok(version)
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

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    #[test]
    fn a_probe_that_hangs_is_killed_at_its_timeout() {
        let dir = std::env::temp_dir().join(format!("fleet-probe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("maestro");
        std::fs::write(&script, "#!/bin/sh\nsleep 30\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let started = Instant::now();
        let err = maestro_version(script.as_os_str(), Duration::from_millis(300)).unwrap_err();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(err.contains("did not finish within"), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
    }
}

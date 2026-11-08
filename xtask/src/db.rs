// SPDX-License-Identifier: AGPL-3.0-only
//! A throwaway Postgres 18 in Docker for `cargo xtask sqlx` and local `cargo xtask ci`
//! (PLAN §11.1). CI brings its own database through `DATABASE_URL`.

use std::{
    net::TcpListener,
    path::Path,
    process::Command,
    time::{Duration, Instant},
};

use anyhow::{Context, bail};

/// The image every Fleet test database uses (PLAN §5.5: Postgres 18).
const IMAGE: &str = "postgres:18";

/// A running container, removed on drop.
#[must_use = "the database is removed when this is dropped"]
pub(crate) struct Throwaway {
    name: String,
    pub(crate) url: String,
}

impl Drop for Throwaway {
    fn drop(&mut self) {
        // -v also removes the container's anonymous volumes (Postgres declares a VOLUME).
        let _ = Command::new("docker")
            .args(["rm", "-f", "-v", &self.name])
            .output();
    }
}

/// Starts Postgres 18 on a free local port and waits until it accepts TCP connections.
pub(crate) fn start() -> anyhow::Result<Throwaway> {
    start_named("fleet-xtask-pg")
}

/// [`start`], with the container named `<prefix>-<pid>`.
pub(crate) fn start_named(prefix: &str) -> anyhow::Result<Throwaway> {
    let port = TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
    let name = format!("{prefix}-{}", std::process::id());
    let status = Command::new("docker")
        .args([
            "run",
            "-d",
            "--rm",
            "--name",
            &name,
            "-e",
            "POSTGRES_USER=fleet",
            "-e",
            "POSTGRES_PASSWORD=fleet",
            "-e",
            "POSTGRES_DB=fleet",
            "-p",
            &format!("127.0.0.1:{port}:5432"),
            IMAGE,
        ])
        .stdout(std::process::Stdio::null())
        .status()
        .context("running docker (is Docker running?)")?;
    if !status.success() {
        bail!("docker run {IMAGE} failed: {status}");
    }
    let db = Throwaway {
        name,
        url: format!("postgres://fleet:fleet@127.0.0.1:{port}/fleet"),
    };
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        // Over TCP: the image's init phase listens on the Unix socket only.
        let ready = Command::new("docker")
            .args([
                "exec",
                &db.name,
                "pg_isready",
                "-h",
                "127.0.0.1",
                "-U",
                "fleet",
                "-d",
                "fleet",
            ])
            .output()
            .is_ok_and(|o| o.status.success());
        if ready {
            println!("postgres 18 ready at {}", db.url);
            return Ok(db);
        }
        if Instant::now() > deadline {
            bail!("postgres did not become ready within 90 s");
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Applies the migrations with sqlx-cli.
pub(crate) fn migrate(root: &Path, url: &str) -> anyhow::Result<()> {
    crate::run_env(
        root,
        "cargo",
        &["sqlx", "migrate", "run", "--source", "migrations"],
        &[("DATABASE_URL", url)],
    )
}

/// `cargo xtask sqlx`: regenerates the committed `.sqlx/` offline query data.
pub(crate) fn prepare(root: &Path) -> anyhow::Result<()> {
    let db = start()?;
    migrate(root, &db.url)?;
    crate::run_env(
        root,
        "cargo",
        &PREPARE,
        &[("DATABASE_URL", &db.url), ("SQLX_OFFLINE", "false")],
    )
}

/// `cargo sqlx prepare` over every target and feature, so test-only queries are covered too.
pub(crate) const PREPARE: [&str; 6] = [
    "sqlx",
    "prepare",
    "--workspace",
    "--",
    "--all-targets",
    "--all-features",
];

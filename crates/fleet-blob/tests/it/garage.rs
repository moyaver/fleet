// SPDX-License-Identifier: AGPL-3.0-only
//! The contract against Garage, the reference S3 store (PLAN D5, ADR-0007).
//!
//! Garage runs in Docker, started the way `xtask/src/db.rs` starts Postgres: `docker run`, a free
//! local port, removed on drop. It uses the reference deployment's own `garage.toml` and the
//! same `--single-node --default-bucket` flags as `deploy/compose`, so the test also proves that
//! configuration works. Without Docker the test fails with instructions: it is not skipped,
//! because S3 is the production backend.

use std::{
    net::TcpListener,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use fleet_blob::{BlobStore, S3, S3Config};

use crate::contract;

/// Garage v2.5.0, the version in `deploy/compose/docker-compose.yml`, pinned by manifest-list
/// digest so a re-tagged image cannot change what CI tests.
const IMAGE: &str =
    "dxflrs/garage@sha256:677e5b758e4425a325b811c425c026bf35a4bf42f7ddbaa773de3f4fd779a5a5";
const GARAGE_TOML: &str = include_str!("../../../../deploy/compose/garage.toml");
/// Garage wants `GK` + 24 hex digits and 64 hex digits; these are throwaway test credentials.
const ACCESS_KEY: &str = "GK000000000000000000000001";
const SECRET_KEY: &str = "0000000000000000000000000000000000000000000000000000000000000001";
const RPC_SECRET: &str = "0000000000000000000000000000000000000000000000000000000000000002";
const BUCKET: &str = "fleet-artifacts";

/// A running Garage container, removed on drop.
struct Garage {
    name: String,
    endpoint: String,
}

impl Drop for Garage {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["rm", "-f", &self.name])
            .output();
    }
}

fn docker(args: &[&str]) -> Result<(), String> {
    let out = Command::new("docker")
        .args(args)
        .stdout(Stdio::null())
        .output()
        .map_err(|e| {
            format!(
                "cannot run docker ({e}). The S3 half of the blob contract needs Docker: install \
                 and start it, or run only the filesystem half with \
                 `cargo nextest run -p fleet-blob -E 'not test(garage)'`"
            )
        })?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "docker {} failed: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&out.stderr)
        ))
    }
}

impl Garage {
    fn start() -> Result<Self, String> {
        let port = TcpListener::bind("127.0.0.1:0")
            .and_then(|l| l.local_addr())
            .map_err(|e| e.to_string())?
            .port();
        let name = format!("fleet-blob-garage-{}-{port}", std::process::id());
        let toml = std::env::temp_dir().join(format!("{name}.toml"));
        std::fs::write(&toml, GARAGE_TOML).map_err(|e| e.to_string())?;
        // Created before it is configured, so a failure below still removes it on drop.
        let garage = Self {
            endpoint: format!("http://127.0.0.1:{port}"),
            name,
        };
        let env = |k: &str, v: &str| format!("{k}={v}");
        docker(&[
            "create",
            "--name",
            &garage.name,
            "-e",
            &env("GARAGE_DEFAULT_ACCESS_KEY", ACCESS_KEY),
            "-e",
            &env("GARAGE_DEFAULT_SECRET_KEY", SECRET_KEY),
            "-e",
            &env("GARAGE_DEFAULT_BUCKET", BUCKET),
            "-e",
            &env("GARAGE_RPC_SECRET", RPC_SECRET),
            "-p",
            &format!("127.0.0.1:{port}:3900"),
            IMAGE,
            "/garage",
            "server",
            "--single-node",
            "--default-bucket",
        ])?;
        let copied = docker(&[
            "cp",
            toml.to_str().ok_or("temp path is not UTF-8")?,
            &format!("{}:/etc/garage.toml", garage.name),
        ]);
        let _ = std::fs::remove_file(&toml);
        copied?;
        docker(&["start", &garage.name])?;
        Ok(garage)
    }

    fn config(&self) -> S3Config {
        S3Config {
            endpoint: self.endpoint.clone(),
            region: "garage".into(),
            bucket: BUCKET.into(),
            access_key_id: ACCESS_KEY.into(),
            secret_access_key: SECRET_KEY.into(),
        }
    }

    /// Waits until the S3 API answers: Garage needs a few seconds to elect itself and lay out.
    async fn ready_store(&self) -> S3 {
        let store = S3::new(&self.config()).expect("S3 config should be valid");
        let probe = contract::key_of(contract::project(), b"readiness probe");
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            match store.head(&probe).await {
                Ok(_) => return store,
                Err(e) if Instant::now() > deadline => {
                    panic!("garage did not become ready within 90 s: {e:#?}")
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(500)).await,
            }
        }
    }
}

#[tokio::test]
async fn garage_meets_the_contract() {
    let garage = Garage::start().unwrap_or_else(|e| panic!("{e}"));
    let store = garage.ready_store().await;
    contract::run(&store).await;
}

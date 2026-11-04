// SPDX-License-Identifier: AGPL-3.0-only
//! Server configuration (research 03 §11): defaults, then a TOML file, then `FLEET_*`
//! environment variables (`__` separates sections: `FLEET_DATABASE__URL`), then `FLEET_*_FILE`
//! variables whose value is a path to read the setting from (Docker and Kubernetes secrets).
//!
//! Errors name the source and key of a bad value. Secrets never print: `check-config` shows
//! `<redacted>`.

use std::{net::SocketAddr, path::Path};

use anyhow::Context;
use figment::{
    Figment, Metadata, Profile, Provider,
    providers::{Format, Serialized, Toml},
    value::{Dict, Map, Value},
};
use serde::{Deserialize, Serialize};

/// A secret setting: deserializes from a string, never prints.
#[derive(Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    /// The value. Use only where the secret is needed.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

impl Serialize for Secret {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str("<redacted>")
    }
}

impl From<&str> for Secret {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

/// Everything the server reads at start-up.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Where to listen.
    pub listen: SocketAddr,
    /// Postgres.
    pub database: Database,
    /// The agent protocol.
    pub ingest: Ingest,
    /// Liveness thresholds (research 05 §6.8).
    pub liveness: Liveness,
    /// Projects to create at start-up (by slug); an admin UI arrives in M6.
    pub projects: Vec<ProjectConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([127, 0, 0, 1], 8080)),
            database: Database::default(),
            ingest: Ingest::default(),
            liveness: Liveness::default(),
            projects: Vec::new(),
        }
    }
}

/// When migrations run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MigrateMode {
    /// On start, under an advisory lock (the Compose default).
    Auto,
    /// Only by `fleet migrate`; `serve` refuses an outdated schema.
    Manual,
}

/// Postgres settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Database {
    /// `postgres://…` (set it with `FLEET_DATABASE__URL` or `FLEET_DATABASE__URL_FILE`).
    pub url: Option<Secret>,
    /// Pool size.
    pub max_connections: u32,
    /// When migrations run.
    pub migrate: MigrateMode,
}

impl Default for Database {
    fn default() -> Self {
        Self {
            url: None,
            max_connections: 20,
            migrate: MigrateMode::Auto,
        }
    }
}

/// Agent protocol limits and timers (research 05 §3.5, §3.8, §3.9).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Ingest {
    /// How often agents send a heartbeat.
    pub heartbeat_interval_ms: u64,
    /// WebSocket Ping cadence: below common 60 s proxy idle cuts.
    pub ping_interval_ms: u64,
    /// Close a socket with no inbound frame for this long.
    pub idle_timeout_ms: u64,
    /// How long a new socket may take to say `hello`.
    pub hello_timeout_ms: u64,
    /// Largest frame accepted.
    pub max_frame_bytes: u32,
    /// Events in flight per agent.
    pub max_inflight_events: u32,
    /// Telemetry checkpoint: buffered samples are written and acked this often (10–30 s).
    pub telemetry_flush_ms: u64,
    /// On shutdown: how long to drain sockets before closing them anyway.
    pub shutdown_grace_ms: u64,
    /// `goaway` asks agents to reconnect after a random delay up to this.
    pub reconnect_jitter_ms: u64,
}

impl Default for Ingest {
    fn default() -> Self {
        Self {
            heartbeat_interval_ms: 10_000,
            ping_interval_ms: 20_000,
            idle_timeout_ms: 45_000,
            hello_timeout_ms: 10_000,
            max_frame_bytes: 1024 * 1024,
            max_inflight_events: 512,
            telemetry_flush_ms: 15_000,
            shutdown_grace_ms: 15_000,
            reconnect_jitter_ms: 2_000,
        }
    }
}

/// Liveness thresholds (research 05 §6.8).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Liveness {
    /// A worker shows `stale` after this long without a frame (display only).
    pub stale_after_ms: u64,
    /// A detached run is abandoned after this long; its open flows become `unknown`.
    pub resume_grace_ms: u64,
    /// How often the leader sweeps.
    pub sweep_interval_ms: u64,
}

impl Default for Liveness {
    fn default() -> Self {
        Self {
            stale_after_ms: 25_000,
            resume_grace_ms: 15 * 60_000,
            sweep_interval_ms: 10_000,
        }
    }
}

/// A project created at start-up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectConfig {
    /// URL-safe identifier.
    pub slug: String,
    /// Display name.
    pub name: String,
}

impl Config {
    /// Loads the configuration from an optional TOML file and the given environment.
    pub fn load(file: Option<&Path>, env: &[(String, String)]) -> anyhow::Result<Self> {
        let mut figment = Figment::new().merge(Serialized::defaults(Self::default()));
        if let Some(path) = file {
            anyhow::ensure!(path.exists(), "config file {} not found", path.display());
            figment = figment.merge(Toml::file_exact(path));
        }
        let config: Self = figment
            .merge(EnvProvider::new(env)?)
            .extract()
            .context("reading the configuration")?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> anyhow::Result<()> {
        let i = &self.ingest;
        anyhow::ensure!(
            i.telemetry_flush_ms >= 100,
            "ingest.telemetry_flush_ms must be at least 100"
        );
        if !(10_000..=30_000).contains(&i.telemetry_flush_ms) {
            tracing::warn!(
                telemetry_flush_ms = i.telemetry_flush_ms,
                "telemetry checkpoints outside 10–30 s: more database load or later acks"
            );
        }
        anyhow::ensure!(
            i.idle_timeout_ms > i.heartbeat_interval_ms,
            "ingest.idle_timeout_ms must exceed ingest.heartbeat_interval_ms"
        );
        for p in &self.projects {
            anyhow::ensure!(
                p.slug.len() >= 2
                    && p.slug.len() <= 63
                    && p.slug
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                    && !p.slug.starts_with('-'),
                "project slug {:?} must be 2–63 of a-z, 0-9 and -",
                p.slug
            );
        }
        Ok(())
    }

    /// The effective configuration as JSON, secrets redacted (`check-config`).
    pub fn redacted(&self) -> anyhow::Result<String> {
        Ok(serde_json::to_string_pretty(self)?)
    }
}

/// `FLEET_*` variables from an explicit list, so loading is testable without touching the
/// process environment. `FLEET_X_FILE=/path` sets `x` to the file's contents.
struct EnvProvider {
    dict: Dict,
}

/// Variables that configure the binary, not the server.
const NOT_SETTINGS: &[&str] = &["FLEET_CONFIG", "FLEET_LOG"];

impl EnvProvider {
    fn new(env: &[(String, String)]) -> anyhow::Result<Self> {
        let mut dict = Dict::new();
        for (key, value) in env {
            let Some(name) = key.strip_prefix("FLEET_") else {
                continue;
            };
            if NOT_SETTINGS.contains(&key.as_str()) || name.is_empty() {
                continue;
            }
            let name = name.to_ascii_lowercase();
            let (path, value) = if let Some(stem) = name.strip_suffix("_file") {
                let contents = std::fs::read_to_string(value)
                    .with_context(|| format!("reading {key}={value}"))?;
                // Secrets stay strings, whatever they look like.
                (
                    stem.to_owned(),
                    Value::from(contents.trim_end_matches(['\n', '\r']).to_owned()),
                )
            } else {
                (
                    name,
                    value
                        .parse::<Value>()
                        .unwrap_or_else(|_| Value::from(value.clone())),
                )
            };
            insert(&mut dict, &path.split("__").collect::<Vec<_>>(), value);
        }
        Ok(Self { dict })
    }
}

fn insert(dict: &mut Dict, path: &[&str], value: Value) {
    match path {
        [] => {}
        [last] => {
            dict.insert((*last).to_owned(), value);
        }
        [first, rest @ ..] => {
            let entry = dict
                .entry((*first).to_owned())
                .or_insert_with(|| Value::from(Dict::new()));
            if entry.as_dict().is_none() {
                *entry = Value::from(Dict::new());
            }
            if let Value::Dict(_, inner) = entry {
                insert(inner, rest, value);
            }
        }
    }
}

impl Provider for EnvProvider {
    fn metadata(&self) -> Metadata {
        Metadata::named("FLEET_* environment variable")
    }

    fn data(&self) -> Result<Map<Profile, Dict>, figment::Error> {
        Ok(Profile::Default.collect(self.dict.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn layers_merge_in_order_and_secrets_come_from_files() {
        let dir = std::env::temp_dir().join(format!("fleet-config-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let toml = dir.join("fleet.toml");
        std::fs::write(
            &toml,
            "listen = \"0.0.0.0:9000\"\n[ingest]\nmax_inflight_events = 64\n\
             [[projects]]\nslug = \"app\"\nname = \"App\"\n",
        )
        .unwrap();
        let secret = dir.join("db");
        std::fs::write(&secret, "postgres://u:p@db/fleet\n").unwrap();
        let c = Config::load(
            Some(&toml),
            &env(&[
                ("FLEET_INGEST__MAX_INFLIGHT_EVENTS", "128"),
                ("FLEET_DATABASE__URL_FILE", secret.to_str().unwrap()),
                ("FLEET_LOG", "debug"),
                ("FLEET_JOB_KEY", "ignored by unknown-field tolerance"),
                ("PATH", "/bin"),
            ]),
        )
        .unwrap();
        assert_eq!(c.listen.port(), 9000, "from the file");
        assert_eq!(c.ingest.max_inflight_events, 128, "env beats the file");
        assert_eq!(c.ingest.idle_timeout_ms, 45_000, "default");
        assert_eq!(
            c.database.url.as_ref().map(Secret::expose),
            Some("postgres://u:p@db/fleet")
        );
        assert_eq!(c.projects.len(), 1);
        let shown = c.redacted().unwrap();
        assert!(
            shown.contains("<redacted>") && !shown.contains("u:p@db"),
            "{shown}"
        );
        assert!(!format!("{c:?}").contains("u:p@db"));
    }

    #[test]
    fn bad_values_name_their_source() {
        let err =
            Config::load(None, &env(&[("FLEET_INGEST__MAX_FRAME_BYTES", "lots")])).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("max_frame_bytes") && msg.contains("FLEET_"),
            "{msg}"
        );
        assert!(Config::load(Some(Path::new("/nonexistent/fleet.toml")), &[]).is_err());
        let bad_slug = Config::load(
            None,
            &env(&[("FLEET_PROJECTS", "[{slug=\"Bad Slug\",name=\"x\"}]")]),
        );
        assert!(bad_slug.is_err());
    }
}

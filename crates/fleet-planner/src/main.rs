// SPDX-License-Identifier: AGPL-3.0-only
//! Static, duration-aware Maestro shard planner (ADR-0013).
//!
//! `fleet-planner plan` finds the flows the way Maestro's workspace planner does (`config.yaml`
//! `flows` globs and tags), keeps each `executionOrder` sequence in one shard, balances the rest
//! Longest Processing Time first, and writes an immutable manifest and a CI matrix.
//!
//! Fleet trouble never stops a plan (invariant I2): history that can't be read falls back to the
//! supplied estimates or an even split, and a manifest that can't be published is a warning. A
//! workspace that plans to no flows, or to a shard with nothing in it, is an error, because a
//! job that runs nothing would look like a pass.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
    io::Write as _,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context as _, ensure};
use clap::{Args, Parser, Subcommand};
use fleet_protocol::plan::{PlanManifest, PlanShard};
use serde::{Deserialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};

/// Upper bound for one request to Fleet.
const TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Plans the shards of a suite and writes the manifest and the CI matrix.
    Plan(PlanArgs),
}

#[derive(Args)]
struct PlanArgs {
    /// The workspace `config.yaml`; its directory is the workspace.
    #[arg(long)]
    config: PathBuf,
    /// Number of shards, one CI job each.
    #[arg(long)]
    shards: u32,
    /// Where the immutable manifest goes.
    #[arg(long)]
    manifest: PathBuf,
    /// Where the GitHub Actions matrix goes.
    #[arg(long)]
    matrix: PathBuf,
    /// Seconds per flow, as a JSON object keyed by repository-relative flow path. Used when
    /// Fleet has no history for a flow.
    #[arg(long)]
    estimates: Option<PathBuf>,
    /// Shard capabilities and flow constraints, as JSON, keyed by repository-relative flow path.
    #[arg(long)]
    constraints: Option<PathBuf>,
    #[arg(long)]
    plan_id: Option<String>,
    /// Fleet server to publish the manifest to.
    #[arg(long)]
    server: Option<String>,
    #[arg(long, env = "FLEET_TOKEN")]
    token: Option<String>,
    /// Fleet server to read per-flow history from.
    #[arg(long, requires = "project")]
    history_server: Option<String>,
    #[arg(long)]
    project: Option<String>,
}

/// The part of a workspace `config.yaml` that selects and orders flows.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Config {
    #[serde(default)]
    flows: Vec<String>,
    #[serde(default)]
    include_tags: Vec<String>,
    #[serde(default)]
    exclude_tags: Vec<String>,
    execution_order: Option<ExecutionOrder>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExecutionOrder {
    flows_order: Vec<String>,
}

#[derive(Default, Deserialize)]
struct Constraints {
    #[serde(default)]
    shards: Vec<ShardConstraint>,
    #[serde(default)]
    flows: BTreeMap<String, FlowConstraint>,
}

#[derive(Deserialize)]
struct ShardConstraint {
    id: u32,
    #[serde(default)]
    capabilities: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FlowConstraint {
    #[serde(default)]
    requires: Vec<String>,
    exclusive_group: Option<String>,
}

/// Per-flow durations in whole seconds, from Fleet.
#[derive(Default)]
struct History {
    p50: BTreeMap<String, u64>,
    p90: BTreeMap<String, u64>,
}

fn main() -> anyhow::Result<()> {
    let Command::Plan(args) = Cli::parse().command;
    plan(args)
}

fn plan(args: PlanArgs) -> anyhow::Result<()> {
    ensure!(args.shards > 0, "shards must be positive");
    let config = std::fs::canonicalize(&args.config)
        .with_context(|| format!("reading config {}", args.config.display()))?;
    let root = config.parent().context("config path has no directory")?;
    let config_bytes = std::fs::read(&config)?;
    let parsed: Config = serde_yaml_ng::from_slice(&config_bytes)?;
    let constraints: Constraints = read_json(args.constraints.as_deref())?;
    let mut estimates: BTreeMap<String, u64> = read_json(args.estimates.as_deref())?;

    // Flow names are repository-relative, as Fleet records them and the agent runs them.
    let repo_root = root
        .ancestors()
        .find(|p| p.join(".git").exists())
        .unwrap_or(root);
    let prefix = root.strip_prefix(repo_root)?;
    let found: Vec<String> = discover(root, &config, &parsed)?
        .into_iter()
        .map(|flow| prefix.join(flow).to_string_lossy().into_owned())
        .collect();
    ensure!(
        !found.is_empty(),
        "no flows match the `flows` globs in {}",
        config.display()
    );

    let mut p90 = BTreeMap::new();
    if let Some(server) = &args.history_server {
        let project = args.project.as_deref().unwrap_or_default();
        match fetch_history(server, args.token.as_deref(), project, &found) {
            Ok(history) => {
                estimates.extend(history.p50);
                p90 = history.p90;
            }
            Err(e) => warn(&format!(
                "no history from Fleet, planning without it: {e:#}"
            )),
        }
    }

    let units = build_units(found, parsed.execution_order.as_ref(), &constraints);
    let shards = allocate(units, args.shards, &estimates, &p90, &constraints)?;
    let config_name = config.file_name().context("config path has no file name")?;

    let plan_id = match args.plan_id {
        Some(id) => id,
        None => format!(
            "plan-{}",
            hex(&Sha256::digest(serde_json::to_vec(&shards)?))
        ),
    };
    let plan = PlanManifest {
        plan_id,
        total_flows: Some(u32::try_from(
            shards.iter().map(|s| s.flows.len()).sum::<usize>(),
        )?),
        config_path: Some(prefix.join(config_name).to_string_lossy().into_owned()),
        config_sha256: Some(hex(&Sha256::digest(&config_bytes))),
        shards,
    };
    let manifest_bytes = serde_json::to_vec_pretty(&plan)?;
    let matrix_bytes = serde_json::to_vec_pretty(&serde_json::json!({
        "include": plan.shards.iter().map(|s| serde_json::json!({"shard": s.id})).collect::<Vec<_>>()
    }))?;
    ensure!(
        args.manifest != args.matrix,
        "manifest and matrix paths must differ"
    );
    for (path, bytes) in [
        (&args.manifest, &manifest_bytes),
        (&args.matrix, &matrix_bytes),
    ] {
        match std::fs::read(path) {
            Ok(existing) => ensure!(
                existing == *bytes,
                "{} already exists with different contents",
                path.display()
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    write_pair(&args.manifest, &manifest_bytes, &args.matrix, &matrix_bytes)?;

    if let Some(server) = &args.server
        && let Err(e) = publish(server, args.token.as_deref(), &plan)
    {
        warn(&format!(
            "plan {} was not published to Fleet, so its totals won't show there: {e:#}",
            plan.plan_id
        ));
    }
    Ok(())
}

/// The flow files Maestro would run for `config`, relative to its directory, sorted.
///
/// Mirrors Maestro's workspace planner: `flows` globs (default `*`, top level only, `!` negates),
/// only `*.yaml`/`*.yml` files that are not the config, then the header tags filter.
fn discover(root: &Path, config: &Path, parsed: &Config) -> anyhow::Result<Vec<String>> {
    let defaults = ["*".to_owned()];
    let patterns: &[String] = if parsed.flows.is_empty() {
        &defaults
    } else {
        &parsed.flows
    };
    let (negated, positive): (Vec<&String>, Vec<&String>) =
        patterns.iter().partition(|p| p.starts_with('!'));
    ensure!(
        !positive.is_empty(),
        "`flows` in {} has only negated globs",
        config.display()
    );
    let mut found = Vec::new();
    for pattern in positive {
        for path in glob::glob(&root.join(pattern).to_string_lossy())? {
            let path = path?;
            let is_flow = path.is_file()
                && path.extension().is_some_and(|e| e == "yaml" || e == "yml")
                && path.file_stem().and_then(|s| s.to_str()) != Some("config")
                && path != config;
            if is_flow {
                found.push(path.strip_prefix(root)?.to_string_lossy().into_owned());
            }
        }
    }
    for pattern in negated {
        let pattern = glob::Pattern::new(pattern.trim_start_matches('!'))?;
        found.retain(|flow| !pattern.matches(flow));
    }
    found.sort();
    found.dedup();

    if parsed.include_tags.is_empty() && parsed.exclude_tags.is_empty() {
        return Ok(found);
    }
    let mut selected = Vec::new();
    for flow in found {
        let tags = flow_tags(&root.join(&flow))?;
        let included =
            parsed.include_tags.is_empty() || parsed.include_tags.iter().any(|t| tags.contains(t));
        if included && !parsed.exclude_tags.iter().any(|t| tags.contains(t)) {
            selected.push(flow);
        }
    }
    Ok(selected)
}

/// Tags from a flow's header, the YAML before the first `---`.
fn flow_tags(path: &Path) -> anyhow::Result<Vec<String>> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("reading flow {}", path.display()))?;
    let header = contents.split("\n---").next().unwrap_or_default();
    let yaml: serde_yaml_ng::Value = serde_yaml_ng::from_str(header)
        .with_context(|| format!("reading the header of {}", path.display()))?;
    Ok(yaml
        .get("tags")
        .and_then(serde_yaml_ng::Value::as_sequence)
        .into_iter()
        .flatten()
        .filter_map(|t| t.as_str().map(str::to_owned))
        .collect())
}

/// Scheduling units: each `executionOrder` sequence is one unit, in its order, and every other
/// flow is a unit of its own. Units that share an exclusive group are merged.
///
/// Names in `flowsOrder` that match no selected flow are ignored, as Maestro ignores them.
fn build_units(
    mut found: Vec<String>,
    order: Option<&ExecutionOrder>,
    constraints: &Constraints,
) -> Vec<Vec<String>> {
    let mut units = Vec::new();
    if let Some(order) = order {
        let mut sequence: Vec<String> = Vec::new();
        for name in &order.flows_order {
            if let Some(path) = found.iter().find(|p| stem(p) == Some(name.as_str()))
                && !sequence.contains(path)
            {
                sequence.push(path.clone());
            }
        }
        found.retain(|p| !sequence.contains(p));
        if !sequence.is_empty() {
            units.push(sequence);
        }
    }
    units.extend(found.into_iter().map(|f| vec![f]));

    while let Some((i, j)) = shared_group(&units, constraints) {
        let merged = units.remove(j);
        units[i].extend(merged);
    }
    units
}

/// The first pair of units (by index) that contain flows from the same exclusive group.
fn shared_group(units: &[Vec<String>], constraints: &Constraints) -> Option<(usize, usize)> {
    let groups = |unit: &[String]| {
        unit.iter()
            .filter_map(|f| constraints.flows.get(f)?.exclusive_group.clone())
            .collect::<BTreeSet<_>>()
    };
    let sets: Vec<BTreeSet<String>> = units.iter().map(|u| groups(u)).collect();
    (0..sets.len())
        .flat_map(|i| ((i + 1)..sets.len()).map(move |j| (i, j)))
        .find(|&(i, j)| !sets[i].is_disjoint(&sets[j]))
}

fn stem(path: &str) -> Option<&str> {
    Path::new(path).file_stem().and_then(|s| s.to_str())
}

/// Longest Processing Time first: each unit goes to the eligible shard with the least estimated
/// work so far. A flow without an estimate counts as one second, which makes the split even.
///
/// Fails when a shard would get nothing, so no job is ever planned to run an empty shard.
fn allocate(
    mut units: Vec<Vec<String>>,
    shards: u32,
    estimates: &BTreeMap<String, u64>,
    p90: &BTreeMap<String, u64>,
    constraints: &Constraints,
) -> anyhow::Result<Vec<PlanShard>> {
    let unit_count = units.len();
    let cost = |unit: &[String]| {
        unit.iter()
            .map(|f| estimates.get(f).copied().unwrap_or(1))
            .sum::<u64>()
    };
    let tail = |unit: &[String]| {
        unit.iter()
            .map(|f| p90.get(f).copied().unwrap_or(0))
            .sum::<u64>()
    };
    units.sort_by(|a, b| {
        cost(b)
            .cmp(&cost(a))
            .then_with(|| tail(b).cmp(&tail(a)))
            .then_with(|| a.cmp(b))
    });

    let mut result: Vec<PlanShard> = (0..shards)
        .map(|id| PlanShard {
            id,
            estimated_seconds: Some(0),
            flows: Vec::new(),
        })
        .collect();
    for unit in units {
        let required: Vec<&String> = unit
            .iter()
            .filter_map(|f| constraints.flows.get(f))
            .flat_map(|f| &f.requires)
            .collect();
        let target = result
            .iter_mut()
            .filter(|s| {
                let capabilities = constraints.shards.iter().find(|c| c.id == s.id);
                required
                    .iter()
                    .all(|r| capabilities.is_some_and(|c| c.capabilities.contains(r)))
            })
            .min_by_key(|s| (s.estimated_seconds, s.id))
            .with_context(|| {
                format!("no shard has the capabilities {} requires", unit.join(", "))
            })?;
        target.estimated_seconds = Some(target.estimated_seconds.unwrap_or(0) + cost(&unit));
        target.flows.extend(unit);
    }
    if let Some(empty) = result.iter().find(|s| s.flows.is_empty()) {
        anyhow::bail!(
            "shard {} would run no flows: {unit_count} units cannot fill {shards} shards; lower --shards",
            empty.id
        );
    }
    Ok(result)
}

/// Reads durations for `flows` from Fleet. All or nothing: one failed request means no history,
/// so a plan never mixes measured and guessed durations.
fn fetch_history(
    server: &str,
    token: Option<&str>,
    project: &str,
    flows: &[String],
) -> anyhow::Result<History> {
    let token = token.context("--token or FLEET_TOKEN is required for history")?;
    let client = fleet_client(&api_url(server, &["api", "v1"])?)?;
    let mut history = History::default();
    for flow in flows {
        let url = api_url(
            server,
            &[
                "api",
                "v1",
                "projects",
                project,
                "flows",
                flow.as_str(),
                "stats",
            ],
        )?;
        let response = client.get(url).bearer_auth(token).send()?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            continue;
        }
        let json: serde_json::Value = response.error_for_status()?.json()?;
        if let Some(ms) = json["duration"]["p50_ms"].as_u64() {
            history.p50.insert(flow.clone(), ms.div_ceil(1000));
        }
        if let Some(ms) = json["duration"]["p90_ms"].as_u64() {
            history.p90.insert(flow.clone(), ms.div_ceil(1000));
        }
    }
    Ok(history)
}

fn publish(server: &str, token: Option<&str>, plan: &PlanManifest) -> anyhow::Result<()> {
    let token = token.context("--token or FLEET_TOKEN is required with --server")?;
    let url = api_url(server, &["api", "v1", "plans"])?;
    let client = fleet_client(&url)?;
    client
        .post(url)
        .bearer_auth(token)
        .json(&serde_json::json!({"manifest": plan}))
        .send()?
        .error_for_status()?;
    Ok(())
}

/// `server` with `segments` appended as path segments, each percent-encoded.
fn api_url(server: &str, segments: &[&str]) -> anyhow::Result<url::Url> {
    let mut url =
        url::Url::parse(server).with_context(|| format!("invalid Fleet server URL {server}"))?;
    url.path_segments_mut()
        .map_err(|()| anyhow::anyhow!("{server} can't carry an API path"))?
        .extend(segments);
    Ok(url)
}

/// The HTTP client for requests that carry the token. As in fleet-agent, the token goes only
/// over https, or in cleartext to a loopback host, which bypasses any proxy. Redirects are never
/// followed, so the token can't be sent on to another host.
fn fleet_client(url: &url::Url) -> anyhow::Result<reqwest::blocking::Client> {
    let loopback = match url.host() {
        Some(url::Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    };
    ensure!(
        url.scheme() == "https" || (url.scheme() == "http" && loopback),
        "credentials need https:// unless the host is loopback (got {})",
        url.host_str().unwrap_or("no host")
    );
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut builder = reqwest::blocking::Client::builder()
        .timeout(TIMEOUT)
        .redirect(reqwest::redirect::Policy::none());
    if url.scheme() == "http" {
        builder = builder.no_proxy();
    }
    Ok(builder.build()?)
}

/// Writes both files, or neither: each is staged under a temporary name first.
fn write_pair(first: &Path, a: &[u8], second: &Path, b: &[u8]) -> anyhow::Result<()> {
    let mut staged = Vec::new();
    let result = (|| -> anyhow::Result<()> {
        for (path, bytes) in [(first, a), (second, b)] {
            let name = path
                .file_name()
                .context("output path needs a filename")?
                .to_string_lossy();
            let temporary = path.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            staged.push((temporary, path));
            file.write_all(bytes)?;
            file.sync_all()?;
        }
        for (temporary, path) in &staged {
            std::fs::rename(temporary, path)?;
        }
        Ok(())
    })();
    for (temporary, _) in &staged {
        let _ = std::fs::remove_file(temporary);
    }
    result
}

/// Reads an optional JSON file; `None` gives the default.
fn read_json<T: DeserializeOwned + Default>(path: Option<&Path>) -> anyhow::Result<T> {
    match path {
        Some(path) => {
            let bytes =
                std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
            Ok(serde_json::from_slice(&bytes)?)
        }
        None => Ok(T::default()),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut hex, b| {
        let _ = write!(hex, "{b:02x}");
        hex
    })
}

fn warn(message: &str) {
    let _ = writeln!(
        std::io::stderr().lock(),
        "[fleet-planner] warning: {message}"
    );
}

// SPDX-License-Identifier: AGPL-3.0-only
//! Repository rules that rustc and clippy can't check (PLAN §3, §4.2).

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
    process::Command,
};

use anyhow::{Context, bail};
use serde_json::Value;

const SPDX_HEADER: &str = "// SPDX-License-Identifier: AGPL-3.0-only";

/// Directories never scanned: build output and VCS metadata.
const SKIP_DIRS: &[&str] = &[".git", "target", "dist", "book"];

/// I9: no Node.js, npm or hand-written JS/TS.
const FORBIDDEN_FILES: &[&str] = &[
    "package.json",
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
];
const FORBIDDEN_EXTENSIONS: &[&str] = &["js", "mjs", "cjs", "ts", "mts", "cts", "jsx", "tsx"];

/// I8: crate-name prefixes of cloud-provider and orchestrator SDKs.
const PROVIDER_PREFIXES: &[&str] = &[
    "aws-",
    "aws_",
    "google-cloud",
    "gcp_",
    "azure_",
    "azure-",
    "cloudflare",
    "kube",
    "k8s-",
];

/// PLAN §4.2 and ADR-0011 (Maestro knowledge stays in fleet-maestro, used only by the agent).
/// `internal` lists the workspace crates a crate may depend on (`None` = any);
/// `banned` lists crates that must not appear anywhere in its normal dependency tree when built
/// for `on`.
struct Rule {
    krate: &'static str,
    internal: Option<&'static [&'static str]>,
    forbidden_internal: &'static [&'static str],
    banned: &'static [&'static str],
    on: On,
}

/// Which build the `banned` check looks at. Browser crates only ship as wasm; some of their
/// dependencies pull in tokio for native builds only, which is fine.
#[derive(PartialEq)]
enum On {
    Host,
    Wasm,
    Both,
}

const WASM_TARGET: &str = "wasm32-unknown-unknown";

const NO_IO: &[&str] = &["tokio", "sqlx", "reqwest", "mio"];

const RULES: &[Rule] = &[
    Rule {
        krate: "fleet-protocol",
        internal: Some(&[]),
        forbidden_internal: &[],
        banned: NO_IO,
        on: On::Both,
    },
    Rule {
        krate: "fleet-core",
        internal: Some(&["fleet-protocol"]),
        forbidden_internal: &[],
        banned: NO_IO,
        on: On::Both,
    },
    Rule {
        krate: "fleet-maestro",
        internal: Some(&["fleet-protocol", "fleet-core"]),
        forbidden_internal: &[],
        banned: NO_IO,
        on: On::Host,
    },
    Rule {
        krate: "fleet-agent",
        internal: None,
        forbidden_internal: &["fleet-store", "fleet-server"],
        banned: &[],
        on: On::Host,
    },
    Rule {
        krate: "fleet-ui",
        internal: Some(&["fleet-protocol", "fleet-core"]),
        forbidden_internal: &[],
        banned: &["tokio"],
        on: On::Wasm,
    },
    Rule {
        krate: "fleet-web",
        internal: Some(&["fleet-protocol", "fleet-core", "fleet-ui"]),
        forbidden_internal: &[],
        banned: &["tokio"],
        on: On::Wasm,
    },
    Rule {
        krate: "fleet-store",
        internal: None,
        forbidden_internal: &["fleet-maestro", "fleet-agent", "fleet-server"],
        banned: &[],
        on: On::Host,
    },
    Rule {
        krate: "fleet-server",
        internal: None,
        forbidden_internal: &["fleet-agent", "fleet-web", "fleet-maestro"],
        banned: &[],
        on: On::Host,
    },
];

/// Library crates: they report errors with `thiserror` and leave logging setup to binaries
/// (docs/CONVENTIONS.md), so these never appear among their direct dependencies.
const LIBRARY_CRATES: &[&str] = &[
    "fleet-protocol",
    "fleet-core",
    "fleet-maestro",
    "fleet-store",
    "fleet-blob",
    "fleet-ui",
    "fleet-web-assets",
];
const BINARY_ONLY: &[&str] = &["anyhow", "tracing-subscriber"];

/// The only file allowed to opt out of `unsafe_code = "deny"`.
const UNSAFE_FILE: &str = "crates/fleet-agent/src/sys.rs";

/// External crates `fleet-protocol` may use directly (PLAN §4.2).
const PROTOCOL_EXTERNAL: &[&str] = &["serde", "serde_json", "uuid", "time", "schemars", "utoipa"];

pub(crate) fn run(root: &Path) -> anyhow::Result<()> {
    let mut problems = Vec::new();
    walk(root, root, &mut problems)?;
    problems.extend(workflow_problems(root)?);
    let host = String::from_utf8(output(root, "rustc", &["--print", "host-tuple"])?)?;
    problems.extend(dependency_problems(
        &metadata(root, host.trim())?,
        &metadata(root, WASM_TARGET)?,
    ));
    if problems.is_empty() {
        println!("xtask lint: ok");
        return Ok(());
    }
    for p in &problems {
        eprintln!("lint: {p}");
    }
    bail!("{} lint problem(s)", problems.len())
}

/// Workflows that may use `pull_request_target`: none that checks out a pull request.
const PULL_REQUEST_TARGET_OK: &[&str] = &[".github/workflows/cla.yml"];

/// Research 06 T1 and T19: a fork pull request never reaches a self-hosted runner, and no workflow
/// trusts a fork's code with secrets. `pull_request_target` is never used. A job that runs on a
/// self-hosted runner in a workflow a pull request can start must pick its runner on the pull
/// request's head repository being this one, so a fork gets a hosted runner (`ci.yml`'s
/// `agent-macos`).
fn workflow_problems(root: &Path) -> anyhow::Result<Vec<String>> {
    let dir = root.join(".github/workflows");
    let mut problems = Vec::new();
    for entry in fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("yml") {
            continue;
        }
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let text = fs::read_to_string(&path).with_context(|| format!("reading {name}"))?;
        problems.extend(fork_runner_problems(
            &format!(".github/workflows/{name}"),
            &text,
        ));
    }
    Ok(problems)
}

/// The fork rule for one workflow's text (see [`workflow_problems`]).
fn fork_runner_problems(file: &str, text: &str) -> Vec<String> {
    let mut problems = Vec::new();
    if text.contains("pull_request_target") {
        // The one workflow allowed it never checks out a pull request's code (its header says so).
        if !PULL_REQUEST_TARGET_OK.contains(&file) {
            problems.push(format!(
                "{file}: pull_request_target runs a fork's code with secrets"
            ));
        } else if text.contains("actions/checkout") {
            problems.push(format!(
                "{file}: pull_request_target checks out code, which a fork controls"
            ));
        }
    }
    let pull_request = text
        .lines()
        .any(|l| l.trim_start().starts_with("pull_request:"));
    let runs_here = text
        .lines()
        .filter(|l| !l.trim_start().starts_with('#') && l.contains("self-hosted"));
    for line in runs_here {
        let guarded = line
            .contains("github.event.pull_request.head.repo.full_name == github.repository")
            && line.contains("fromJSON(");
        if pull_request && !guarded {
            problems.push(format!(
                "{file}: a self-hosted runner in a workflow that pull requests start, not guarded on the head repository being this one: {}",
                line.trim()
            ));
        }
    }
    problems
}

fn walk(root: &Path, dir: &Path, problems: &mut Vec<String>) -> anyhow::Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .display()
            .to_string();
        if path.is_dir() {
            if !SKIP_DIRS.contains(&name.as_str()) {
                walk(root, &path, problems)?;
            }
            continue;
        }
        if let Some(p) = file_problem(&rel, &name, || {
            fs::read_to_string(&path).unwrap_or_default()
        }) {
            problems.push(p);
        }
    }
    Ok(())
}

/// Checks one file. `contents` is only read for Rust sources.
fn file_problem(rel: &str, name: &str, contents: impl FnOnce() -> String) -> Option<String> {
    let ext = name.rsplit_once('.').map(|(_, e)| e).unwrap_or_default();
    if FORBIDDEN_FILES.contains(&name) || FORBIDDEN_EXTENSIONS.contains(&ext) {
        return Some(format!(
            "{rel}: JavaScript/Node files are not allowed (invariant I9)"
        ));
    }
    if ext != "rs" {
        return None;
    }
    let text = contents();
    if text.lines().next() != Some(SPDX_HEADER) {
        return Some(format!("{rel}: first line must be `{SPDX_HEADER}`"));
    }
    // Built at runtime so this file doesn't match its own pattern.
    let opt_outs = ["allow", "expect"].map(|a| format!("{a}(unsafe_code"));
    if rel != UNSAFE_FILE && opt_outs.iter().any(|p| text.contains(p.as_str())) {
        return Some(format!(
            "{rel}: only {UNSAFE_FILE} may opt out of unsafe_code"
        ));
    }
    // T8 (research 06): user strings are text nodes. Raw HTML injection is banned in Rust
    // sources. Built at runtime so this file doesn't match its own pattern.
    let raw_html = ["inner", "_html"].concat();
    let raw_html_api = ["dangerously", "SetInnerHtml"].concat();
    if text.contains(&raw_html) || text.contains(&raw_html_api) {
        return Some(format!(
            "{rel}: raw HTML injection is not allowed; render user text as text nodes (research 06 T8)"
        ));
    }
    None
}

/// `cargo metadata` with the dependency graph resolved for one target.
fn metadata(root: &Path, target: &str) -> anyhow::Result<Value> {
    let args = [
        "metadata",
        "--format-version",
        "1",
        "--locked",
        "--filter-platform",
        target,
    ];
    Ok(serde_json::from_slice(&output(root, "cargo", &args)?)?)
}

fn output(dir: &Path, program: &str, args: &[&str]) -> anyhow::Result<Vec<u8>> {
    let out = Command::new(program)
        .args(args)
        .current_dir(dir)
        .output()
        .with_context(|| format!("running {program}"))?;
    if !out.status.success() {
        bail!("{program} failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(out.stdout)
}

/// Workspace crate names and normal (non-dev, non-build) dependency edges, by crate name.
type Graph = (BTreeSet<String>, BTreeMap<String, BTreeSet<String>>);

fn graph(meta: &Value) -> Graph {
    let name_of = |id: &str| -> String {
        meta["packages"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|p| p["id"] == id)
            .and_then(|p| p["name"].as_str())
            .unwrap_or(id)
            .to_owned()
    };
    let workspace: BTreeSet<String> = meta["workspace_members"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(&name_of)
        .collect();

    let mut edges: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for node in meta["resolve"]["nodes"].as_array().into_iter().flatten() {
        let from = name_of(node["id"].as_str().unwrap_or_default());
        let deps = node["deps"].as_array().into_iter().flatten().filter(|d| {
            d["dep_kinds"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|k| k["kind"].is_null())
        });
        let set = edges.entry(from).or_default();
        set.extend(deps.map(|d| name_of(d["pkg"].as_str().unwrap_or_default())));
    }
    (workspace, edges)
}

/// Applies `RULES`, the protocol allowlist and I8 to the host and wasm `cargo metadata` output.
fn dependency_problems(host: &Value, wasm: &Value) -> Vec<String> {
    let (workspace, edges) = graph(host);
    let (_, wasm_edges) = graph(wasm);
    let mut problems = Vec::new();
    for krate in &workspace {
        let direct = edges.get(krate).cloned().unwrap_or_default();
        let tree = closure(&edges, krate);

        for dep in &tree {
            if PROVIDER_PREFIXES.iter().any(|p| dep.starts_with(p)) {
                problems.push(format!(
                    "{krate}: depends on provider-specific crate `{dep}` (invariant I8)"
                ));
            }
        }
        if LIBRARY_CRATES.contains(&krate.as_str()) {
            for dep in direct.iter().filter(|d| BINARY_ONLY.contains(&d.as_str())) {
                problems.push(format!(
                    "{krate}: `{dep}` belongs in binaries only (docs/CONVENTIONS.md)"
                ));
            }
        }
        if krate == "fleet-protocol" {
            for dep in direct
                .iter()
                .filter(|d| !PROTOCOL_EXTERNAL.contains(&d.as_str()) && !workspace.contains(*d))
            {
                problems.push(format!(
                    "{krate}: `{dep}` is not in the protocol allowlist (PLAN §4.2)"
                ));
            }
        }
        let Some(rule) = RULES.iter().find(|r| r.krate == krate) else {
            continue;
        };
        for dep in direct.iter().filter(|d| workspace.contains(*d)) {
            let allowed = rule.internal.is_none_or(|ok| ok.contains(&dep.as_str()));
            if !allowed || rule.forbidden_internal.contains(&dep.as_str()) {
                problems.push(format!("{krate}: must not depend on `{dep}` (PLAN §4.2)"));
            }
        }
        let wasm_tree = closure(&wasm_edges, krate);
        let in_tree = |dep: &str| {
            (rule.on != On::Wasm && tree.contains(dep))
                || (rule.on != On::Host && wasm_tree.contains(dep))
        };
        for banned in rule.banned.iter().filter(|b| in_tree(b)) {
            problems.push(format!(
                "{krate}: `{banned}` is in its dependency tree (PLAN §4.2)"
            ));
        }
    }
    problems
}

fn closure(edges: &BTreeMap<String, BTreeSet<String>>, start: &str) -> BTreeSet<String> {
    let mut seen = BTreeSet::new();
    let mut stack: Vec<&str> = vec![start];
    while let Some(next) = stack.pop() {
        for dep in edges.get(next).into_iter().flatten() {
            if seen.insert(dep.clone()) {
                stack.push(dep);
            }
        }
    }
    seen
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_fork_pull_request_never_reaches_a_self_hosted_runner() {
        let guarded = "    runs-on: ${{ (github.event_name == 'push' || github.event.pull_request.head.repo.full_name == github.repository) && fromJSON('[\"self-hosted\"]') || 'macos-latest' }}";
        let ci = format!("on:\n  pull_request:\n  push:\njobs:\n  a:\n{guarded}\n");
        assert!(fork_runner_problems("ci.yml", &ci).is_empty());
        let unguarded =
            "on:\n  pull_request:\njobs:\n  a:\n    runs-on: [self-hosted, fleet-mac]\n";
        assert_eq!(fork_runner_problems("x.yml", unguarded).len(), 1);
        // A workflow that only runs on a schedule may use its own runner.
        let nightly = "on:\n  schedule:\n    - cron: '21 2 * * *'\njobs:\n  a:\n    runs-on: [self-hosted, fleet-mac]\n";
        assert!(fork_runner_problems("nightly.yml", nightly).is_empty());
        let target = "on:\n  pull_request_target:\njobs:\n  a:\n    runs-on: ubuntu-latest\n";
        assert_eq!(fork_runner_problems("t.yml", target).len(), 1);
        assert!(fork_runner_problems(".github/workflows/cla.yml", target).is_empty());
        let checkout = "on:\n  pull_request_target:\njobs:\n  a:\n    steps:\n      - uses: actions/checkout@v7\n";
        assert_eq!(
            fork_runner_problems(".github/workflows/cla.yml", checkout).len(),
            1
        );
    }

    #[test]
    fn files() {
        let ok = || format!("{SPDX_HEADER}\nfn main() {{}}\n");
        assert_eq!(file_problem("src/a.rs", "a.rs", ok), None);
        assert!(file_problem("src/a.rs", "a.rs", || "fn main() {}".into()).is_some());
        assert!(file_problem("web/app.ts", "app.ts", String::new).is_some());
        assert!(file_problem("package.json", "package.json", String::new).is_some());
        assert_eq!(
            file_problem("README.md", "README.md", || unreachable!()),
            None
        );
        let opt_out = || {
            format!(
                "{SPDX_HEADER}\n#![{}(unsafe_code, reason = \"x\")]\n",
                "expect"
            )
        };
        assert!(file_problem("crates/fleet-core/src/lib.rs", "lib.rs", opt_out).is_some());
        assert_eq!(file_problem(UNSAFE_FILE, "sys.rs", opt_out), None);
        // T8: raw HTML injection is refused wherever it is spelled (Leptos attribute or React).
        let inject = || {
            format!(
                "{SPDX_HEADER}\nview! {{ <p {}=\"x\"/> }}\n",
                ["inner", "_html"].concat()
            )
        };
        assert!(file_problem("crates/fleet-ui/src/a.rs", "a.rs", inject).is_some());
    }

    /// Builds minimal `cargo metadata` output from (crate, normal deps) pairs.
    fn meta(workspace: &[&str], graph: &[(&str, &[&str])]) -> Value {
        let packages: Vec<Value> = graph
            .iter()
            .map(|(n, _)| json!({"id": n, "name": n}))
            .collect();
        let nodes: Vec<Value> = graph
            .iter()
            .map(|(n, deps)| {
                let deps: Vec<Value> = deps
                    .iter()
                    .map(|d| json!({"pkg": d, "dep_kinds": [{"kind": null}]}))
                    .collect();
                json!({"id": n, "deps": deps})
            })
            .collect();
        json!({"packages": packages, "workspace_members": workspace, "resolve": {"nodes": nodes}})
    }

    #[test]
    fn dependency_rules() {
        let clean = meta(
            &["fleet-protocol", "fleet-core", "fleet-agent"],
            &[
                ("fleet-protocol", &["serde"]),
                ("fleet-core", &["fleet-protocol"]),
                ("fleet-agent", &["fleet-core", "tokio"]),
                ("serde", &[]),
                ("tokio", &[]),
            ],
        );
        assert!(
            dependency_problems(&clean, &clean).is_empty(),
            "{:?}",
            dependency_problems(&clean, &clean)
        );

        // Browser crates are judged on their wasm graph: native-only tokio is fine.
        let web_host = meta(&["fleet-web"], &[("fleet-web", &["tokio"]), ("tokio", &[])]);
        let web_wasm = meta(&["fleet-web"], &[("fleet-web", &[])]);
        assert_eq!(
            dependency_problems(&web_host, &web_wasm),
            Vec::<String>::new()
        );
        assert_ne!(
            dependency_problems(&web_wasm, &web_host),
            Vec::<String>::new()
        );

        let dirty = meta(
            &["fleet-protocol", "fleet-core", "fleet-agent", "fleet-store"],
            &[
                ("fleet-protocol", &["reqwest"]),
                ("fleet-core", &["helper"]),
                ("helper", &["tokio"]),
                ("fleet-agent", &["fleet-store", "aws-sdk-s3"]),
                ("fleet-store", &["anyhow"]),
                ("anyhow", &[]),
                ("reqwest", &[]),
                ("tokio", &[]),
                ("aws-sdk-s3", &[]),
            ],
        );
        let problems = dependency_problems(&dirty, &dirty).join("\n");
        assert!(
            problems.contains("fleet-protocol: `reqwest` is not in the protocol allowlist"),
            "{problems}"
        );
        assert!(
            problems.contains("fleet-core: `tokio` is in its dependency tree"),
            "{problems}"
        );
        assert!(
            problems.contains("fleet-agent: must not depend on `fleet-store`"),
            "{problems}"
        );
        assert!(
            problems.contains("fleet-agent: depends on provider-specific crate `aws-sdk-s3`"),
            "{problems}"
        );
        assert!(
            problems.contains("fleet-store: `anyhow` belongs in binaries only"),
            "{problems}"
        );
        assert!(
            !problems.contains("fleet-agent: `anyhow`"),
            "binaries may use anyhow"
        );
    }
}

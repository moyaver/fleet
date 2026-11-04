// SPDX-License-Identifier: LicenseRef-PolyForm-Internal-Use-1.0.0
//! Repository rules that rustc and clippy can't check (PLAN §3, §4.2).

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
    process::Command,
};

use anyhow::{Context, bail};
use serde_json::Value;

const SPDX_HEADER: &str = "// SPDX-License-Identifier: LicenseRef-PolyForm-Internal-Use-1.0.0";

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

/// External crates `fleet-protocol` may use directly (PLAN §4.2).
const PROTOCOL_EXTERNAL: &[&str] = &["serde", "serde_json", "uuid", "time", "schemars"];

pub fn run(root: &Path) -> anyhow::Result<()> {
    let mut problems = Vec::new();
    walk(root, root, &mut problems)?;
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
    if ext == "rs" && contents().lines().next() != Some(SPDX_HEADER) {
        return Some(format!("{rel}: first line must be `{SPDX_HEADER}`"));
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
                ("fleet-store", &[]),
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
    }
}

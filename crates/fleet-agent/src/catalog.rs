// SPDX-License-Identifier: AGPL-3.0-only
//! The workspace's flows, as the agent finds them before Maestro runs (issue #85). The agent reads
//! the flow files Maestro's workspace planner would select (the `flows:` globs of `config.yaml`,
//! else the top-level YAML files) and their headers, and sends the list in `hello`, so the server
//! knows flows that have not run yet.
//!
//! Bounded (invariant I10): at most [`MAX_CATALOG_FLOWS`] flows, a header read is at most
//! [`HEADER_BYTES`] of a file, the headers of one run read at most [`HEADER_BUDGET`] bytes in all,
//! and the glob scan visits at most [`MAX_CANDIDATES`] paths. Whatever is left out sets
//! `truncated`. Nothing here fails the run (invariant I2): an unreadable file is a flow without
//! tags, and a workspace that can't be read is an empty catalog.

use std::{
    io::Read as _,
    path::{Path, PathBuf},
};

use fleet_maestro::workspace::{flow_header, is_flow_file_name, workspace_config};
use fleet_protocol::frame::{
    CatalogFlow, FlowCatalog, MAX_CATALOG_FLOWS, MAX_CATALOG_PATH_BYTES, MAX_CATALOG_TAG_BYTES,
    MAX_CATALOG_TAGS,
};

/// Bytes read from the start of a flow file: its header, never its steps.
const HEADER_BYTES: u64 = 8 * 1024;
/// Bytes read from the headers of one run. Past it, the flows are listed without their headers.
const HEADER_BUDGET: u64 = 1024 * 1024;
/// Paths the glob scan visits at most.
const MAX_CANDIDATES: usize = 10_000;
/// The longest flow name kept, in bytes.
const MAX_NAME_BYTES: usize = 256;
/// Bytes read from a `config.yaml`. A config that is larger is not a Maestro workspace config.
const CONFIG_BYTES: u64 = 64 * 1024;

/// Finds the flows of the workspace at `workspace`. Paths are relative to `repo_root`, the checkout
/// the flows' runs name them from.
pub(crate) fn discover(workspace: &Path, repo_root: &Path) -> FlowCatalog {
    let globs = config_globs(workspace);
    let (negated, positive): (Vec<&str>, Vec<&str>) = if globs.is_empty() {
        (Vec::new(), vec!["*"])
    } else {
        let (negated, positive): (Vec<&String>, Vec<&String>) =
            globs.iter().partition(|g| g.starts_with('!'));
        (
            negated.iter().map(|g| g.trim_start_matches('!')).collect(),
            positive.iter().map(|g| g.as_str()).collect(),
        )
    };
    let mut truncated = false;
    let mut found = Vec::new();
    let mut scanned = 0_usize;
    'globs: for glob in positive {
        let pattern = Path::new(&glob::Pattern::escape(&workspace.to_string_lossy())).join(glob);
        let Ok(paths) = glob::glob(&pattern.to_string_lossy()) else {
            continue;
        };
        for path in paths.flatten() {
            scanned += 1;
            if scanned > MAX_CANDIDATES {
                truncated = true;
                break 'globs;
            }
            let file_name = path.file_name().map(|n| n.to_string_lossy().into_owned());
            if path.is_file()
                && file_name.is_some_and(|n| is_flow_file_name(&n))
                && let Ok(rel) = path.strip_prefix(workspace)
            {
                found.push(rel.to_path_buf());
            }
        }
    }
    let negated: Vec<glob::Pattern> = negated
        .iter()
        .filter_map(|g| glob::Pattern::new(g).ok())
        .collect();
    found.retain(|rel| !negated.iter().any(|p| p.matches_path(rel)));
    found.sort();
    found.dedup();

    let prefix = workspace.strip_prefix(repo_root).unwrap_or(Path::new(""));
    let mut budget = HEADER_BUDGET;
    let mut flows = Vec::new();
    for rel in found {
        let path = prefix.join(&rel).to_string_lossy().into_owned();
        if path.len() > MAX_CATALOG_PATH_BYTES {
            truncated = true;
            continue;
        }
        if flows.len() == MAX_CATALOG_FLOWS {
            truncated = true;
            break;
        }
        let (name, tags) = header_of(&workspace.join(&rel), &mut budget);
        flows.push(CatalogFlow { path, name, tags });
    }
    FlowCatalog { flows, truncated }
}

/// The `flows:` globs of the workspace's `config.yaml` (or `config.yml`), empty when it has none.
fn config_globs(workspace: &Path) -> Vec<String> {
    ["config.yaml", "config.yml"]
        .iter()
        .map(|name| workspace.join(name))
        .find(|p| p.is_file())
        .and_then(|config| read_prefix(&config, CONFIG_BYTES))
        .and_then(|text| workspace_config(&text))
        .map(|config| config.flows)
        .unwrap_or_default()
}

/// A flow file's name and tags, from its header; empty when the header is unreadable or the
/// budget is spent.
fn header_of(path: &Path, budget: &mut u64) -> (Option<String>, Vec<String>) {
    let want = HEADER_BYTES.min(*budget);
    if want == 0 {
        return (None, Vec::new());
    }
    let Some(text) = read_prefix(path, want) else {
        return (None, Vec::new());
    };
    *budget = budget.saturating_sub(want);
    let Some(header) = flow_header(&text) else {
        return (None, Vec::new());
    };
    let name = header.name.filter(|n| n.len() <= MAX_NAME_BYTES);
    let tags = header
        .tags
        .into_iter()
        .filter(|t| !t.is_empty() && t.len() <= MAX_CATALOG_TAG_BYTES)
        .take(MAX_CATALOG_TAGS)
        .collect();
    (name, tags)
}

/// The first `limit` bytes of a file as text. `None` when it can't be read.
fn read_prefix(path: &Path, limit: u64) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(limit).read_to_end(&mut bytes).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// The checkout a workspace belongs to: its nearest ancestor with a `.git`, else the workspace.
pub(crate) fn repo_root_of(workspace: &Path) -> PathBuf {
    workspace
        .ancestors()
        .find(|p| p.join(".git").exists())
        .map_or_else(|| workspace.to_path_buf(), Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory under the system temp dir, removed when dropped.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "fleet-catalog-{name}-{}-{}",
                std::process::id(),
                uuid::Uuid::now_v7()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn write(&self, rel: &str, text: &str) {
            let path = self.0.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn fixture(rel: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/maestro/2.12.1")
            .join(rel)
    }

    #[test]
    fn the_captured_workspace_yields_its_flow_and_not_its_config() {
        // The captured `flows` workspace holds `config.yaml` and `pass.yaml`; only the flow counts.
        let workspace = fixture("paths-config-dir-input/run/flows");
        let catalog = discover(&workspace, &workspace);
        assert_eq!(
            catalog,
            FlowCatalog {
                flows: vec![CatalogFlow {
                    path: "pass.yaml".into(),
                    name: Some("pass".into()),
                    tags: Vec::new(),
                }],
                truncated: false,
            }
        );
    }

    #[test]
    fn the_flows_globs_and_negations_of_the_config_select_the_flows_and_paths_are_repo_relative() {
        let s = Scratch::new("globs");
        s.write(
            "config.yaml",
            "flows:\n  - flows/*.yaml\n  - flows/*.yml\n  - '!flows/skip.yaml'\n",
        );
        s.write(
            "flows/a.yaml",
            "name: login\ntags:\n  - smoke\n---\n- back\n",
        );
        s.write("flows/skip.yaml", "name: skipped\n---\n- back\n");
        s.write("flows/b.yml", "appId: x\n---\n- back\n");
        s.write("flows/notes.md", "not a flow\n");
        s.write("top.yaml", "name: top\n---\n- back\n");
        let catalog = discover(&s.0, &s.0);
        let paths: Vec<&str> = catalog.flows.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["flows/a.yaml", "flows/b.yml"]);
        assert_eq!(catalog.flows[0].name.as_deref(), Some("login"));
        assert_eq!(catalog.flows[0].tags, ["smoke"]);
        assert!(!catalog.truncated);

        // With the checkout above the workspace, the same flows are named from it.
        let checkout = s.0.join("checkout");
        std::fs::create_dir_all(checkout.join(".git")).unwrap();
        s.write("checkout/app/config.yaml", "flows:\n  - flows/a.yaml\n");
        s.write("checkout/app/flows/a.yaml", "name: login\n---\n- back\n");
        let nested = discover(&checkout.join("app"), &checkout);
        assert_eq!(nested.flows[0].path, "app/flows/a.yaml");
    }

    #[test]
    fn a_workspace_with_more_flows_than_the_cap_lists_the_cap_and_says_so() {
        let s = Scratch::new("cap");
        for i in 0..=MAX_CATALOG_FLOWS {
            s.write(&format!("f{i:04}.yaml"), "name: f\n---\n- back\n");
        }
        let catalog = discover(&s.0, &s.0);
        assert_eq!(catalog.flows.len(), MAX_CATALOG_FLOWS);
        assert!(catalog.truncated);
        assert_eq!(catalog.flows[0].path, "f0000.yaml");
    }

    #[test]
    fn a_missing_workspace_is_an_empty_catalog_and_an_unreadable_header_is_a_flow_without_tags() {
        let s = Scratch::new("missing");
        assert!(discover(&s.0.join("nope"), &s.0).is_empty());
        s.write("broken.yaml", "name: [unclosed\n");
        let catalog = discover(&s.0, &s.0);
        assert_eq!(catalog.flows.len(), 1);
        assert_eq!(
            (catalog.flows[0].name.clone(), catalog.flows[0].tags.len()),
            (None, 0)
        );
    }
}

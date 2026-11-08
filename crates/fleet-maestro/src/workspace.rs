// SPDX-License-Identifier: AGPL-3.0-only
//! What a Maestro workspace says about its flows (issue #85): a flow file's header (`name:`,
//! `tags:`) and the `flows:` globs of `config.yaml`. Sans-I/O: the caller reads the files and
//! passes their text, so this module never touches the disk.
//!
//! Mirrors Maestro's workspace planner (`WorkspaceExecutionPlanner.kt`, 2.12.1): a flow is a
//! top-level `*.yaml` or `*.yml` file that is not the config, and a flow's header is the YAML before
//! the first `---`. Parsing never panics (invariant I2): anything unreadable reads as `None`.

use serde::Deserialize;

/// What a flow file's header says about it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FlowHeader {
    /// The `name:`, which Maestro matches `executionOrder` against.
    pub name: Option<String>,
    /// The `tags:`, in the order the header lists them. Non-text entries are dropped.
    pub tags: Vec<String>,
}

/// Reads a flow file's header from the text of the file (or its first bytes). `None` only when the
/// header is not YAML; an empty header, or one that is not a mapping, has neither a name nor tags.
#[must_use]
pub fn flow_header(text: &str) -> Option<FlowHeader> {
    let header = text.split("\n---").next().unwrap_or_default();
    let yaml: serde_yaml_ng::Value = serde_yaml_ng::from_str(header).ok()?;
    let Some(mapping) = yaml.as_mapping() else {
        return Some(FlowHeader::default());
    };
    let name = mapping
        .get("name")
        .and_then(serde_yaml_ng::Value::as_str)
        .map(str::to_owned);
    let tags = mapping
        .get("tags")
        .and_then(serde_yaml_ng::Value::as_sequence)
        .into_iter()
        .flatten()
        .filter_map(|t| t.as_str().map(str::to_owned))
        .collect();
    Some(FlowHeader { name, tags })
}

/// The part of a workspace `config.yaml` that selects flows.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceConfig {
    /// The `flows:` globs, relative to the workspace; empty means Maestro's default, `*`.
    #[serde(default)]
    pub flows: Vec<String>,
}

/// Reads the `flows:` globs from a `config.yaml`. `None` when the text is not a YAML mapping.
#[must_use]
pub fn workspace_config(text: &str) -> Option<WorkspaceConfig> {
    serde_yaml_ng::from_str(text).ok()
}

/// Whether a file name is a flow file: a `.yaml` or `.yml` file that is not `config`.
#[must_use]
pub fn is_flow_file_name(name: &str) -> bool {
    let path = std::path::Path::new(name);
    let extension_ok = path.extension().is_some_and(|e| e == "yaml" || e == "yml");
    extension_ok && path.file_stem().and_then(|s| s.to_str()) != Some("config")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_header_gives_its_name_and_tags_and_drops_non_text_tags() {
        let header =
            flow_header("name: login\ntags:\n  - smoke\n  - 3\n  - ios\n---\n- launchApp\n")
                .unwrap();
        assert_eq!(header.name.as_deref(), Some("login"));
        assert_eq!(header.tags, ["smoke", "ios"]);
    }

    #[test]
    fn a_header_without_name_or_tags_is_still_a_header() {
        let header = flow_header("appId: com.example\n---\n- back\n").unwrap();
        assert_eq!(header, FlowHeader::default());
    }

    #[test]
    fn a_header_that_is_not_yaml_is_none_and_one_that_is_not_a_mapping_has_nothing() {
        assert_eq!(
            flow_header("- just\n- a list\n"),
            Some(FlowHeader::default())
        );
        assert_eq!(flow_header("name: [unclosed\n"), None);
        assert_eq!(flow_header(""), Some(FlowHeader::default()));
    }

    #[test]
    fn the_flows_globs_come_from_the_config_and_default_to_none() {
        let config = workspace_config(
            "flows:\n  - flows/*.yaml\n  - '!flows/skip.yaml'\ntestOutputDir: out\n",
        )
        .unwrap();
        assert_eq!(config.flows, ["flows/*.yaml", "!flows/skip.yaml"]);
        assert_eq!(
            workspace_config("testOutputDir: out\n").unwrap().flows,
            Vec::<String>::new()
        );
        assert_eq!(workspace_config("- not a mapping\n"), None);
    }

    #[test]
    fn only_yaml_files_other_than_the_config_are_flows() {
        assert!(is_flow_file_name("login.yaml"));
        assert!(is_flow_file_name("checkout.yml"));
        assert!(!is_flow_file_name("config.yaml"));
        assert!(!is_flow_file_name("notes.md"));
        assert!(!is_flow_file_name("yaml"));
    }
}

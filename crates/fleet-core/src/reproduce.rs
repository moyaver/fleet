// SPDX-License-Identifier: AGPL-3.0-only
//! Rerunning failed flows (issue #82): the copyable command for one flow and the plan manifest
//! that reruns a CI run's failed flows with `fleet-agent run --plan` (ADR-0013).
//!
//! Only the agent reads Maestro's command line (`fleet-maestro`, which the server may not depend
//! on), so the agent sends the words of the command with the run, already redacted. This module
//! joins them with the flow's own path, and writes the manifest from the failed flows' paths.

use std::{borrow::Cow, collections::BTreeSet, sync::LazyLock};

use fleet_protocol::{
    event::Reproduction,
    plan::{PlanManifest, PlanShard},
    rest::ReproCommandView,
};

use crate::redact::Redactor;

/// The token-shape redaction, compiled once: every command passes through it.
static TOKEN_SHAPES: LazyLock<Redactor> = LazyLock::new(|| Redactor::new(Vec::<String>::new()));

/// Joins `words` into one line that a POSIX shell reads back as the same words. Words made of
/// letters, digits and `-_./:@%+,=` go unquoted; every other word is single-quoted.
#[must_use]
pub fn shell_join(words: &[String]) -> String {
    words
        .iter()
        .map(|w| shell_word(w))
        .collect::<Vec<_>>()
        .join(" ")
}

/// One word, quoted when it needs to be.
fn shell_word(word: &str) -> Cow<'_, str> {
    let plain = !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:@%+,=".contains(c));
    if plain && !word.starts_with('=') {
        Cow::Borrowed(word)
    } else {
        Cow::Owned(format!("'{}'", word.replace('\'', "'\\''")))
    }
}

/// How to rerun one flow: `reproduction` is the run's command words (`None` for a run that
/// recorded none), `flow_path` the flow's repository path. Never invents a command: a missing
/// part gives `command: None` and says which part it is.
///
/// The token-shape redaction runs over the finished line as well, so a token that reached the
/// words is still masked.
#[must_use]
pub fn command_for(
    reproduction: Option<&Reproduction>,
    flow_path: Option<&str>,
) -> ReproCommandView {
    let mut view = ReproCommandView {
        command: None,
        unavailable: None,
        left_out: Vec::new(),
    };
    let Some(reproduction) = reproduction else {
        view.unavailable = Some("This run did not record a command line Fleet can rerun.".into());
        return view;
    };
    view.left_out.clone_from(&reproduction.left_out);
    if let Some(reason) = &reproduction.unavailable {
        view.unavailable = Some(reason.clone());
        return view;
    }
    let Some(path) = flow_path.filter(|p| !p.is_empty()) else {
        view.unavailable = Some(
            "This flow has no repository path, so Fleet does not know which file to run.".into(),
        );
        return view;
    };
    if path.chars().any(char::is_control) {
        view.unavailable =
            Some("This flow's path has characters that a command line can't carry.".into());
        return view;
    }
    if reproduction.words.is_empty() {
        view.unavailable = Some("This run recorded no command words to rerun.".into());
        return view;
    }
    // A path that starts with a dash would read as an option: `./` keeps it a file.
    let file = if path.starts_with('-') {
        format!("./{path}")
    } else {
        path.to_owned()
    };
    let mut words = reproduction.words.clone();
    words.push(file);
    let line = shell_join(&words);
    view.command = Some(TOKEN_SHAPES.redact(&line).into_owned());
    view
}

/// A failed flow of a run, as the manifest needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedFlow {
    /// The repository-relative path, when Fleet knows it.
    pub path: Option<String>,
    /// The flow's name, for an error message.
    pub name: String,
}

/// Why a manifest can't be written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestError {
    /// These failed flows have no path, so the manifest could not rerun exactly them.
    NoPath(Vec<String>),
}

/// The plan manifest that reruns the failed flows of a pipeline attempt. Each run with a failed
/// flow is one shard, numbered from 0 in the order given, holding its failed flows' paths in that
/// order (a path repeated in one run counts once). Shards are the runs, so each one reruns on its
/// own job, as the original did. The manifest has no workspace config: a config's digest can't be
/// known here, and the agent refuses a plan with a config it can't verify.
///
/// # Errors
///
/// [`ManifestError::NoPath`] when a failed flow has no path. The manifest is never written
/// without it, so a rerun never silently skips a failed flow.
pub fn rerun_manifest(
    attempt: &str,
    runs: &[Vec<FailedFlow>],
) -> Result<PlanManifest, ManifestError> {
    let no_path: Vec<String> = runs
        .iter()
        .flatten()
        .filter(|f| f.path.is_none())
        .map(|f| f.name.clone())
        .collect();
    if !no_path.is_empty() {
        return Err(ManifestError::NoPath(no_path));
    }
    let shards: Vec<PlanShard> = runs
        .iter()
        .filter(|flows| !flows.is_empty())
        .enumerate()
        .map(|(id, flows)| {
            let mut seen = BTreeSet::new();
            let flows: Vec<String> = flows
                .iter()
                .filter_map(|f| f.path.clone())
                .filter(|p| seen.insert(p.clone()))
                .collect();
            PlanShard {
                id: u32::try_from(id).unwrap_or(u32::MAX),
                estimated_seconds: None,
                flows,
            }
        })
        .collect();
    let total: usize = shards.iter().map(|s| s.flows.len()).sum();
    Ok(PlanManifest {
        plan_id: format!("rerun-{attempt}"),
        total_flows: Some(u32::try_from(total).unwrap_or(u32::MAX)),
        config_path: None,
        config_sha256: None,
        equal_count_estimated_seconds: None,
        shards,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(words: &[&str]) -> Reproduction {
        Reproduction {
            words: words.iter().map(|w| (*w).to_owned()).collect(),
            left_out: Vec::new(),
            unavailable: None,
        }
    }

    #[test]
    fn plain_words_stay_bare_and_others_are_quoted() {
        assert_eq!(
            shell_join(&[
                "maestro".into(),
                "--device".into(),
                "emulator-5554".into(),
                "KEY=[REDACTED]".into(),
                "it's here".into(),
            ]),
            "maestro --device emulator-5554 'KEY=[REDACTED]' 'it'\\''s here'"
        );
    }

    #[test]
    fn the_flow_path_is_the_flows_own_and_comes_last() {
        let repro = words(&["maestro", "--device", "emulator-5554", "test"]);
        let view = command_for(Some(&repro), Some("flows/login.yaml"));
        assert_eq!(
            view.command.as_deref(),
            Some("maestro --device emulator-5554 test flows/login.yaml")
        );
        assert_eq!(view.unavailable, None);
    }

    #[test]
    fn a_flow_path_that_looks_like_an_option_is_made_a_file() {
        let repro = words(&["maestro", "test"]);
        let view = command_for(Some(&repro), Some("-odd.yaml"));
        assert_eq!(view.command.as_deref(), Some("maestro test ./-odd.yaml"));
    }

    #[test]
    fn a_missing_part_gives_no_command_and_says_which() {
        let none = command_for(None, Some("a.yaml"));
        assert_eq!(none.command, None);
        assert!(none.unavailable.is_some());

        let unavailable = Reproduction {
            words: Vec::new(),
            left_out: Vec::new(),
            unavailable: Some("not a maestro test".into()),
        };
        let view = command_for(Some(&unavailable), Some("a.yaml"));
        assert_eq!(view.command, None);
        assert_eq!(view.unavailable.as_deref(), Some("not a maestro test"));

        let no_path = command_for(Some(&words(&["maestro", "test"])), None);
        assert_eq!(no_path.command, None);
        assert!(
            no_path
                .unavailable
                .unwrap_or_default()
                .contains("repository path")
        );
    }

    #[test]
    fn a_token_shape_in_the_words_is_masked_in_the_command() {
        let repro = words(&[
            "maestro",
            "--device",
            "ghp_abcdefghijklmnopqrstuvwxyz0123",
            "test",
        ]);
        let view = command_for(Some(&repro), Some("a.yaml"));
        let command = view.command.unwrap_or_default();
        assert!(!command.contains("ghp_"), "{command}");
        assert!(command.contains("[REDACTED]"), "{command}");
    }

    #[test]
    fn the_manifest_has_one_shard_per_run_and_exactly_the_failed_flows() {
        let flow = |path: &str| FailedFlow {
            path: Some(path.to_owned()),
            name: path.to_owned(),
        };
        let runs = vec![
            vec![flow("a.yaml"), flow("b.yaml"), flow("a.yaml")],
            Vec::new(),
            vec![flow("c.yaml")],
        ];
        let m = rerun_manifest("att-1", &runs).unwrap();
        assert_eq!(m.plan_id, "rerun-att-1");
        assert_eq!(m.total_flows, Some(3));
        assert_eq!(m.config_path, None);
        let shards: Vec<(u32, Vec<&str>)> = m
            .shards
            .iter()
            .map(|s| (s.id, s.flows.iter().map(String::as_str).collect()))
            .collect();
        assert_eq!(
            shards,
            vec![(0, vec!["a.yaml", "b.yaml"]), (1, vec!["c.yaml"])]
        );
    }

    #[test]
    fn a_failed_flow_without_a_path_refuses_the_manifest() {
        let runs = vec![vec![
            FailedFlow {
                path: Some("a.yaml".into()),
                name: "a".into(),
            },
            FailedFlow {
                path: None,
                name: "ghost".into(),
            },
        ]];
        assert_eq!(
            rerun_manifest("att", &runs),
            Err(ManifestError::NoPath(vec!["ghost".into()]))
        );
    }
}

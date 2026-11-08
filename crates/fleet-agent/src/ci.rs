// SPDX-License-Identifier: AGPL-3.0-only
//! CI identity from the environment (research 06 §1, PLAN §6.7).
//!
//! Only the variables listed here are read. Job key: `FLEET_JOB_KEY` (docs recommend
//! `${{ job.check_run_id }}`), else `GITHUB_JOB` plus a hash of `FLEET_MATRIX` so matrix legs
//! don't collide. `FLEET_*` variables override every field, for other CIs.

use std::borrow::Cow;

use fleet_protocol::frame::{CiContext, CiProvider};

/// Makes text safe to echo to a CI log (research 06 T13). A runner reads a line that starts with
/// `::` as a workflow command (`::warning::`, `::add-mask::`, `::stop-commands::`), so on every
/// line of `text`, after its indentation, the leading colons are removed. Line breaks are both
/// `\n` and `\r`, because either can start a new command. Text with no `::` is returned as is.
///
/// Use it on what the agent prints itself, which can carry app or device text. Maestro's own
/// output is passed through byte for byte (I1) and is not rewritten.
#[must_use]
pub(crate) fn neutralize_workflow_commands(text: &str) -> Cow<'_, str> {
    if !text.contains("::") {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    for segment in text.split_inclusive(['\n', '\r']) {
        let indent = segment.len() - segment.trim_start().len();
        let (lead, rest) = segment.split_at(indent);
        out.push_str(lead);
        if rest.starts_with("::") {
            out.push_str(rest.trim_start_matches(':'));
        } else {
            out.push_str(rest);
        }
    }
    Cow::Owned(out)
}

/// Builds the CI context from an environment lookup.
pub(crate) fn context(env: impl Fn(&str) -> Option<String>) -> CiContext {
    let github = env("GITHUB_ACTIONS").as_deref() == Some("true");
    let pick = |fleet: &str, gh: &str| env(fleet).or_else(|| if github { env(gh) } else { None });
    let job_key = env("FLEET_JOB_KEY").unwrap_or_else(|| {
        let job = pick("FLEET_JOB", "GITHUB_JOB").unwrap_or_else(|| "job".into());
        match env("FLEET_MATRIX") {
            Some(m) if !m.trim().is_empty() && m.trim() != "null" => {
                format!("{job}#{:016x}", fnv1a(m.as_bytes()))
            }
            _ => job,
        }
    });
    CiContext {
        provider: if github {
            CiProvider::GithubActions
        } else {
            CiProvider::Generic
        },
        repository: pick("FLEET_REPOSITORY", "GITHUB_REPOSITORY"),
        run_id: pick("FLEET_RUN_ID", "GITHUB_RUN_ID"),
        run_attempt: pick("FLEET_RUN_ATTEMPT", "GITHUB_RUN_ATTEMPT").and_then(|a| a.parse().ok()),
        job_key,
        job_name: pick("FLEET_JOB_NAME", "GITHUB_JOB"),
        sha: pick("FLEET_SHA", "GITHUB_SHA"),
        git_ref: pick("FLEET_REF", "GITHUB_REF"),
        runner: pick("FLEET_RUNNER", "RUNNER_NAME"),
    }
}

/// A small stable hash (FNV-1a, 64-bit): the matrix JSON only needs a stable fingerprint.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[test]
    fn a_line_that_starts_with_a_workflow_command_is_neutralized() {
        assert_eq!(
            neutralize_workflow_commands("::set-env name=A::x\nok\n"),
            "set-env name=A::x\nok\n"
        );
        assert_eq!(
            neutralize_workflow_commands("  ::warning::hi"),
            "  warning::hi"
        );
        assert_eq!(neutralize_workflow_commands("a\r::error::x"), "a\rerror::x");
        assert_eq!(neutralize_workflow_commands("ok\r\n::x\r\n"), "ok\r\nx\r\n");
        assert_eq!(neutralize_workflow_commands(":::x"), "x");
        assert_eq!(neutralize_workflow_commands(":x and a::b"), ":x and a::b");
        assert!(
            matches!(neutralize_workflow_commands("plain"), Cow::Borrowed(_)),
            "no allocation when nothing changes"
        );
    }

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |k| map.get(k).cloned()
    }

    #[test]
    fn github_actions_with_matrix_legs() {
        let base = [
            ("GITHUB_ACTIONS", "true"),
            ("GITHUB_REPOSITORY", "o/r"),
            ("GITHUB_RUN_ID", "9"),
            ("GITHUB_RUN_ATTEMPT", "2"),
            ("GITHUB_JOB", "e2e"),
        ];
        let a = context(env(
            &[&base[..], &[("FLEET_MATRIX", r#"{"api":34}"#)]].concat()
        ));
        let b = context(env(
            &[&base[..], &[("FLEET_MATRIX", r#"{"api":35}"#)]].concat()
        ));
        assert_eq!(a.provider, CiProvider::GithubActions);
        assert_eq!(
            (a.repository.as_deref(), a.run_attempt),
            (Some("o/r"), Some(2))
        );
        assert_ne!(a.job_key, b.job_key, "matrix legs get distinct keys");
        assert!(a.job_key.starts_with("e2e#"));
        let explicit = context(env(&[&base[..], &[("FLEET_JOB_KEY", "12345")]].concat()));
        assert_eq!(explicit.job_key, "12345");
    }

    #[test]
    fn generic_ci_reads_only_fleet_variables() {
        let c = context(env(&[
            ("GITHUB_REPOSITORY", "ignored/when-not-actions"),
            ("FLEET_REPOSITORY", "o/r"),
        ]));
        assert_eq!(c.provider, CiProvider::Generic);
        assert_eq!(c.repository.as_deref(), Some("o/r"));
        assert_eq!(c.job_key, "job");
    }
}

// SPDX-License-Identifier: AGPL-3.0-only
//! Maestro's secret rules (invariant I6, research 08), behind the runner adapter.
//!
//! `fleet-core` redacts with a generic engine: a [`Redactor`] built from the secret values it is
//! given, and an argv walker whose flags the caller names. This module names Maestro's flags and
//! variables: `-e KEY=VALUE` and `--env KEY=VALUE` carry flow data, and every `MAESTRO_*`
//! variable except the ones that configure the CLI is passed into flows, so it is a secret.

use fleet_core::redact::{Redactor, is_secret_name, redact_argv as redact_words};

use crate::invocation::{is_boolean_flag, is_safe_value_flag};

/// `MAESTRO_*` variables that configure the CLI rather than carry flow data.
fn is_maestro_setting(upper: &str) -> bool {
    upper.starts_with("MAESTRO_CLI_")
        || upper == "MAESTRO_DISABLE_UPDATE_CHECK"
        || upper == "MAESTRO_DRIVER_STARTUP_TIMEOUT"
}

/// Secret values for a Maestro run: `-e KEY=VALUE` / `--env KEY=VALUE` arguments, every
/// `MAESTRO_*` variable that carries flow data, and variables whose names look secret.
#[must_use]
pub fn secrets_from<'a>(
    argv: &[String],
    env: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = argv.iter();
    while let Some(arg) = rest.next() {
        let value = match arg.as_str() {
            "-e" | "--env" => rest.next().map(String::as_str),
            a => a.strip_prefix("--env=").or_else(|| a.strip_prefix("-e=")),
        };
        if let Some((_, v)) = value.and_then(|kv| kv.split_once('=')) {
            out.push(v.to_owned());
        }
    }
    for (name, value) in env {
        let upper = name.to_ascii_uppercase();
        let maestro_flow_data = upper.starts_with("MAESTRO_") && !is_maestro_setting(&upper);
        if maestro_flow_data || is_secret_name(&upper) {
            out.push(value.to_owned());
        }
    }
    out
}

/// A redactor for a Maestro run, built from its argv and the environment it runs with.
#[must_use]
pub fn redactor_for_run<'a>(
    argv: &[String],
    env: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Redactor {
    Redactor::new(secrets_from(argv, env))
}

/// The argv of a Maestro command with its secrets replaced: the values of flags nobody has
/// classified, and of `-e`/`--env` (which keep their key), never leave the host (research 06 T5).
#[must_use]
pub fn redact_argv(argv: &[String]) -> Vec<String> {
    redact_words(argv, is_safe_value_flag, is_boolean_flag, |flag| {
        matches!(flag, "-e" | "--env")
    })
}

#[cfg(test)]
mod tests {
    use fleet_core::redact::MARKER;

    use super::*;

    #[test]
    fn finds_secrets_in_argv_and_env() {
        let argv: Vec<String> = [
            "maestro",
            "test",
            "-e",
            "PASSWORD=hunter22",
            "--env=PIN=1234",
            "-e",
            "X=1",
            "flow.yaml",
        ]
        .map(String::from)
        .to_vec();
        let env = [
            ("MAESTRO_FLEET_TOKEN", "abcd-efgh"),
            ("MAESTRO_CLI_NO_ANALYTICS", "1"),
            ("HOME", "/home/me"),
            ("API_KEY", "k3y-value"),
        ];
        let s = secrets_from(&argv, env);
        assert!(s.contains(&"hunter22".to_owned()) && s.contains(&"1234".to_owned()));
        assert!(s.contains(&"abcd-efgh".to_owned()) && s.contains(&"k3y-value".to_owned()));
        assert!(!s.contains(&"/home/me".to_owned()), "HOME is not a secret");
        let r = Redactor::new(s);
        assert_eq!(
            r.redact("input hunter22 then 1"),
            "input [REDACTED] then 1",
            "short values are left alone"
        );
    }

    #[test]
    fn a_maestro_env_value_is_redacted_and_its_key_kept() {
        let argv: Vec<String> = ["maestro", "test", "-e", "PIN=1234", "--env=TOKEN=abc"]
            .map(String::from)
            .to_vec();
        assert_eq!(
            redact_argv(&argv),
            [
                "maestro",
                "test",
                "-e",
                "PIN=[REDACTED]",
                "--env=TOKEN=[REDACTED]"
            ]
            .map(String::from)
            .to_vec()
        );
        let unknown: Vec<String> = ["maestro", "test", "--mystery", "hunter22-value"]
            .map(String::from)
            .to_vec();
        assert_eq!(redact_argv(&unknown)[3], MARKER);
    }
}

// SPDX-License-Identifier: AGPL-3.0-only
//! The rerun command of a run's `maestro test` line (issue #82).
//!
//! The agent builds it from the redacted argv it sends with the run, and the server joins the
//! flow's own path to it (`fleet-core`'s `reproduce`). Four kinds of word are told apart:
//!
//! - kept options, whose values are shown: `--device`, `--udid`, `-p`/`--platform`, `--config`
//!   and `--verbose`/`--no-ansi`;
//! - env flags (`-e`/`--env`): the key is shown and the value is always `[REDACTED]`, whatever
//!   the argv holds, so a short value that the redactor kept is not shown either;
//! - options Fleet owns or that select flows (the output locations the agent adds, sharding, tag
//!   filters, continuous mode): dropped, because a rerun of one named flow must not inherit them;
//! - anything else is an unknown flag: its name goes in `left_out`, and its value (which might be
//!   a secret) is never read.
//!
//! Positional words (the flow paths of the original run) are dropped. The server adds the flow's
//! own path, so a rerun runs that flow and no other.

use fleet_core::redact::MARKER;
use fleet_protocol::event::Reproduction;

/// Options whose value is shown in the command.
const KEPT_WITH_VALUE: &[&str] = &["--device", "--udid", "-p", "--platform", "--config"];

/// Options with no value that are shown in the command.
const KEPT_FLAGS: &[&str] = &["--verbose", "--no-ansi"];

/// Options Fleet drops on purpose. Those that take a value are listed in [`TAKES_VALUE`].
const DROPPED: &[&str] = &[
    "--format",
    "--output",
    "--debug-output",
    "--test-output-dir",
    "--flatten-debug-output",
    "--shard-split",
    "--shard-all",
    "--shards",
    "-s",
    "--include-tags",
    "--exclude-tags",
    "-c",
    "--continuous",
    "--test-suite-name",
];

/// The dropped options that take a value, whose value must be consumed so it is not misread.
const TAKES_VALUE: &[&str] = &[
    "--format",
    "--output",
    "--debug-output",
    "--test-output-dir",
    "--shard-split",
    "--shard-all",
    "--shards",
    "-s",
    "--include-tags",
    "--exclude-tags",
    "--test-suite-name",
];

/// The command words for `argv`, which is the redacted argv of a run (`argv[0]` is the program).
///
/// Returns `unavailable` (and no words) when the program is not `maestro` or the command has no
/// `test`: Fleet then gives no rerun command, rather than guessing one.
#[must_use]
pub fn reproduction(argv: &[String]) -> Reproduction {
    let program = argv.first().map_or("", |p| basename(p));
    if program != "maestro" && program != "maestro.bat" {
        return unavailable("the run did not start `maestro`, so Fleet can't tell how to rerun it");
    }
    let mut words = vec!["maestro".to_owned()];
    let mut left_out = Vec::new();
    let mut test_seen = false;
    let mut i = 1;
    while let Some(arg) = argv.get(i) {
        i += 1;
        if arg == "--" {
            break;
        }
        let (name, inline) = match arg.split_once('=') {
            Some((name, value)) => (name, Some(value.to_owned())),
            None => (arg.as_str(), None),
        };
        if !name.starts_with('-') || name == "-" {
            // The subcommand, or a flow path or a value: only the first `test` is kept.
            if name == "test" && !test_seen {
                test_seen = true;
                words.push("test".to_owned());
            }
            continue;
        }
        if KEPT_WITH_VALUE.contains(&name) {
            match take_value(argv, &mut i, inline) {
                Some(value) => words.extend([name.to_owned(), value]),
                None => left_out.push(name.to_owned()),
            }
        } else if KEPT_FLAGS.contains(&name) {
            words.push(name.to_owned());
        } else if name == "-e" || name == "--env" {
            let key = take_value(argv, &mut i, inline)
                .and_then(|kv| kv.split_once('=').map(|(key, _)| key.to_owned()))
                .filter(|key| is_env_name(key));
            match key {
                Some(key) => words.extend([name.to_owned(), format!("{key}={MARKER}")]),
                None => left_out.push(name.to_owned()),
            }
        } else if DROPPED.contains(&name) || is_short_cluster(name) {
            if TAKES_VALUE.contains(&name) {
                let _ = take_value(argv, &mut i, inline);
            }
        } else {
            left_out.push(name.to_owned());
        }
    }
    if !test_seen {
        return unavailable("the run's command has no `test`, so Fleet can't tell how to rerun it");
    }
    Reproduction {
        words,
        left_out,
        unavailable: None,
    }
}

fn unavailable(reason: &str) -> Reproduction {
    Reproduction {
        words: Vec::new(),
        left_out: Vec::new(),
        unavailable: Some(reason.to_owned()),
    }
}

/// The value of an option: the text after `=`, else the next argument (consumed).
fn take_value(argv: &[String], i: &mut usize, inline: Option<String>) -> Option<String> {
    if inline.is_some() {
        return inline;
    }
    let value = argv.get(*i).cloned();
    if value.is_some() {
        *i += 1;
    }
    value
}

/// A shell-safe environment variable name.
fn is_env_name(key: &str) -> bool {
    let mut chars = key.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// `-ch`-style clusters of Maestro's short boolean flags (`-c`, `-h`).
fn is_short_cluster(name: &str) -> bool {
    name.len() > 2 && name.starts_with('-') && name.chars().skip(1).all(|c| c == 'c' || c == 'h')
}

/// The file name of a program path (`/opt/maestro/bin/maestro` is `maestro`).
fn basename(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn the_captured_features_command_keeps_the_device_and_the_env_key_only() {
        // fixtures/maestro/2.12.1/features: the agent's output flags and the planted `-e` value.
        let r = reproduction(&argv(
            "/opt/maestro/bin/maestro --device emulator-5600 test --format junit \
             --output run/report.xml --flatten-debug-output --debug-output run/debug \
             -e PASSWORD=fleet-planted-e-secret-7f3a flows/features.yaml",
        ));
        assert_eq!(r.unavailable, None);
        assert_eq!(
            r.words,
            [
                "maestro",
                "--device",
                "emulator-5600",
                "test",
                "-e",
                "PASSWORD=[REDACTED]"
            ]
        );
        assert_eq!(r.left_out, Vec::<String>::new());
    }

    #[test]
    fn an_env_value_is_never_shown_even_when_short() {
        let r = reproduction(&argv(
            "maestro test -e PIN=12 -eUSER=bob --env=TOKEN=abcdef a.yaml",
        ));
        assert_eq!(
            r.words,
            [
                "maestro",
                "test",
                "-e",
                "PIN=[REDACTED]",
                "--env",
                "TOKEN=[REDACTED]"
            ]
        );
        // `-eUSER=bob` is not a form Maestro reads as an env flag here, so it is an unknown name.
        assert_eq!(r.left_out, ["-eUSER"]);
    }

    #[test]
    fn a_secret_in_an_env_or_an_unknown_flag_never_reaches_the_words() {
        let r = reproduction(&argv(
            "maestro test --api-key=fleet-planted-api-123456 --cookie fleet-planted-cookie-654321 \
             -e PASSWORD=fleet-planted-env-abcdef --verbose a.yaml",
        ));
        let all = format!("{:?}{:?}", r.words, r.left_out);
        for secret in [
            "fleet-planted-api-123456",
            "fleet-planted-cookie-654321",
            "fleet-planted-env-abcdef",
        ] {
            assert!(!all.contains(secret), "{secret} leaked: {all}");
        }
        assert_eq!(
            r.words,
            ["maestro", "test", "-e", "PASSWORD=[REDACTED]", "--verbose"]
        );
        assert_eq!(r.left_out, ["--api-key", "--cookie"]);
    }

    #[test]
    fn owned_and_selecting_options_are_dropped_with_their_values() {
        let r = reproduction(&argv(
            "maestro test --shard-split 2 --include-tags smoke --format junit --output r.xml \
             --debug-output d --flatten-debug-output -c a.yaml b.yaml",
        ));
        assert_eq!(r.words, ["maestro", "test"]);
        assert_eq!(r.left_out, Vec::<String>::new());
    }

    #[test]
    fn device_config_and_inline_values_are_kept_in_order() {
        let r = reproduction(&argv(
            "maestro --device=emulator-5554 test --config cfg/config.yaml --no-ansi x.yaml",
        ));
        assert_eq!(
            r.words,
            [
                "maestro",
                "--device",
                "emulator-5554",
                "test",
                "--config",
                "cfg/config.yaml",
                "--no-ansi"
            ]
        );
    }

    #[test]
    fn an_env_flag_without_a_usable_name_is_left_out_by_name() {
        let r = reproduction(&argv("maestro test -e 'a b=c' -e"));
        assert_eq!(r.words, ["maestro", "test"]);
        assert_eq!(r.left_out, ["-e", "-e"]);
    }

    #[test]
    fn a_run_that_is_not_maestro_test_has_no_command() {
        let r = reproduction(&argv("./gradlew connectedCheck"));
        assert_eq!(r.words, Vec::<String>::new());
        assert!(r.unavailable.is_some());
        let no_test = reproduction(&argv("maestro --version"));
        assert!(no_test.unavailable.is_some());
        assert!(reproduction(&[]).unavailable.is_some());
    }
}

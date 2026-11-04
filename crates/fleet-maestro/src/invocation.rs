// SPDX-License-Identifier: AGPL-3.0-only
//! Reading and adjusting the user's `maestro …` command line (PLAN §6.3, research 08 §7).
//!
//! The agent never removes a user flag. In `auto` mode it adds only what it needs when the user
//! didn't set it: one flattened debug output directory and a JUnit report, plus environment
//! variables that make the log machine-readable and keep Maestro quiet.

use std::path::Path;

use fleet_protocol::event::ShardMode;

use crate::log::MACHINE_PATTERN;

/// What the command line asks for.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Invocation {
    /// Index of the `test` subcommand in argv.
    pub test_index: Option<usize>,
    /// `--device`/`--udid` values (comma-separated lists split).
    pub devices: Vec<String>,
    /// `--format`.
    pub format: Option<String>,
    /// `--output`.
    pub output: Option<String>,
    /// `--debug-output`.
    pub debug_output: Option<String>,
    /// `--flatten-debug-output`.
    pub flatten: bool,
    /// `--test-output-dir`.
    pub test_output_dir: Option<String>,
    /// Sharding mode.
    pub shard_mode: Option<ShardMode>,
    /// Shard count.
    pub shards: u32,
}

/// Options of `maestro` and `maestro test` that take a value (so the value isn't a flow path).
const VALUE_OPTIONS: &[&str] = &[
    "--device",
    "--udid",
    "-p",
    "--platform",
    "--format",
    "--output",
    "--debug-output",
    "--test-output-dir",
    "--shard-split",
    "--shard-all",
    "--shards",
    "-e",
    "--env",
    "--include-tags",
    "--exclude-tags",
    "--config",
    "--test-suite-name",
    "--host",
    "--port",
    "--api-url",
    "--api-key",
    "--app-file",
];

/// Reads a `maestro …` argv (`argv[0]` is the program).
#[must_use]
pub fn parse(argv: &[String]) -> Invocation {
    let mut inv = Invocation {
        shards: 1,
        ..Invocation::default()
    };
    let mut i = 1;
    while let Some(arg) = argv.get(i) {
        let (name, inline) = arg
            .split_once('=')
            .map_or((arg.as_str(), None), |(n, v)| (n, Some(v)));
        let takes_value = VALUE_OPTIONS.contains(&name);
        let value = if takes_value {
            inline
                .map(str::to_owned)
                .or_else(|| argv.get(i + 1).cloned())
        } else {
            None
        };
        match name {
            "test" if inv.test_index.is_none() => inv.test_index = Some(i),
            "--device" | "--udid" => inv.devices.extend(
                value
                    .iter()
                    .flat_map(|v| v.split(','))
                    .map(|d| d.trim().to_owned()),
            ),
            "--format" => inv.format.clone_from(&value),
            "--output" => inv.output.clone_from(&value),
            "--debug-output" => inv.debug_output.clone_from(&value),
            "--test-output-dir" => inv.test_output_dir.clone_from(&value),
            "--flatten-debug-output" => inv.flatten = true,
            "--shard-split" | "--shards" => {
                inv.shard_mode = Some(ShardMode::Split);
                inv.shards = value.as_deref().and_then(|v| v.parse().ok()).unwrap_or(1);
            }
            "--shard-all" => {
                inv.shard_mode = Some(ShardMode::All);
                inv.shards = value.as_deref().and_then(|v| v.parse().ok()).unwrap_or(1);
            }
            _ => {}
        }
        i += if takes_value && inline.is_none() {
            2
        } else {
            1
        };
    }
    inv
}

/// The command line and environment the agent actually runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prepared {
    /// argv with the agent's flags inserted after `test`.
    pub argv: Vec<String>,
    /// Environment variables to set when the user hasn't.
    pub env: Vec<(String, String)>,
    /// Flags the agent added, for the run log.
    pub added_flags: Vec<String>,
    /// Where the session log and flow bundles will be.
    pub debug_dir: Option<String>,
    /// Where the JUnit report will be.
    pub report: Option<String>,
}

/// Adds the agent's flags and environment (`rewrite` = `--fleet-rewrite=auto`).
#[must_use]
pub fn prepare(
    argv: &[String],
    run_dir: &Path,
    rewrite: bool,
    user_env: impl Fn(&str) -> bool,
) -> Prepared {
    let inv = parse(argv);
    let mut out = argv.to_vec();
    let mut added = Vec::new();
    let mut debug_dir = match (&inv.debug_output, inv.flatten) {
        (Some(d), true) => Some(d.clone()),
        _ => None,
    };
    let mut report = inv.output.clone().filter(|_| {
        inv.format
            .as_deref()
            .is_some_and(|f| f.eq_ignore_ascii_case("junit"))
    });
    if rewrite && let Some(t) = inv.test_index {
        if inv.debug_output.is_none() && inv.test_output_dir.is_none() {
            let dir = run_dir.join("artifacts").to_string_lossy().into_owned();
            added.extend([
                "--flatten-debug-output".to_owned(),
                "--debug-output".to_owned(),
                dir.clone(),
            ]);
            debug_dir = Some(dir);
        }
        if inv.format.is_none() {
            let file = run_dir.join("report.xml").to_string_lossy().into_owned();
            added.extend([
                "--format".to_owned(),
                "junit".to_owned(),
                "--output".to_owned(),
                file.clone(),
            ]);
            report = Some(file);
        }
        for (k, flag) in added.iter().enumerate() {
            out.insert(t + 1 + k, flag.clone());
        }
    }
    let env = [
        ("MAESTRO_CLI_NO_ANALYTICS", "1"),
        ("MAESTRO_DISABLE_UPDATE_CHECK", "true"),
        ("MAESTRO_CLI_ANALYSIS_NOTIFICATION_DISABLED", "true"),
        ("MAESTRO_CLI_LOG_PATTERN_FILE", MACHINE_PATTERN),
    ]
    .into_iter()
    .filter(|(k, _)| !user_env(k))
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect();
    Prepared {
        argv: out,
        env,
        added_flags: added,
        debug_dir,
        report,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn reads_flags_values_and_shards() {
        let i = parse(&argv(
            "maestro --device A,B test --shard-split=2 -e K=V --format junit --output r.xml flows/",
        ));
        assert_eq!(i.test_index, Some(3));
        assert_eq!(i.devices, ["A", "B"]);
        assert_eq!((i.shard_mode, i.shards), (Some(ShardMode::Split), 2));
        assert_eq!(i.output.as_deref(), Some("r.xml"));
    }

    #[test]
    fn adds_only_what_is_missing() {
        let run = Path::new("/run");
        let p = prepare(&argv("maestro test flows/"), run, true, |_| false);
        assert_eq!(
            p.argv,
            argv(
                "maestro test --flatten-debug-output --debug-output /run/artifacts --format junit --output /run/report.xml flows/"
            )
        );
        assert_eq!(
            (p.debug_dir.as_deref(), p.report.as_deref()),
            (Some("/run/artifacts"), Some("/run/report.xml"))
        );
        assert!(
            p.env
                .iter()
                .any(|(k, v)| k == "MAESTRO_CLI_LOG_PATTERN_FILE" && v == MACHINE_PATTERN)
        );

        let user = argv("maestro test --format html --test-output-dir out flows/");
        let p = prepare(&user, run, true, |k| k == "MAESTRO_CLI_NO_ANALYTICS");
        assert_eq!(p.argv, user, "the user chose both; nothing is added");
        assert!(
            !p.env.iter().any(|(k, _)| k == "MAESTRO_CLI_NO_ANALYTICS"),
            "user env wins"
        );

        let p = prepare(&argv("maestro test flows/"), run, false, |_| false);
        assert_eq!(p.argv, argv("maestro test flows/"), "rewrite off");
    }
}

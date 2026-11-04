// SPDX-License-Identifier: AGPL-3.0-only
//! Reading and adjusting the user's `maestro …` command line (PLAN §6.3, research 08 §7).
//!
//! The agent never removes a user flag. In `auto` mode it adds only what it needs when the user
//! didn't set it: one flattened debug output directory, plus environment variables that make the
//! log machine-readable and keep Maestro quiet. It also adds a JUnit report, but only where that
//! cannot change Maestro's execution mode (see [`ReportInjection`]).
//!
//! `--format` is not a passive flag: Maestro's `TestCommand` runs its multi-flow path whenever a
//! report format is asked for, and that path rejects `-c`/`--continuous`. So a single flow, or a
//! continuous run, is never given `--format` (issue #28).

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
    /// Flow files or directories given after `test` (not flags, not flag values).
    pub positionals: Vec<String>,
    /// `-c`/`--continuous` (also inside a short-flag cluster such as `-ch`).
    pub continuous: bool,
}

/// What a flow path on the command line is on disk (the caller looks; this crate does no I/O).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathKind {
    /// A single flow file.
    File,
    /// A directory of flows.
    Dir,
    /// Doesn't exist, or can't be told.
    Unknown,
}

/// Whether the agent added `--format junit --output …`, and if not, why not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportInjection {
    /// Added: Maestro runs its multi-flow path for this command anyway.
    Injected,
    /// Not added: a single flow, where `--format` would switch Maestro to its multi-flow path.
    SkippedSingleFlow,
    /// Not added: `-c`/`--continuous`, which the multi-flow path rejects.
    SkippedContinuous,
    /// Not added: flow paths couldn't be classified (missing or unreadable), so the mode is unknown.
    SkippedUnclassified,
    /// Not added: the user chose a `--format` already.
    UserProvided,
    /// Not added: rewrite mode is off, or there is no `test` subcommand.
    Disabled,
}

impl ReportInjection {
    /// A short stable label, for logs.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Injected => "injected",
            Self::SkippedSingleFlow => "skipped_single_flow",
            Self::SkippedContinuous => "skipped_continuous",
            Self::SkippedUnclassified => "skipped_unclassified",
            Self::UserProvided => "user_provided",
            Self::Disabled => "disabled",
        }
    }
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
    "-s",
    "--screen-size",
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
            "--shard-split" | "--shards" | "-s" => {
                inv.shard_mode = Some(ShardMode::Split);
                inv.shards = value.as_deref().and_then(|v| v.parse().ok()).unwrap_or(1);
            }
            "--shard-all" => {
                inv.shard_mode = Some(ShardMode::All);
                inv.shards = value.as_deref().and_then(|v| v.parse().ok()).unwrap_or(1);
            }
            "--" if inv.test_index.is_some() => {
                inv.positionals.extend(argv.iter().skip(i + 1).cloned());
                break;
            }
            "-c" | "--continuous" => inv.continuous = true,
            _ if is_short_cluster(name) => inv.continuous |= name.contains('c'),
            _ if inv.test_index.is_some() && !arg.starts_with('-') => {
                inv.positionals.push(arg.clone());
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

/// `-ch`-style clusters of Maestro's two short boolean flags (`-c`, `-h`).
fn is_short_cluster(name: &str) -> bool {
    name.len() > 2 && name.starts_with('-') && name.chars().skip(1).all(|c| c == 'c' || c == 'h')
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
    /// Whether the JUnit report was added, and if not, why.
    pub report_injection: ReportInjection,
}

/// Decides whether `--format` can be added without changing Maestro's execution mode.
///
/// Maestro takes its multi-flow path for more than one flow file or a directory, and for
/// `--shard-all` (replication). `flows` is every flow path the run will get, `classify` says
/// what each is. Anything unknown means no injection.
fn report_decision(
    inv: &Invocation,
    flows: &[String],
    classify: &impl Fn(&str) -> PathKind,
) -> ReportInjection {
    if inv.format.is_some() {
        return ReportInjection::UserProvided;
    }
    if inv.continuous {
        return ReportInjection::SkippedContinuous;
    }
    let kinds: Vec<PathKind> = flows.iter().map(|f| classify(f)).collect();
    if kinds.is_empty() || kinds.contains(&PathKind::Unknown) {
        return ReportInjection::SkippedUnclassified;
    }
    // UNVERIFIED against Maestro's source: `--shard-all N` (N > 1) replicates even one flow,
    // which the issue says takes the multi-flow path.
    let replicating = inv.shard_mode == Some(ShardMode::All) && inv.shards > 1;
    if kinds.len() > 1 || kinds.contains(&PathKind::Dir) || replicating {
        ReportInjection::Injected
    } else {
        ReportInjection::SkippedSingleFlow
    }
}

/// Adds the agent's flags and environment (`rewrite` = `--fleet-rewrite=auto`).
///
/// `extra_flows` are flow paths the agent will append later (a planner shard); `classify` tells
/// files from directories.
#[must_use]
pub fn prepare(
    argv: &[String],
    run_dir: &Path,
    rewrite: bool,
    extra_flows: &[String],
    classify: impl Fn(&str) -> PathKind,
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
    let mut injection = ReportInjection::Disabled;
    if inv.format.is_some() && rewrite && inv.test_index.is_some() {
        injection = ReportInjection::UserProvided;
    }
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
        let flows: Vec<String> = inv.positionals.iter().chain(extra_flows).cloned().collect();
        injection = report_decision(&inv, &flows, &classify);
        if injection == ReportInjection::Injected {
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
        report_injection: injection,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    /// Names ending in `/` are directories, `.yaml` are files, the rest don't exist.
    fn dirs(p: &str) -> PathKind {
        if p.ends_with('/') {
            PathKind::Dir
        } else if p.contains(".yaml") {
            PathKind::File
        } else {
            PathKind::Unknown
        }
    }

    fn inject(cmd: &str, extra: &[String]) -> (ReportInjection, Vec<String>) {
        let p = prepare(&argv(cmd), Path::new("/run"), true, extra, dirs, |_| false);
        (p.report_injection, p.argv)
    }

    #[test]
    fn a_single_flow_keeps_maestros_mode() {
        let (why, out) = inject("maestro test login.yaml", &[]);
        assert_eq!(why, ReportInjection::SkippedSingleFlow);
        assert!(
            !out.iter().any(|a| a == "--format" || a == "--output"),
            "{out:?}"
        );
        let (why, _) = inject("maestro --device A test login.yaml", &[]);
        assert_eq!(why, ReportInjection::SkippedSingleFlow);
    }

    #[test]
    fn continuous_never_gets_a_format() {
        for cmd in [
            "maestro test -c login.yaml",
            "maestro test --continuous login.yaml",
            "maestro test -ch login.yaml",
            "maestro -c test flows/",
            "maestro test flows/ --continuous",
        ] {
            let (why, out) = inject(cmd, &[]);
            assert_eq!(why, ReportInjection::SkippedContinuous, "{cmd}");
            assert!(!out.iter().any(|a| a == "--format"), "{cmd}");
        }
        // `-c` as an option's value isn't the flag.
        let (why, _) = inject("maestro test -e K -c flows/", &[]);
        assert_eq!(why, ReportInjection::SkippedContinuous);
        let (why, _) = inject("maestro test --config -c flows/", &[]);
        assert_eq!(why, ReportInjection::Injected);
    }

    #[test]
    fn suites_get_a_report() {
        let (why, out) = inject("maestro test flows/", &[]);
        assert_eq!(why, ReportInjection::Injected);
        assert!(out.iter().any(|a| a == "--format"));
        let (why, _) = inject("maestro test a.yaml b.yaml", &[]);
        assert_eq!(why, ReportInjection::Injected);
        let (why, _) = inject("maestro test --shard-all 2 a.yaml", &[]);
        assert_eq!(why, ReportInjection::Injected);
        let (why, _) = inject("maestro test", &["a.yaml".into(), "b.yaml".into()]);
        assert_eq!(why, ReportInjection::Injected, "planner flows count");
        let (why, _) = inject("maestro test", &["a.yaml".into()]);
        assert_eq!(why, ReportInjection::SkippedSingleFlow);
    }

    #[test]
    fn unknown_paths_and_user_formats_are_left_alone() {
        let (why, _) = inject("maestro test missing", &[]);
        assert_eq!(why, ReportInjection::SkippedUnclassified);
        let (why, _) = inject("maestro test a.yaml missing", &[]);
        assert_eq!(why, ReportInjection::SkippedUnclassified);
        let (why, out) = inject("maestro test --format html flows/", &[]);
        assert_eq!(why, ReportInjection::UserProvided);
        assert!(!out.iter().any(|a| a == "--output"));
        let p = prepare(
            &argv("maestro test flows/"),
            Path::new("/r"),
            false,
            &[],
            dirs,
            |_| false,
        );
        assert_eq!(p.report_injection, ReportInjection::Disabled);
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
        assert_eq!(i.positionals, ["flows/"]);
        assert!(!i.continuous);
    }

    #[test]
    fn finds_flow_paths_but_not_option_values() {
        let i = parse(&argv(
            "maestro --device emulator-5554 test --device=B -e A=B --config c.yaml --format junit --output out/r.xml --debug-output dbg -s 2 --headless a.yaml flows/",
        ));
        assert_eq!(i.positionals, ["a.yaml", "flows/"]);
        let i = parse(&argv("maestro test --include-tags smoke -- x.yaml"));
        assert_eq!(i.positionals, ["x.yaml"], "everything after -- is a path");
        assert_eq!(
            parse(&argv("maestro test --device d -e K=V"))
                .positionals
                .len(),
            0
        );
        assert!(
            parse(&argv("maestro login.yaml")).positionals.is_empty(),
            "no test"
        );
    }

    #[test]
    fn adds_only_what_is_missing() {
        let run = Path::new("/run");
        let p = prepare(&argv("maestro test flows/"), run, true, &[], dirs, |_| {
            false
        });
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
        let p = prepare(&user, run, true, &[], dirs, |k| {
            k == "MAESTRO_CLI_NO_ANALYTICS"
        });
        assert_eq!(p.argv, user, "the user chose both; nothing is added");
        assert!(
            !p.env.iter().any(|(k, _)| k == "MAESTRO_CLI_NO_ANALYTICS"),
            "user env wins"
        );

        let p = prepare(&argv("maestro test flows/"), run, false, &[], dirs, |_| {
            false
        });
        assert_eq!(p.argv, argv("maestro test flows/"), "rewrite off");
    }
}

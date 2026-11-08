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

use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
};

use fleet_protocol::event::{EvidenceSources, ReportReason, ReportSource, ShardMode};

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
    /// `--config`: the workspace config file, relative to the working directory as given.
    pub config: Option<String>,
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
            "--config" => inv.config.clone_from(&value),
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
    /// The evidence sources this command will produce, for the dashboard.
    pub evidence: EvidenceSources,
}

/// Where Maestro writes the JUnit report when `--format junit` has no `--output`: `report.xml` in
/// its working directory (research 01 §test options, `TestCommand.kt` L691-699, Maestro 2.12.1).
/// Relative on purpose: Maestro inherits the agent's working directory, so it resolves the same.
pub const DEFAULT_REPORT: &str = "report.xml";

// Picocli behaviour, checked against the real CLI (Homebrew Maestro 2.11.0, picocli 4.6.3):
// `maestro test --output a.xml --output b.xml f.yaml` exits 2 with
// "option '--output' (<output>) should be specified only once", and a repeated `--format` is
// rejected the same way. So the agent must never add a second `--output` or `--format`: that
// would turn a working command into a usage error (I1).

/// What the user's `--format` is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UserFormat {
    None,
    Junit,
    /// `html`, `html-detailed`, `noop`, or anything else: no JUnit report.
    Other,
}

/// The report path the command asks Maestro for, if it asks for JUnit.
#[must_use]
pub fn requested_report(inv: &Invocation) -> Option<String> {
    match user_format(inv) {
        UserFormat::Junit => Some(
            inv.output
                .clone()
                .unwrap_or_else(|| DEFAULT_REPORT.to_owned()),
        ),
        UserFormat::None | UserFormat::Other => None,
    }
}

/// Where Maestro writes an HTML report (`--format html` or `html-detailed`) when there is no
/// `--output`: `report.html` in its working directory, as captured in
/// `fixtures/maestro/2.12.1/html-report-default-path`. Relative on purpose, like [`DEFAULT_REPORT`].
pub const DEFAULT_HTML_REPORT: &str = "report.html";

/// The HTML report the command asks Maestro for: `--format html` or `html-detailed`, written to
/// its `--output`, or to [`DEFAULT_HTML_REPORT`] without one. Nothing is injected for it: the
/// report is found where the command puts it.
#[must_use]
pub fn requested_html_report(inv: &Invocation) -> Option<String> {
    match inv.format.as_deref() {
        Some(f) if f.eq_ignore_ascii_case("html") || f.eq_ignore_ascii_case("html-detailed") => {
            Some(
                inv.output
                    .clone()
                    .unwrap_or_else(|| DEFAULT_HTML_REPORT.to_owned()),
            )
        }
        _ => None,
    }
}

/// A directory Maestro writes one run's files into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputRoot {
    /// The directory itself: `--flatten-debug-output` puts the session log and the flow folders
    /// straight into it.
    Flat(PathBuf),
    /// A `yyyy-MM-dd_HHmmss` directory that Maestro creates inside this one for the run.
    Stamped(PathBuf),
}

/// Where one `maestro test` command writes its session `maestro.log` and its per-flow bundles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputLayout {
    /// The directory that holds the session `maestro.log`.
    pub log: OutputRoot,
    /// The directory that holds the per-flow folders (`commands.json`, `manifest.json`, …).
    pub bundles: OutputRoot,
}

/// The directory Maestro calls `user.home`: the one `-Duser.home=` names in `JAVA_OPTS` or
/// `MAESTRO_OPTS` (the launcher passes both to the JVM, the later one wins), else the account's home
/// directory (`account_home`). The JVM does not read `$HOME`: with `HOME` changed, `user.home` stayed
/// the account's, on macOS (checked with `java -XshowSettings:properties`). Maestro's state
/// directory and `--flatten-debug-output` without a debug directory both use it.
#[must_use]
pub fn user_home(
    java_opts: Option<&str>,
    maestro_opts: Option<&str>,
    account_home: Option<&Path>,
) -> Option<PathBuf> {
    let overridden = [java_opts, maestro_opts]
        .into_iter()
        .flatten()
        .flat_map(str::split_whitespace)
        .filter_map(|opt| opt.strip_prefix("-Duser.home="))
        .next_back();
    overridden.map_or_else(
        || account_home.map(Path::to_path_buf),
        |h| Some(PathBuf::from(h)),
    )
}

/// Maestro's state directory for test runs: `$XDG_STATE_HOME/maestro/tests` when that variable is
/// set (and not empty), else `<user.home>/.maestro/tests` (research 01 §4.1). `None` when neither is
/// known. `home` is [`user_home`].
#[must_use]
pub fn state_tests_dir(xdg_state_home: Option<&OsStr>, home: Option<&Path>) -> Option<PathBuf> {
    match (xdg_state_home.filter(|v| !v.is_empty()), home) {
        (Some(state), _) => Some(PathBuf::from(state).join("maestro").join("tests")),
        (None, Some(home)) => Some(home.join(".maestro").join("tests")),
        (None, None) => None,
    }
}

/// The `testOutputDir` of a workspace `config.yaml` (or `config.yml`), if it sets one. Maestro reads
/// it when the command has no `--test-output-dir`, and it is relative to the working directory, not
/// to the config file (captured: `paths-config-explicit`). Unknown keys are ignored, as Maestro does.
/// `None` for a config that does not parse or sets no directory.
#[must_use]
pub fn workspace_test_output_dir(config_yaml: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Workspace {
        test_output_dir: Option<String>,
    }
    serde_yaml_ng::from_str::<Workspace>(config_yaml)
        .ok()?
        .test_output_dir
}

/// Where Maestro writes the session log and the flow bundles for a parsed `maestro test` command.
///
/// The rules are Maestro 2.12.1's (`TestDebugReporter.kt` L187-226 and `TestCommand.kt`
/// `resolveTestOutputDir`, research 01 §4.1), and the captured fixtures agree with them:
/// `paths-*`, `html-test-output-dir` and `html-no-debug-flags` (see `fixtures/maestro/2.12.1`).
///
/// - `--flatten-debug-output` writes both into `--debug-output`, or into `home` without one.
///   `--test-output-dir` and the workspace config's `testOutputDir` are ignored.
/// - Otherwise the log goes into a stamped directory under `<debug>/.maestro/tests`, or under the
///   state directory without `--debug-output`. The bundles go into a stamped directory under the
///   test output directory when one is set (`--test-output-dir`, else `workspace_test_dir`, the
///   config's `testOutputDir`), else beside the log. The flag wins over the config.
///
/// `home` is [`user_home`]. `None` when the directory is needed and unknown (no home, no state
/// directory).
#[must_use]
pub fn output_layout(
    inv: &Invocation,
    state_tests: Option<&Path>,
    home: Option<&Path>,
    workspace_test_dir: Option<&str>,
) -> Option<OutputLayout> {
    if inv.flatten {
        let dir = match inv.debug_output.as_deref() {
            Some(debug) => PathBuf::from(debug),
            None => home?.to_path_buf(),
        };
        return Some(OutputLayout {
            log: OutputRoot::Flat(dir.clone()),
            bundles: OutputRoot::Flat(dir),
        });
    }
    let log_base = match inv.debug_output.as_deref() {
        Some(debug) => PathBuf::from(debug).join(".maestro").join("tests"),
        None => state_tests?.to_path_buf(),
    };
    let bundles = match inv.test_output_dir.as_deref().or(workspace_test_dir) {
        Some(test) => OutputRoot::Stamped(PathBuf::from(test)),
        None => OutputRoot::Stamped(log_base.clone()),
    };
    Some(OutputLayout {
        log: OutputRoot::Stamped(log_base),
        bundles,
    })
}

fn user_format(inv: &Invocation) -> UserFormat {
    match inv.format.as_deref() {
        None => UserFormat::None,
        Some(f) if f.eq_ignore_ascii_case("junit") => UserFormat::Junit,
        Some(_) => UserFormat::Other,
    }
}

/// Decides whether `--format` can be added without changing Maestro's execution mode.
///
/// Maestro takes its multi-flow path for more than one flow file or a directory, and for
/// `--shard-all` (replication). `flows` is every flow path the run will get, `classify` says
/// what each is. Anything unknown means no injection. Only called when the user gave no
/// `--format`, and rewriting is on.
fn report_decision(
    inv: &Invocation,
    flows: &[String],
    classify: &impl Fn(&str) -> PathKind,
) -> ReportInjection {
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
/// files from directories. `workspace_test_dir` is the config's `testOutputDir` ([`workspace_test_output_dir`]):
/// a directory the user set counts as a flag they chose, so the agent adds no output flags then.
///
/// Maestro rejects a repeated `--flatten-debug-output` (captured: `paths-flatten-repeated`, exit 2),
/// so a user's own flatten flag is never added a second time.
///
/// The report is one normalization over (user's `--format`, user's `--output`, injection
/// decision); the agent never adds an option the user already gave:
///
/// | `--format` | `--output` | injection allowed | added | report read |
/// |---|---|---|---|---|
/// | none | none | yes | `--format junit --output <run>/report.xml` | that file |
/// | none | `X` | yes | `--format junit` | `X` |
/// | none | any | no | nothing | none (reason says why) |
/// | `junit` | none | n/a | nothing | [`DEFAULT_REPORT`] |
/// | `junit` | `X` | n/a | nothing | `X` |
/// | other | any | n/a | nothing | none, by the user's choice |
#[must_use]
pub fn prepare(
    argv: &[String],
    run_dir: &Path,
    rewrite: bool,
    extra_flows: &[String],
    workspace_test_dir: Option<&str>,
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
    let rewriting = rewrite && inv.test_index.is_some();
    let format = user_format(&inv);
    let decision = match (format, rewriting) {
        (UserFormat::None, true) => {
            let flows: Vec<String> = inv.positionals.iter().chain(extra_flows).cloned().collect();
            report_decision(&inv, &flows, &classify)
        }
        (UserFormat::None, false) => ReportInjection::Disabled,
        (UserFormat::Junit | UserFormat::Other, _) => ReportInjection::UserProvided,
    };
    let (report, reason) = match (format, inv.output.as_deref(), decision) {
        (UserFormat::None, None, ReportInjection::Injected) => {
            let file = run_dir.join("report.xml").to_string_lossy().into_owned();
            added.extend([
                "--format".to_owned(),
                "junit".to_owned(),
                "--output".to_owned(),
                file.clone(),
            ]);
            (Some(file), ReportReason::Injected)
        }
        (UserFormat::None, Some(x), ReportInjection::Injected) => {
            added.extend(["--format".to_owned(), "junit".to_owned()]);
            (Some(x.to_owned()), ReportReason::Injected)
        }
        (UserFormat::None, _, skipped) => (None, skipped_reason(skipped)),
        (UserFormat::Junit, ..) => (requested_report(&inv), ReportReason::UserJunit),
        (UserFormat::Other, ..) => (None, ReportReason::UserFormat),
    };
    if rewriting
        && inv.debug_output.is_none()
        && inv.test_output_dir.is_none()
        && workspace_test_dir.is_none()
    {
        let dir = run_dir.join("artifacts").to_string_lossy().into_owned();
        let flatten = (!inv.flatten).then(|| "--flatten-debug-output".to_owned());
        added.splice(
            0..0,
            flatten
                .into_iter()
                .chain(["--debug-output".to_owned(), dir.clone()]),
        );
        debug_dir = Some(dir);
    }
    if let Some(t) = inv.test_index.filter(|_| rewriting) {
        for (k, flag) in added.iter().enumerate() {
            out.insert(t + 1 + k, flag.clone());
        }
    }
    let env: Vec<(String, String)> = [
        ("MAESTRO_CLI_NO_ANALYTICS", "1"),
        ("MAESTRO_DISABLE_UPDATE_CHECK", "true"),
        ("MAESTRO_CLI_ANALYSIS_NOTIFICATION_DISABLED", "true"),
        ("MAESTRO_CLI_LOG_PATTERN_FILE", MACHINE_PATTERN),
    ]
    .into_iter()
    .filter(|(k, _)| !user_env(k))
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect();
    let maestro_log = env.iter().any(|(k, _)| k == "MAESTRO_CLI_LOG_PATTERN_FILE");
    Prepared {
        argv: out,
        env,
        added_flags: added,
        evidence: EvidenceSources {
            report: ReportSource {
                path: report.clone(),
                reason,
            },
            debug_output: debug_dir.clone(),
            maestro_log,
        },
        debug_dir,
        report,
        report_injection: decision,
    }
}

/// The reason a report wasn't added, for the decision that said so.
const fn skipped_reason(d: ReportInjection) -> ReportReason {
    match d {
        ReportInjection::SkippedSingleFlow => ReportReason::SkippedSingleFlow,
        ReportInjection::SkippedContinuous => ReportReason::SkippedContinuous,
        ReportInjection::SkippedUnclassified => ReportReason::SkippedUnclassified,
        ReportInjection::Disabled => ReportReason::RewriteDisabled,
        ReportInjection::Injected | ReportInjection::UserProvided => ReportReason::UserFormat,
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
        let p = prepare(
            &argv(cmd),
            Path::new("/run"),
            true,
            extra,
            None,
            dirs,
            |_| false,
        );
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
            None,
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
        let p = prepare(
            &argv("maestro test flows/"),
            run,
            true,
            &[],
            None,
            dirs,
            |_| false,
        );
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
        let p = prepare(&user, run, true, &[], None, dirs, |k| {
            k == "MAESTRO_CLI_NO_ANALYTICS"
        });
        assert_eq!(p.argv, user, "the user chose both; nothing is added");
        assert!(
            !p.env.iter().any(|(k, _)| k == "MAESTRO_CLI_NO_ANALYTICS"),
            "user env wins"
        );

        let p = prepare(
            &argv("maestro test flows/"),
            run,
            false,
            &[],
            None,
            dirs,
            |_| false,
        );
        assert_eq!(p.argv, argv("maestro test flows/"), "rewrite off");
    }

    fn run(cmd: &str) -> Prepared {
        prepare(&argv(cmd), Path::new("/run"), true, &[], None, dirs, |_| {
            false
        })
    }

    fn count(p: &Prepared, flag: &str) -> usize {
        p.argv.iter().filter(|a| *a == flag).count()
    }

    #[test]
    fn format_junit_without_output_reads_maestros_default() {
        let p = run("maestro test --format junit login.yaml");
        assert_eq!(count(&p, "--format"), 1);
        assert_eq!(count(&p, "--output"), 0, "{:?}", p.argv);
        assert!(!p.added_flags.iter().any(|a| a == "--format"));
        assert_eq!(p.report.as_deref(), Some(DEFAULT_REPORT));
        assert_eq!(p.evidence.report.reason, ReportReason::UserJunit);
        // Case-insensitive, like picocli's converter.
        let p = run("maestro test --format=JUNIT login.yaml");
        assert_eq!(p.report.as_deref(), Some(DEFAULT_REPORT));
    }

    #[test]
    fn output_without_format_never_gets_a_second_output() {
        // A suite: injecting `--format junit` is safe, and the user's path is the report.
        let p = run("maestro test --output custom.xml flows/");
        assert_eq!(count(&p, "--output"), 1, "{:?}", p.argv);
        assert_eq!(count(&p, "--format"), 1);
        assert_eq!(p.report.as_deref(), Some("custom.xml"));
        assert_eq!(p.evidence.report.reason, ReportReason::Injected);
        assert!(p.argv.windows(2).any(|w| w == ["--output", "custom.xml"]));
        // A single flow: adding `--format` would change the execution mode (#28), so nothing.
        let p = run("maestro test --output custom.xml login.yaml");
        assert_eq!((count(&p, "--output"), count(&p, "--format")), (1, 0));
        assert_eq!(p.report, None);
        assert_eq!(p.evidence.report.reason, ReportReason::SkippedSingleFlow);
        let p = run("maestro test -c --output custom.xml flows/");
        assert_eq!((count(&p, "--output"), count(&p, "--format")), (1, 0));
        assert_eq!(p.evidence.report.reason, ReportReason::SkippedContinuous);
    }

    #[test]
    fn other_formats_are_no_junit_by_choice() {
        for cmd in [
            "maestro test --format html flows/",
            "maestro test --format html-detailed --output r.html flows/",
            "maestro test --format noop flows/",
        ] {
            let p = run(cmd);
            assert_eq!(p.argv.len(), argv(cmd).len() + 3, "only debug flags: {cmd}");
            assert_eq!(p.report, None, "{cmd}");
            assert_eq!(p.evidence.report.reason, ReportReason::UserFormat);
        }
    }

    #[test]
    fn an_html_report_is_named_by_its_output_or_by_maestros_default() {
        let html = |cmd: &str| requested_html_report(&parse(&argv(cmd)));
        assert_eq!(
            html("maestro test --format html --output out/r.html flows/").as_deref(),
            Some("out/r.html")
        );
        assert_eq!(
            html("maestro test --format=HTML --output=r.html flows/").as_deref(),
            Some("r.html")
        );
        assert_eq!(
            html("maestro test --format html flows/").as_deref(),
            Some(DEFAULT_HTML_REPORT)
        );
        assert_eq!(
            html("maestro test --format html-detailed --output r.html flows/").as_deref(),
            Some("r.html")
        );
        assert_eq!(
            html("maestro test --format html-detailed flows/").as_deref(),
            Some(DEFAULT_HTML_REPORT)
        );
        assert_eq!(
            html("maestro test --format junit --output r.xml flows/"),
            None
        );
    }

    #[test]
    fn both_given_adds_nothing() {
        let p = run("maestro test --format junit --output x.xml flows/");
        assert_eq!((count(&p, "--format"), count(&p, "--output")), (1, 1));
        assert_eq!(p.report.as_deref(), Some("x.xml"));
        let p = prepare(
            &argv("maestro test --format junit --output x.xml flows/"),
            Path::new("/run"),
            false,
            &[],
            None,
            dirs,
            |_| false,
        );
        assert_eq!(
            p.report.as_deref(),
            Some("x.xml"),
            "rewrite off still reads it"
        );
        assert_eq!(p.added_flags.len(), 0);
    }

    #[test]
    fn nothing_given_follows_the_injection_decision() {
        let p = run("maestro test flows/");
        assert_eq!(p.evidence.report.path.as_deref(), Some("/run/report.xml"));
        assert_eq!(p.evidence.report.reason, ReportReason::Injected);
        assert_eq!(p.evidence.debug_output.as_deref(), Some("/run/artifacts"));
        assert!(p.evidence.maestro_log);
        let p = run("maestro test login.yaml");
        assert_eq!(p.evidence.report.path, None);
        assert_eq!(p.evidence.report.reason, ReportReason::SkippedSingleFlow);
        let p = prepare(
            &argv("maestro test flows/"),
            Path::new("/r"),
            false,
            &[],
            None,
            dirs,
            |_| true,
        );
        assert_eq!(p.evidence.report.reason, ReportReason::RewriteDisabled);
        assert!(!p.evidence.maestro_log, "the user's own log pattern");
    }

    /// Where Maestro writes for each flag combination (research 01 §4.1; the captured fixtures in
    /// `fixtures/maestro/2.12.1` are the runtime proof for the rows they cover).
    #[test]
    fn the_layout_follows_maestros_rules_for_each_flag_combination() {
        let state = Path::new("/state/maestro/tests");
        let home = Path::new("/home/me");
        let layout = |cmd: &str| {
            output_layout(&parse(&argv(cmd)), Some(state), Some(home), None).expect("a layout")
        };
        let stamped = |p: &str| OutputRoot::Stamped(PathBuf::from(p));
        let flat = |p: &str| OutputRoot::Flat(PathBuf::from(p));

        // paths-none, single-flow-no-format, html-no-debug-flags: the state directory, log and bundles.
        assert_eq!(
            layout("maestro test flows/"),
            OutputLayout {
                log: stamped("/state/maestro/tests"),
                bundles: stamped("/state/maestro/tests"),
            }
        );
        // paths-test-output-dir, html-test-output-dir: the log stays in the state directory.
        assert_eq!(
            layout("maestro test --test-output-dir T flows/"),
            OutputLayout {
                log: stamped("/state/maestro/tests"),
                bundles: stamped("T"),
            }
        );
        // paths-debug-output: the debug directory's own tree, for the log and the bundles.
        assert_eq!(
            layout("maestro test --debug-output D flows/"),
            OutputLayout {
                log: stamped("D/.maestro/tests"),
                bundles: stamped("D/.maestro/tests"),
            }
        );
        // paths-debug-and-test-output-same: two subtrees even when the paths are equal.
        assert_eq!(
            layout("maestro test --debug-output X --test-output-dir X flows/"),
            OutputLayout {
                log: stamped("X/.maestro/tests"),
                bundles: stamped("X"),
            }
        );
        // The same with a different test output directory (the rule is per flag).
        assert_eq!(
            layout("maestro test --debug-output D --test-output-dir T flows/"),
            OutputLayout {
                log: stamped("D/.maestro/tests"),
                bundles: stamped("T"),
            }
        );
        // html-report-default-path, the flattened debug directory: one flat directory for both.
        assert_eq!(
            layout(
                "maestro test --format html --flatten-debug-output --debug-output R/debug f.yaml"
            ),
            OutputLayout {
                log: flat("R/debug"),
                bundles: flat("R/debug"),
            }
        );
        // The agent's own injection is this same layout; `--test-output-dir` is ignored under flatten.
        assert_eq!(
            layout(
                "maestro test --flatten-debug-output --debug-output R --test-output-dir T flows/"
            ),
            OutputLayout {
                log: flat("R"),
                bundles: flat("R"),
            }
        );
        // `--flatten-debug-output` alone writes into the home directory (user.home).
        assert_eq!(
            layout("maestro test --flatten-debug-output flows/"),
            OutputLayout {
                log: flat("/home/me"),
                bundles: flat("/home/me"),
            }
        );
        assert_eq!(
            output_layout(
                &parse(&argv("maestro test --flatten-debug-output flows/")),
                Some(state),
                None,
                None,
            ),
            None,
            "no home, no directory"
        );
        assert_eq!(
            output_layout(&parse(&argv("maestro test flows/")), None, Some(home), None),
            None,
            "no state directory"
        );
    }

    #[test]
    fn the_workspace_config_sets_the_bundles_directory_and_the_flag_wins() {
        let state = Path::new("/state/maestro/tests");
        let layout = |cmd: &str, config: Option<&str>| {
            output_layout(&parse(&argv(cmd)), Some(state), None, config).expect("a layout")
        };
        // paths-config-explicit, paths-config-dir-input: the log stays in the state directory and
        // the bundles go into the config's directory (relative to the working directory).
        assert_eq!(
            layout(
                "maestro test --config cfg/config.yaml flows/",
                Some("out-config")
            ),
            OutputLayout {
                log: OutputRoot::Stamped(PathBuf::from("/state/maestro/tests")),
                bundles: OutputRoot::Stamped(PathBuf::from("out-config")),
            }
        );
        // paths-config-overridden-by-flag: --test-output-dir wins.
        assert_eq!(
            layout(
                "maestro test --config cfg/config.yaml --test-output-dir T flows/",
                Some("out-config")
            ),
            OutputLayout {
                log: OutputRoot::Stamped(PathBuf::from("/state/maestro/tests")),
                bundles: OutputRoot::Stamped(PathBuf::from("T")),
            }
        );
        // paths-config-file-beside: a single flow file does not read the config beside it, so the
        // config is None for it and the bundles stay beside the log.
        assert_eq!(
            layout("maestro test flows/pass.yaml", None).bundles,
            OutputRoot::Stamped(PathBuf::from("/state/maestro/tests"))
        );
        // paths-flatten-config: flatten ignores the config.
        assert_eq!(
            output_layout(
                &parse(&argv(
                    "maestro test --flatten-debug-output --config cfg/config.yaml flows/"
                )),
                Some(state),
                Some(Path::new("/home/me")),
                Some("out-config"),
            ),
            Some(OutputLayout {
                log: OutputRoot::Flat(PathBuf::from("/home/me")),
                bundles: OutputRoot::Flat(PathBuf::from("/home/me")),
            })
        );
    }

    #[test]
    fn the_workspace_config_reads_test_output_dir_and_ignores_the_rest() {
        assert_eq!(
            workspace_test_output_dir("flows:\n  - a.yaml\ntestOutputDir: out-config\n"),
            Some("out-config".to_owned())
        );
        assert_eq!(
            workspace_test_output_dir("includeTags: [smoke]\n"),
            None,
            "no key, no directory"
        );
        assert_eq!(
            workspace_test_output_dir("testOutputDir: [not, a, string]\n"),
            None,
            "a value of the wrong type does not parse"
        );
        assert_eq!(
            workspace_test_output_dir("testOutputDir: ["),
            None,
            "invalid YAML"
        );
    }

    #[test]
    fn user_home_is_the_jvm_override_else_the_account_home() {
        let account = Some(Path::new("/Users/me"));
        assert_eq!(
            user_home(None, None, account),
            Some(PathBuf::from("/Users/me"))
        );
        assert_eq!(
            user_home(Some("-Xmx1g -Duser.home=/w/h"), None, account),
            Some(PathBuf::from("/w/h")),
            "-Duser.home in JAVA_OPTS"
        );
        assert_eq!(
            user_home(Some("-Duser.home=/a"), Some("-Duser.home=/b"), account),
            Some(PathBuf::from("/b")),
            "MAESTRO_OPTS comes after JAVA_OPTS, so it wins"
        );
        assert_eq!(user_home(None, None, None), None);
    }

    #[test]
    fn a_users_flatten_flag_is_never_added_again() {
        // Maestro rejects a second --flatten-debug-output (paths-flatten-repeated, exit 2), so the
        // agent adds only the debug directory it needs.
        let p = run("maestro test --flatten-debug-output flows/");
        assert_eq!(count(&p, "--flatten-debug-output"), 1, "{:?}", p.argv);
        assert!(
            p.argv
                .windows(2)
                .any(|w| w == ["--debug-output", "/run/artifacts"])
        );
        assert_eq!(p.debug_dir.as_deref(), Some("/run/artifacts"));
        assert!(!p.added_flags.iter().any(|a| a == "--flatten-debug-output"));
        // Without the user's flag the agent adds it, as before.
        let p = run("maestro test flows/");
        assert_eq!(count(&p, "--flatten-debug-output"), 1);
    }

    #[test]
    fn a_configured_test_output_dir_is_the_users_choice() {
        let p = prepare(
            &argv("maestro test flows/"),
            Path::new("/run"),
            true,
            &[],
            Some("out-config"),
            dirs,
            |_| false,
        );
        assert!(
            !p.argv
                .iter()
                .any(|a| a == "--flatten-debug-output" || a == "--debug-output"),
            "the agent adds no debug flags over a config directory: {:?}",
            p.argv
        );
        assert_eq!(p.debug_dir, None);
    }

    #[test]
    fn the_state_directory_is_xdg_or_the_home_directory() {
        let xdg = OsStr::new("/xdg");
        assert_eq!(
            state_tests_dir(Some(xdg), Some(Path::new("/home/me"))),
            Some(PathBuf::from("/xdg/maestro/tests"))
        );
        assert_eq!(
            state_tests_dir(Some(OsStr::new("")), Some(Path::new("/home/me"))),
            Some(PathBuf::from("/home/me/.maestro/tests")),
            "an empty XDG_STATE_HOME is not set"
        );
        assert_eq!(
            state_tests_dir(None, Some(Path::new("/home/me"))),
            Some(PathBuf::from("/home/me/.maestro/tests"))
        );
        assert_eq!(state_tests_dir(None, None), None);
    }
}

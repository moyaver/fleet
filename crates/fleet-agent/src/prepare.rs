// SPDX-License-Identifier: AGPL-3.0-only
//! What the supervisor runs: the user's command, adjusted (PLAN §6.3).
//!
//! Only a `maestro … test …` command is adjusted, and only by adding: a flattened debug output
//! directory when the user didn't choose one, a JUnit report only where that can't change Maestro's
//! execution mode (several flows or a directory, not `-c`), and the machine log pattern. Without a
//! plan, anything else runs as given.
//!
//! A planner shard (`--plan`) is different: the shard's flows and the plan's workspace config are
//! the only selection Maestro gets, so the command must be `maestro test` with no flow paths, no
//! config, and no tag filters of its own. Every such problem is an error (exit 2), never a
//! fail-open case: running the whole suite on every shard would be worse than stopping.

use std::{
    ffi::OsString,
    fmt::Write as _,
    path::{Component, Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use fleet_maestro::invocation::PathKind;
use fleet_protocol::{
    event::{EvidenceSources, PlanAssigned},
    plan::PlanManifest,
};
use sha2::{Digest, Sha256};

use crate::layout;

/// The command and where its outputs will be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Prepared {
    /// What to run.
    pub(crate) argv: Vec<OsString>,
    /// Environment to add.
    pub(crate) env: Vec<(String, String)>,
    /// JUnit report.
    pub(crate) report: Option<String>,
    /// The agent's directory for this run.
    pub(crate) run_dir: Option<PathBuf>,
    /// The planner shard, when `--plan` was given.
    pub(crate) plan: Option<PlanAssigned>,
    /// The evidence sources the command will produce, when it was a `maestro test`.
    pub(crate) evidence: Option<EvidenceSources>,
}

/// Why the command can't run as asked.
#[derive(Debug)]
pub(crate) struct PlanError(pub(crate) String);

/// A planned shard: what it runs, and the workspace config it was planned with.
struct Planned {
    assigned: PlanAssigned,
    config: Option<String>,
}

/// Prepares `command`, for a planner shard when `plan` is given.
pub(crate) fn prepare(
    command: &[OsString],
    rewrite: bool,
    run_dir: Option<&Path>,
    plan: Option<(&Path, u32)>,
    user_env: impl Fn(&str) -> bool,
) -> Result<Prepared, PlanError> {
    let argv: Option<Vec<String>> = command
        .iter()
        .map(|a| a.to_str().map(str::to_owned))
        .collect();
    let Some((path, shard)) = plan else {
        return Ok(match argv {
            Some(argv) if is_maestro_test(&argv) => adjust(&argv, &[], rewrite, run_dir, user_env),
            _ => Prepared {
                argv: command.to_vec(),
                env: Vec::new(),
                report: None,
                run_dir: None,
                plan: None,
                evidence: None,
            },
        });
    };
    let planned = read_plan(path, shard)?;
    let argv = match argv {
        Some(argv) if is_maestro_test(&argv) => argv,
        _ => {
            return Err(PlanError(format!(
                "--plan runs `maestro test` for shard {} of plan {}, but the command is not one. \
                 Use `fleet-agent run --plan <plan> --shard {} -- maestro test`.",
                planned.assigned.shard_id, planned.assigned.plan_id, planned.assigned.shard_id,
            )));
        }
    };
    refuse_overrides(&argv, &planned)?;
    let mut argv = argv;
    if let Some(config) = &planned.config {
        argv.extend(["--config".to_owned(), config.clone()]);
    }
    let flows = planned.assigned.flows.clone();
    let mut prepared = adjust(&argv, &flows, rewrite, run_dir, user_env);
    prepared.argv.extend(flows.iter().map(OsString::from));
    prepared.plan = Some(planned.assigned);
    Ok(prepared)
}

fn is_maestro_test(argv: &[String]) -> bool {
    argv.first().is_some_and(|p| {
        Path::new(p)
            .file_name()
            .is_some_and(|n| n == "maestro" || n == "maestro.bat")
    }) && fleet_maestro::invocation::parse(argv).test_index.is_some()
}

/// Adds the agent's flags to a `maestro test` command. `flows` are the planned flows: they are
/// classified, so the JUnit decision matches the shard, but they are not added here.
fn adjust(
    argv: &[String],
    flows: &[String],
    rewrite: bool,
    run_dir: Option<&Path>,
    user_env: impl Fn(&str) -> bool,
) -> Prepared {
    let run_dir = Some(run_dir.map_or_else(default_run_dir, Path::to_path_buf))
        .filter(|d| std::fs::create_dir_all(d).is_ok());
    let cwd = std::env::current_dir().ok();
    let workspace = layout::workspace_test_dir(argv, cwd.as_deref());
    let p = match &run_dir {
        Some(dir) => fleet_maestro::invocation::prepare(
            argv,
            dir,
            rewrite,
            flows,
            workspace.as_deref(),
            classify,
            &user_env,
        ),
        // No place for outputs: keep the user's flags, still make the log machine-readable.
        None => fleet_maestro::invocation::prepare(
            argv,
            Path::new("."),
            false,
            flows,
            workspace.as_deref(),
            classify,
            &user_env,
        ),
    };
    tracing::info!(
        report = p.report_injection.as_str(),
        "junit report decision"
    );
    Prepared {
        argv: p.argv.into_iter().map(OsString::from).collect(),
        env: p.env,
        report: p.report,
        run_dir,
        plan: None,
        evidence: Some(p.evidence),
    }
}

/// Looks at the disk: file, directory, or unknown (missing, unreadable).
fn classify(path: &str) -> PathKind {
    match std::fs::metadata(path) {
        Ok(m) if m.is_dir() => PathKind::Dir,
        Ok(m) if m.is_file() => PathKind::File,
        _ => PathKind::Unknown,
    }
}

/// A planned shard's command must not select flows or config itself: flow paths, tag filters, and
/// (when the plan carries one) `--config` would each change what Maestro runs.
fn refuse_overrides(argv: &[String], planned: &Planned) -> Result<(), PlanError> {
    let plan = &planned.assigned;
    let given = fleet_maestro::invocation::parse(argv).positionals;
    if !given.is_empty() {
        return Err(PlanError(format!(
            "--plan runs shard {} of plan {} ({} flows), but the command already names {}: {}. \
             Maestro would run those as well as the shard's flows, so the shard would run more than it was assigned. \
             Remove the flow paths from the command; the plan supplies the flows, e.g. `fleet-agent run --plan <plan> --shard {} -- maestro test`",
            plan.shard_id,
            plan.plan_id,
            plan.flows.len(),
            if given.len() == 1 {
                "a flow path"
            } else {
                "flow paths"
            },
            given.join(", "),
            plan.shard_id,
        )));
    }
    let mut overrides = vec!["--include-tags", "--exclude-tags"];
    if planned.config.is_some() {
        overrides.push("--config");
    }
    if let Some(arg) = argv.iter().find(|a| {
        let name = a.split_once('=').map_or(a.as_str(), |(name, _)| name);
        overrides.contains(&name)
    }) {
        return Err(PlanError(format!(
            "--plan supplies the flows and the workspace config of shard {} of plan {}, so {arg} would change what runs; remove it",
            plan.shard_id, plan.plan_id,
        )));
    }
    Ok(())
}

/// The plan's workspace config, if it has one, after checking it is the file the planner read.
///
/// The path is relative to the checkout, like the flow paths, and the digest must match.
fn planned_config(manifest: &PlanManifest) -> Result<Option<String>, PlanError> {
    match (&manifest.config_path, &manifest.config_sha256) {
        (Some(path), Some(expected)) => {
            let relative = Path::new(path);
            if !relative
                .components()
                .all(|c| matches!(c, Component::Normal(_)))
            {
                return Err(PlanError(format!(
                    "plan config {path} must be a path inside the checkout"
                )));
            }
            let bytes = std::fs::read(relative)
                .map_err(|e| PlanError(format!("reading plan config {path}: {e}")))?;
            if hex(&Sha256::digest(&bytes)) != *expected {
                return Err(PlanError(format!(
                    "plan config {path} changed since the plan was made"
                )));
            }
            Ok(Some(path.clone()))
        }
        (None, None) => Ok(None),
        _ => Err(PlanError(
            "plan config path and digest must both be present, or both absent".into(),
        )),
    }
}

fn read_plan(path: &Path, shard: u32) -> Result<Planned, PlanError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| PlanError(format!("reading plan {}: {e}", path.display())))?;
    let manifest: PlanManifest = serde_json::from_str(&text)
        .map_err(|e| PlanError(format!("plan {} is not a Fleet plan: {e}", path.display())))?;
    let config = planned_config(&manifest)?;
    let plan_id = manifest.plan_id;
    let s = manifest
        .shards
        .into_iter()
        .find(|s| s.id == shard)
        .ok_or_else(|| PlanError(format!("plan {plan_id} has no shard {shard}")))?;
    // Maestro with no flow paths runs the whole workspace, so an empty shard must not reach it.
    if s.flows.is_empty() {
        return Err(PlanError(format!(
            "shard {shard} of plan {plan_id} has no flows, so there is nothing to run; plan again with fewer shards"
        )));
    }
    Ok(Planned {
        assigned: PlanAssigned {
            plan_id,
            shard_id: shard,
            flows: s.flows,
        },
        config,
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut hex, b| {
        let _ = write!(hex, "{b:02x}");
        hex
    })
}

/// `$FLEET_RUN_DIR`, else `$RUNNER_TEMP/fleet-agent/<id>`, else the system temp dir.
fn default_run_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("FLEET_RUN_DIR") {
        return PathBuf::from(d);
    }
    let base = std::env::var_os("RUNNER_TEMP").map_or_else(std::env::temp_dir, PathBuf::from);
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    base.join("fleet-agent")
        .join(format!("{stamp}-{}", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(s: &str) -> Vec<OsString> {
        s.split_whitespace().map(OsString::from).collect()
    }

    fn plan_file(name: &str, json: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("fleet-{name}-{}.json", std::process::id()));
        std::fs::write(&path, json).unwrap();
        path
    }

    #[test]
    fn only_maestro_test_is_adjusted() {
        let dir = std::env::temp_dir().join(format!("fleet-prep-{}", std::process::id()));
        let flows = dir.join("flows");
        std::fs::create_dir_all(&flows).unwrap();
        let cmd = vec![
            OsString::from("/opt/bin/maestro"),
            OsString::from("test"),
            flows.clone().into_os_string(),
        ];
        let p = prepare(&cmd, true, Some(&dir), None, |_| false).unwrap();
        assert!(p.argv.iter().any(|a| a == "--flatten-debug-output"));
        assert!(
            p.argv.iter().any(|a| a == "--format"),
            "a directory is a suite"
        );
        let single = flows.join("one.yaml");
        std::fs::write(&single, "appId: x\n---\n").unwrap();
        let cmd = vec![
            OsString::from("/opt/bin/maestro"),
            OsString::from("test"),
            single.into_os_string(),
        ];
        let p = prepare(&cmd, true, Some(&dir), None, |_| false).unwrap();
        assert!(p.argv.iter().any(|a| a == "--flatten-debug-output"));
        assert!(
            !p.argv.iter().any(|a| a == "--format"),
            "one flow keeps its mode"
        );
        assert_eq!(p.report, None);
        assert!(
            p.env
                .iter()
                .any(|(k, _)| k == "MAESTRO_CLI_LOG_PATTERN_FILE")
        );
        assert_eq!(p.run_dir.as_deref(), Some(dir.as_path()));
        let p = prepare(&os("sh -c true"), true, Some(&dir), None, |_| false).unwrap();
        assert_eq!(p.argv, os("sh -c true"));
        assert_eq!(p.env, Vec::<(String, String)>::new());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn plans_add_the_shard_flows_or_fail() {
        let path = plan_file(
            "plan-add",
            r#"{"planId":"p1","totalFlows":2,"shards":[{"id":0,"estimatedSeconds":5,"flows":["a.yaml"]},{"id":1,"flows":["b.yaml"]}]}"#,
        );
        let p = prepare(&os("maestro test"), false, None, Some((&path, 1)), |_| {
            false
        })
        .unwrap();
        assert_eq!(
            p.argv
                .last()
                .map(|a| a.to_string_lossy().into_owned())
                .as_deref(),
            Some("b.yaml")
        );
        assert_eq!(p.plan.as_ref().map(|p| p.plan_id.as_str()), Some("p1"));
        assert!(
            prepare(&os("maestro test"), false, None, Some((&path, 7)), |_| {
                false
            })
            .is_err(),
            "no such shard"
        );
        assert!(
            prepare(
                &os("maestro test"),
                false,
                None,
                Some((Path::new("/nope.json"), 0)),
                |_| false
            )
            .is_err()
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn plans_refuse_flow_paths_but_accept_options() {
        let path = plan_file(
            "plan-pos",
            r#"{"planId":"p1","totalFlows":2,"shards":[{"id":0,"flows":["a.yaml"]}]}"#,
        );
        let run = |cmd: &str| prepare(&os(cmd), false, None, Some((&path, 0)), |_| false);
        for cmd in [
            "maestro test .maestro/",
            "maestro test --device d x.yaml",
            "maestro test -- x.yaml",
        ] {
            let e = run(cmd).unwrap_err().0;
            assert!(
                e.contains("Remove the flow paths") && e.contains("p1"),
                "{e}"
            );
        }
        let p = run("maestro --device emulator-5554 test -e A=b/c.yaml --config cfg/c.yaml --output out/r.xml --format junit")
            .unwrap();
        let args: Vec<_> = p
            .argv
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args.last().map(String::as_str), Some("a.yaml"));
        assert_eq!(
            args.iter().filter(|a| a.contains(".y")).count(),
            3,
            "only the option values and the shard flow"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn plans_refuse_an_empty_shard_before_maestro_runs() {
        let path = plan_file(
            "plan-empty",
            r#"{"planId":"p1","totalFlows":1,"shards":[{"id":0,"flows":[]},{"id":1,"flows":["b.yaml"]}]}"#,
        );
        let e = prepare(&os("maestro test"), false, None, Some((&path, 0)), |_| {
            false
        })
        .unwrap_err()
        .0;
        assert!(e.contains("nothing to run"), "{e}");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn plans_run_only_maestro_test() {
        let path = plan_file(
            "plan-other",
            r#"{"planId":"p1","totalFlows":1,"shards":[{"id":0,"flows":["a.yaml"]}]}"#,
        );
        for cmd in ["sh -c true", "maestro --version"] {
            assert!(
                prepare(&os(cmd), false, None, Some((&path, 0)), |_| false).is_err(),
                "{cmd} must not get the shard's flows"
            );
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn plans_refuse_tag_filters_of_their_own() {
        let path = plan_file(
            "plan-tags",
            r#"{"planId":"p1","totalFlows":1,"shards":[{"id":0,"flows":["a.yaml"]}]}"#,
        );
        for cmd in [
            "maestro test --include-tags smoke",
            "maestro test --exclude-tags=slow",
        ] {
            let e = prepare(&os(cmd), false, None, Some((&path, 0)), |_| false)
                .unwrap_err()
                .0;
            assert!(e.contains("would change what runs"), "{e}");
        }
        std::fs::remove_file(&path).ok();
    }
}

// SPDX-License-Identifier: AGPL-3.0-only
//! What the supervisor runs: the user's command, adjusted (PLAN §6.3).
//!
//! Only a `maestro … test …` command is adjusted, and only by adding: a flattened debug output
//! directory and a JUnit report when the user didn't choose them, the machine log pattern, and a
//! planner shard's flows. Anything that fails here fails open: the command runs as given.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use fleet_protocol::{event::PlanAssigned, plan::PlanManifest};

/// The command and where its outputs will be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Prepared {
    /// What to run.
    pub(crate) argv: Vec<OsString>,
    /// Environment to add.
    pub(crate) env: Vec<(String, String)>,
    /// Session log and flow bundles.
    pub(crate) debug_dir: Option<String>,
    /// JUnit report.
    pub(crate) report: Option<String>,
    /// The agent's directory for this run.
    pub(crate) run_dir: Option<PathBuf>,
    /// The planner shard, when `--plan` was given.
    pub(crate) plan: Option<PlanAssigned>,
}

/// Why the command can't run as asked.
#[derive(Debug)]
pub(crate) struct PlanError(pub(crate) String);

/// Prepares `command`. A plan that can't be read is an error, not a fail-open case: running the
/// whole suite on every shard would be worse than stopping.
pub(crate) fn prepare(
    command: &[OsString],
    rewrite: bool,
    run_dir: Option<&Path>,
    plan: Option<(&Path, u32)>,
    user_env: impl Fn(&str) -> bool,
) -> Result<Prepared, PlanError> {
    let plan = plan
        .map(|(path, shard)| read_plan(path, shard))
        .transpose()?;
    let unchanged = Prepared {
        argv: command.to_vec(),
        env: Vec::new(),
        debug_dir: None,
        report: None,
        run_dir: None,
        plan: plan.clone(),
    };
    let Some(argv): Option<Vec<String>> = command
        .iter()
        .map(|a| a.to_str().map(str::to_owned))
        .collect()
    else {
        return Ok(unchanged);
    };
    let is_maestro = argv.first().is_some_and(|p| {
        Path::new(p)
            .file_name()
            .is_some_and(|n| n == "maestro" || n == "maestro.bat")
    });
    if !is_maestro || fleet_maestro::invocation::parse(&argv).test_index.is_none() {
        return Ok(Prepared {
            argv: with_plan_flows(command.to_vec(), plan.as_ref()),
            ..unchanged
        });
    }
    let run_dir = Some(run_dir.map_or_else(default_run_dir, Path::to_path_buf))
        .filter(|d| std::fs::create_dir_all(d).is_ok());
    let p = match &run_dir {
        Some(dir) => fleet_maestro::invocation::prepare(&argv, dir, rewrite, &user_env),
        // No place for outputs: keep the user's flags, still make the log machine-readable.
        None => fleet_maestro::invocation::prepare(&argv, Path::new("."), false, &user_env),
    };
    let argv = with_plan_flows(
        p.argv.into_iter().map(OsString::from).collect(),
        plan.as_ref(),
    );
    Ok(Prepared {
        argv,
        env: p.env,
        debug_dir: p.debug_dir,
        report: p.report,
        run_dir,
        plan,
    })
}

fn with_plan_flows(mut argv: Vec<OsString>, plan: Option<&PlanAssigned>) -> Vec<OsString> {
    if let Some(p) = plan {
        argv.extend(p.flows.iter().map(OsString::from));
    }
    argv
}

fn read_plan(path: &Path, shard: u32) -> Result<PlanAssigned, PlanError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| PlanError(format!("reading plan {}: {e}", path.display())))?;
    let manifest: PlanManifest = serde_json::from_str(&text)
        .map_err(|e| PlanError(format!("plan {} is not a Fleet plan: {e}", path.display())))?;
    let s = manifest
        .shards
        .into_iter()
        .find(|s| s.id == shard)
        .ok_or_else(|| PlanError(format!("plan {} has no shard {shard}", manifest.plan_id)))?;
    Ok(PlanAssigned {
        plan_id: manifest.plan_id,
        shard_id: shard,
        flows: s.flows,
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

    #[test]
    fn only_maestro_test_is_adjusted() {
        let dir = std::env::temp_dir().join(format!("fleet-prep-{}", std::process::id()));
        let p = prepare(
            &os("/opt/bin/maestro test flows/"),
            true,
            Some(&dir),
            None,
            |_| false,
        )
        .unwrap();
        assert!(p.argv.iter().any(|a| a == "--flatten-debug-output"));
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
        let path = std::env::temp_dir().join(format!("fleet-plan-{}.json", std::process::id()));
        std::fs::write(&path, r#"{"planId":"p1","totalFlows":2,"shards":[{"id":0,"estimatedSeconds":5,"flows":["a.yaml"]},{"id":1,"flows":["b.yaml"]}]}"#).unwrap();
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
}

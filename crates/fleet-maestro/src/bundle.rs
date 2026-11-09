// SPDX-License-Identifier: AGPL-3.0-only
//! Per-flow bundle folders (evidence lane C; research 08 Q6–Q7).
//!
//! A flow folder appears when the flow starts; failure screenshots and hierarchies when a step
//! fails; `commands.json` and then `manifest.json` when it ends. `commands.json` is not final on
//! abnormal ends: empty after SIGTERM, `RUNNING` entries left after a device loss.

use std::collections::BTreeMap;

use serde::Deserialize;

/// What a path inside the debug output directory is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    /// A flow folder: `<flow>` or `<flow>-shard-N`.
    FlowDir {
        /// Flow name as Maestro sanitised it.
        flow: String,
        /// Shard, when sharded.
        shard: Option<u32>,
    },
    /// A failure or warning screenshot (`screenshots/step-NNN-….png`, never the `.png.tmp`).
    StepScreenshot {
        /// The flow folder.
        flow_dir: String,
        /// 1-based step number from the file name.
        step: u32,
    },
    /// `screen-hierarchy/step-NNN-….json`.
    StepHierarchy {
        /// The flow folder.
        flow_dir: String,
        /// 1-based step number.
        step: u32,
    },
    /// `commands.json`: exact step data, written at flow end.
    Commands {
        /// The flow folder.
        flow_dir: String,
    },
    /// `manifest.json`: the flow's last file.
    Manifest {
        /// The flow folder.
        flow_dir: String,
    },
    /// `logs/crash-report.txt`: the app died during the flow (issue #75).
    CrashReport {
        /// The flow folder.
        flow_dir: String,
    },
    /// `takeScreenshot/<name>.png`: a screenshot the flow took on purpose (issue #133).
    UserScreenshot {
        /// The flow folder.
        flow_dir: String,
    },
    /// `startRecording/<name>.mp4`: a recording the flow made on purpose (issue #133).
    UserRecording {
        /// The flow folder.
        flow_dir: String,
    },
    /// Something else.
    Other,
}

/// Classifies a path relative to the debug output directory (`/`-separated).
#[must_use]
pub fn classify(rel: &str) -> Entry {
    let parts: Vec<&str> = rel.split('/').filter(|p| !p.is_empty()).collect();
    match parts.as_slice() {
        [dir] if !dir.contains('.') => {
            let (flow, shard) = split_shard_suffix(dir);
            Entry::FlowDir {
                flow: flow.to_owned(),
                shard,
            }
        }
        [dir, "commands.json"] => Entry::Commands {
            flow_dir: (*dir).to_owned(),
        },
        [dir, "manifest.json"] => Entry::Manifest {
            flow_dir: (*dir).to_owned(),
        },
        [dir, "logs", "crash-report.txt"] => Entry::CrashReport {
            flow_dir: (*dir).to_owned(),
        },
        [dir, "takeScreenshot", file] if has_ext(file, "png") => Entry::UserScreenshot {
            flow_dir: (*dir).to_owned(),
        },
        [dir, "startRecording", file] if has_ext(file, "mp4") => Entry::UserRecording {
            flow_dir: (*dir).to_owned(),
        },
        [dir, "screenshots", file] if has_ext(file, "png") => {
            step_number(file).map_or(Entry::Other, |step| Entry::StepScreenshot {
                flow_dir: (*dir).to_owned(),
                step,
            })
        }
        [dir, "screen-hierarchy", file] if has_ext(file, "json") => {
            step_number(file).map_or(Entry::Other, |step| Entry::StepHierarchy {
                flow_dir: (*dir).to_owned(),
                step,
            })
        }
        _ => Entry::Other,
    }
}

fn has_ext(file: &str, ext: &str) -> bool {
    std::path::Path::new(file)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case(ext))
}

/// Splits `name-shard-N` into `(name, Some(N))`.
#[must_use]
pub fn split_shard_suffix(dir: &str) -> (&str, Option<u32>) {
    dir.rsplit_once("-shard-")
        .and_then(|(name, n)| Some((name, Some(n.parse().ok()?))))
        .unwrap_or((dir, None))
}

fn step_number(file: &str) -> Option<u32> {
    file.strip_prefix("step-")?.split('-').next()?.parse().ok()
}

/// One entry of `commands.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandRecord {
    /// 0-based sequence number.
    pub sequence: u32,
    /// Nesting depth (0 = top level).
    pub depth: u32,
    /// Maestro's status string.
    pub status: String,
    /// Start, Unix milliseconds.
    pub timestamp_ms: Option<u64>,
    /// Duration in milliseconds.
    pub duration_ms: Option<u64>,
    /// Flow-folder-relative paths of the files this command produced (`artifacts[].path`, for
    /// example `screenshots/step-005-….png`). `None` when the command carries no `artifacts[]`
    /// key at all.
    pub artifacts: Option<Vec<String>>,
    /// Maestro's command kind: the one key of `command`, for example `tapOnElement`. `None` when
    /// `command` holds no single key.
    pub kind: Option<String>,
}

/// What a `commands.json` says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Commands {
    /// Non-empty, and every command has a recognised terminal status ([`TERMINAL_STATUSES`]).
    Final(Vec<CommandRecord>),
    /// Written, but some commands are still `RUNNING` or `PENDING`: the flow was cut short.
    Aborted(Vec<CommandRecord>),
    /// Some command has a status that is neither terminal nor in-flight: Maestro drift, so the
    /// file proves nothing (fail closed, I3).
    Unverified(Vec<CommandRecord>),
    /// No commands: a zero-byte file (Maestro was terminated while writing it) or `[]`.
    Empty,
    /// Not valid JSON of the expected shape.
    Invalid,
}

/// Terminal statuses of a command, as captured in the 2.12.1 fixtures (`COMPLETED`, `FAILED`,
/// `SKIPPED`, `WARNED`; research 08 §3, research 01 §14).
pub const TERMINAL_STATUSES: [&str; 4] = ["COMPLETED", "FAILED", "SKIPPED", "WARNED"];

/// In-flight statuses: a command still carrying one means the flow was cut short.
const IN_FLIGHT_STATUSES: [&str; 2] = ["RUNNING", "PENDING"];

#[derive(Deserialize)]
struct RawCommand {
    #[serde(default)]
    command: serde_json::Value,
    metadata: RawMetadata,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawMetadata {
    status: String,
    timestamp: Option<u64>,
    duration: Option<u64>,
    sequence_number: u32,
    depth: u32,
    artifacts: Option<Vec<RawArtifact>>,
}

#[derive(Deserialize)]
struct RawArtifact {
    path: String,
}

/// Parses a `commands.json` file's bytes.
#[must_use]
pub fn parse_commands(bytes: &[u8]) -> Commands {
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Commands::Empty;
    }
    let Ok(raw) = serde_json::from_slice::<Vec<RawCommand>>(bytes) else {
        return Commands::Invalid;
    };
    let records: Vec<CommandRecord> = raw
        .into_iter()
        .map(|c| CommandRecord {
            sequence: c.metadata.sequence_number,
            depth: c.metadata.depth,
            status: c.metadata.status,
            timestamp_ms: c.metadata.timestamp,
            duration_ms: c.metadata.duration,
            artifacts: c
                .metadata
                .artifacts
                .map(|a| a.into_iter().map(|a| a.path).collect()),
            kind: kind_of(&c.command),
        })
        .collect();
    let is = |set: &[&str], r: &CommandRecord| set.contains(&r.status.as_str());
    if records.is_empty() {
        Commands::Empty
    } else if records.iter().any(|r| is(&IN_FLIGHT_STATUSES, r)) {
        Commands::Aborted(records)
    } else if records.iter().all(|r| is(&TERMINAL_STATUSES, r)) {
        Commands::Final(records)
    } else {
        Commands::Unverified(records)
    }
}

/// The command kind of a `command` object: its only key, when it has exactly one.
fn kind_of(command: &serde_json::Value) -> Option<String> {
    let mut keys = command.as_object()?.keys();
    match (keys.next(), keys.next()) {
        (Some(key), None) => Some(key.clone()),
        _ => None,
    }
}

/// Which command produced each file of a flow folder, from `commands.json`'s `artifacts[]`: the
/// flow-relative path mapped to the producing command's sequence number. `None` when no command
/// carries `artifacts[]` (older Maestro output), so a file name is the only link to a step.
#[must_use]
pub fn produced_files(records: &[CommandRecord]) -> Option<BTreeMap<String, u32>> {
    if records.iter().all(|r| r.artifacts.is_none()) {
        return None;
    }
    let mut files = BTreeMap::new();
    for r in records {
        for path in r.artifacts.iter().flatten() {
            files.insert(path.clone(), r.sequence);
        }
    }
    Some(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_bundle_paths() {
        assert_eq!(
            classify("delta-fails-shard-1"),
            Entry::FlowDir {
                flow: "delta-fails".into(),
                shard: Some(1)
            }
        );
        assert_eq!(
            classify("pass"),
            Entry::FlowDir {
                flow: "pass".into(),
                shard: None
            }
        );
        assert_eq!(classify("maestro.log"), Entry::Other);
        assert_eq!(
            classify("crash/logs/crash-report.txt"),
            Entry::CrashReport {
                flow_dir: "crash".into()
            }
        );
        assert_eq!(
            classify("crash/logs/device-logcat.txt"),
            Entry::Other,
            "the logcat is read for its window, not as a bundle file"
        );
        assert_eq!(
            classify("fail/screenshots/step-005-assertCondition-Fleet_never_shows_this.png"),
            Entry::StepScreenshot {
                flow_dir: "fail".into(),
                step: 5
            }
        );
        assert_eq!(
            classify("fail/screenshots/step-005-x.png.tmp"),
            Entry::Other,
            "not until renamed"
        );
        assert_eq!(
            classify("fail/takeScreenshot/fleet-main.png"),
            Entry::UserScreenshot {
                flow_dir: "fail".into()
            },
            "Maestro's own screenshots are their own entries, not step screenshots"
        );
        assert_eq!(
            classify("recording/startRecording/fleet-recording.mp4"),
            Entry::UserRecording {
                flow_dir: "recording".into()
            }
        );
        assert_eq!(
            classify("recording/startRecording/fleet-recording.mp4.tmp"),
            Entry::Other,
            "a recording is not classified until its name is final"
        );
        assert_eq!(
            classify("fail/manifest.json"),
            Entry::Manifest {
                flow_dir: "fail".into()
            }
        );
        assert_eq!(
            split_shard_suffix("my-shard-thing"),
            ("my-shard-thing", None)
        );
    }

    #[test]
    fn commands_files() {
        let ok = br#"[{"command":{},"metadata":{"status":"COMPLETED","timestamp":1,"duration":5,"sequenceNumber":0,"depth":0}}]"#;
        assert!(
            matches!(parse_commands(ok), Commands::Final(v) if v.len() == 1 && v[0].duration_ms == Some(5))
        );
        let running = br#"[{"metadata":{"status":"RUNNING","sequenceNumber":3,"depth":1}}]"#;
        assert!(matches!(parse_commands(running), Commands::Aborted(_)));
        assert_eq!(parse_commands(b""), Commands::Empty);
        for status in TERMINAL_STATUSES {
            let one =
                format!(r#"[{{"metadata":{{"status":"{status}","sequenceNumber":0,"depth":0}}}}]"#);
            assert!(matches!(parse_commands(one.as_bytes()), Commands::Final(_)));
        }
        assert_eq!(parse_commands(b"[{\"metadata\":"), Commands::Invalid);
    }

    #[test]
    fn commands_files_fail_closed() {
        // An empty array proves nothing.
        assert_eq!(parse_commands(b"[]"), Commands::Empty);
        assert_eq!(parse_commands(b" [ ] \n"), Commands::Empty);
        // An unknown status is never final.
        let error = br#"[{"metadata":{"status":"ERROR","sequenceNumber":0,"depth":0}}]"#;
        assert!(matches!(parse_commands(error), Commands::Unverified(_)));
        // One unknown entry among completed ones taints the file.
        let mixed = br#"[{"metadata":{"status":"COMPLETED","sequenceNumber":0,"depth":0}},
            {"metadata":{"status":"completed","sequenceNumber":1,"depth":0}}]"#;
        assert!(matches!(parse_commands(mixed), Commands::Unverified(_)));
        let blank = br#"[{"metadata":{"status":"","sequenceNumber":0,"depth":0}}]"#;
        assert!(matches!(parse_commands(blank), Commands::Unverified(_)));
        // A missing status, metadata or non-array shape is invalid.
        let no_status = br#"[{"metadata":{"sequenceNumber":0,"depth":0}}]"#;
        assert_eq!(parse_commands(no_status), Commands::Invalid);
        assert_eq!(parse_commands(b"[{}]"), Commands::Invalid);
        assert_eq!(parse_commands(b"null"), Commands::Invalid);
        assert_eq!(parse_commands(b"{}"), Commands::Invalid);
        // In-flight beats unknown: the flow was cut short.
        let cut = br#"[{"metadata":{"status":"ERROR","sequenceNumber":0,"depth":0}},
            {"metadata":{"status":"RUNNING","sequenceNumber":1,"depth":0}}]"#;
        assert!(matches!(parse_commands(cut), Commands::Aborted(_)));
    }

    /// The records of a fixture's `commands.json`, whatever its state.
    fn records(bytes: &[u8]) -> Vec<CommandRecord> {
        match parse_commands(bytes) {
            Commands::Final(v) | Commands::Aborted(v) | Commands::Unverified(v) => v,
            other => panic!("not a commands file: {other:?}"),
        }
    }

    /// `artifacts[]` names the step that produced each file, so the link never depends on a name.
    #[test]
    fn produced_files_follow_artifacts_fixtures() {
        let fail = records(include_bytes!(
            "../../../fixtures/maestro/2.12.1/fail/run/debug/fail/commands.json"
        ));
        assert_eq!(
            produced_files(&fail),
            Some(BTreeMap::from([
                (
                    "screen-hierarchy/step-005-assertCondition-Fleet_never_shows_this.json".into(),
                    4
                ),
                (
                    "screenshots/step-005-assertCondition-Fleet_never_shows_this.png".into(),
                    4
                ),
            ]))
        );

        // A retry's own screenshot belongs to the retry (3), each failed attempt to itself (4, 5).
        let retry = records(include_bytes!(
            "../../../fixtures/maestro/2.12.1/retry-fail/run/debug/retry-fail/commands.json"
        ));
        assert_eq!(
            produced_files(&retry),
            Some(BTreeMap::from([
                ("screenshots/step-004-retry.png".into(), 3),
                (
                    "screen-hierarchy/step-005-assertCondition-Fleet_never_shows_this.json".into(),
                    4
                ),
                (
                    "screenshots/step-005-assertCondition-Fleet_never_shows_this.png".into(),
                    4
                ),
                (
                    "screen-hierarchy/step-006-assertCondition-Fleet_never_shows_this.json".into(),
                    5
                ),
                (
                    "screenshots/step-006-assertCondition-Fleet_never_shows_this.png".into(),
                    5
                ),
            ]))
        );

        // A passing flow lists no artifacts at all: older output, so names are the link.
        let pass = records(include_bytes!(
            "../../../fixtures/maestro/2.12.1/pass/run/debug/pass/commands.json"
        ));
        assert_eq!(produced_files(&pass), None);
    }
}

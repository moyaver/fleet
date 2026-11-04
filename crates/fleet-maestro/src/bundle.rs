// SPDX-License-Identifier: AGPL-3.0-only
//! Per-flow bundle folders (evidence lane C; research 08 Q6–Q7).
//!
//! A flow folder appears when the flow starts; failure screenshots and hierarchies when a step
//! fails; `commands.json` and then `manifest.json` when it ends. `commands.json` is not final on
//! abnormal ends: empty after SIGTERM, `RUNNING` entries left after a device loss.

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
}

/// What a `commands.json` says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Commands {
    /// Every command reached a final status.
    Final(Vec<CommandRecord>),
    /// Written, but some commands are still `RUNNING` or `PENDING`: the flow was cut short.
    Aborted(Vec<CommandRecord>),
    /// Empty (Maestro was terminated while writing it).
    Empty,
    /// Not valid JSON of the expected shape.
    Invalid,
}

#[derive(Deserialize)]
struct RawCommand {
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
        })
        .collect();
    if records
        .iter()
        .any(|r| r.status == "RUNNING" || r.status == "PENDING")
    {
        Commands::Aborted(records)
    } else {
        Commands::Final(records)
    }
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
        assert_eq!(parse_commands(b"[{\"metadata\":"), Commands::Invalid);
    }
}

// SPDX-License-Identifier: AGPL-3.0-only
//! `inspect` over every captured fixture: the live session's reading of real Maestro runs.

use std::{fs, path::Path};

use fleet_protocol::{
    event::{EventBody, RunSignal},
    journal::{ClockSample, EventEnvelope, Provenance, SourceKind, Subject},
};
use time::OffsetDateTime;

/// A journal that stopped on a failed write says so, with the error, and from which `seq` its
/// events are missing. The marker is what the writer leaves behind (issue #112, I5).
#[test]
fn a_journal_that_stopped_on_a_write_error_says_so() {
    let dir = std::env::temp_dir().join(format!("fleet-inspect-io-{}", std::process::id()));
    fs::remove_dir_all(&dir).ok();
    fs::create_dir_all(&dir).unwrap();
    let event = |seq| {
        let at = ClockSample {
            wall: OffsetDateTime::UNIX_EPOCH,
            mono_ms: seq,
        };
        EventEnvelope::new(
            seq,
            at,
            Subject::default(),
            Provenance::new(SourceKind::AgentObserved),
            &EventBody::RunSignal(RunSignal { signal: 2 }),
        )
    };
    let mut text = String::new();
    for seq in 1..=2 {
        text.push_str(&serde_json::to_string(&event(seq)).unwrap());
        text.push('\n');
    }
    let marker = serde_json::json!({
        "type": "journal.truncated",
        "first_missing_seq": 3,
        "bytes_written": text.len(),
        "reason": "io: StorageFull",
    });
    text.push_str(&marker.to_string());
    text.push('\n');
    fs::write(dir.join("journal.jsonl"), &text).unwrap();

    let (result, events) = fleet_agent::inspect::inspect(&dir).unwrap();
    fs::remove_dir_all(&dir).ok();
    let journal = result.journal.as_ref().expect("the journal is read");
    assert_eq!(journal.events, 2);
    assert_eq!(events.len(), 2);
    let stop = journal.truncated.as_ref().expect("the stop is reported");
    assert_eq!(stop.first_missing_seq, 3);
    assert_eq!(stop.reason, "io: StorageFull");
    let table = fleet_agent::inspect::table(&result);
    assert!(
        table.contains("it stopped on a write error (io: StorageFull)"),
        "{table}"
    );
    assert!(
        table.contains("Events from seq 3 on are missing from it."),
        "{table}"
    );
}

#[test]
fn every_fixture_reconciles() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/maestro/2.12.1");
    let mut names: Vec<_> = fs::read_dir(&root)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    for name in names {
        let (result, events) = fleet_agent::inspect::inspect(&root.join(&name)).unwrap();
        // Invariant I6: the planted secrets never reach a journal event.
        let journal = serde_json::to_string(&events).unwrap();
        for secret in [
            "fleet-planted-e-secret-7f3a",
            "fleet-planted-maestro-secret-91c2",
        ] {
            assert!(
                !journal.contains(secret),
                "{name}: {secret} leaked into the journal"
            );
        }
        // Gap-free sequence (invariant I5).
        for (i, e) in events.iter().enumerate() {
            assert_eq!(e.seq, i as u64 + 1, "{name}");
        }
        insta::assert_json_snapshot!(name, result);
    }
}

/// Each step's `step.ended` pairs with its own `step.started`: a block (`runFlow`, `retry`,
/// `repeat`) is named once, closed by its own end and carries its own status (#62).
#[test]
fn nested_steps_pair_their_ends_with_their_starts() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/maestro/2.12.1");

    // `features` nests runFlow, retry and repeat.
    let (_, events) = fleet_agent::inspect::inspect(&root.join("features")).unwrap();
    let steps = paired_steps("features", &events);
    assert_eq!(
        status_of(&steps, "Run subflows/open-apps.yaml"),
        ["completed"]
    );
    assert_eq!(status_of(&steps, "Retry 2 times"), ["completed"]);
    assert_eq!(status_of(&steps, "Repeat 2 times"), ["completed"]);
    assert_eq!(
        status_of(&steps, "Run flow when \"Fleet never shows this\""),
        ["skipped"]
    );

    // `retry-fail` fails a child inside a retry: I3, the parent is failed, never passed.
    let (_, events) = fleet_agent::inspect::inspect(&root.join("retry-fail")).unwrap();
    let steps = paired_steps("retry-fail", &events);
    assert_eq!(status_of(&steps, "Retry 1 times"), ["failed"]);
    assert_eq!(
        status_of(&steps, "Assert that \"Fleet never shows this\""),
        ["failed", "failed"]
    );
}

/// Every step as `index -> (description, status)`. Asserts that each step has exactly one start
/// and one end, so none is left running and none is an unnamed `Step N` row.
fn paired_steps(
    name: &str,
    events: &[fleet_protocol::journal::EventEnvelope],
) -> std::collections::BTreeMap<u32, (String, String)> {
    let mut started = std::collections::BTreeMap::new();
    let mut ended = std::collections::BTreeMap::new();
    for e in events {
        let Some(step) = &e.subject.step else {
            continue;
        };
        match e.kind.as_str() {
            "step.started" => {
                let description = e.data["description"].as_str().unwrap_or_default();
                assert!(
                    started.insert(step.index, description.to_owned()).is_none(),
                    "{name}: step {} started twice",
                    step.index
                );
            }
            "step.ended" => {
                let status = e.data["status"].as_str().unwrap_or_default();
                assert!(
                    ended.insert(step.index, status.to_owned()).is_none(),
                    "{name}: step {} ended twice",
                    step.index
                );
            }
            _ => {}
        }
    }
    assert_eq!(
        started.keys().collect::<Vec<_>>(),
        ended.keys().collect::<Vec<_>>(),
        "{name}: every step needs one start and one end"
    );
    started
        .into_iter()
        .map(|(index, description)| {
            let status = ended[&index].clone();
            (index, (description, status))
        })
        .collect()
}

/// The statuses of the steps whose description starts with `prefix`, in order.
fn status_of<'a>(
    steps: &'a std::collections::BTreeMap<u32, (String, String)>,
    prefix: &str,
) -> Vec<&'a str> {
    steps
        .values()
        .filter(|(description, _)| description.starts_with(prefix))
        .map(|(_, status)| status.as_str())
        .collect()
}

/// A failure screenshot and screen hierarchy attach to the step Maestro's `commands.json` says
/// produced them (`artifacts[]`), so the index is the step's, not one guessed from the file name.
#[test]
fn failure_files_attach_to_the_step_that_produced_them() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/maestro/2.12.1");

    // `fail`: the failing assertion is command 4, the fifth step (index 4).
    let (_, events) = fleet_agent::inspect::inspect(&root.join("fail")).unwrap();
    let steps = paired_steps("fail", &events);
    assert_eq!(
        status_of(&steps, "Assert that \"Fleet never shows this\""),
        ["failed"]
    );
    assert_eq!(
        artifact_steps(&events),
        [
            (
                "screen-hierarchy/step-005-assertCondition-Fleet_never_shows_this.json",
                Some(4)
            ),
            (
                "screenshots/step-005-assertCondition-Fleet_never_shows_this.png",
                Some(4)
            ),
        ]
    );
    assert!(
        steps[&4]
            .0
            .starts_with("Assert that \"Fleet never shows this\"")
    );

    // `retry-fail`: the retry itself (index 3) produced `step-004-retry.png`, and each failed
    // attempt inside it (indices 4 and 5) produced its own files.
    let (_, events) = fleet_agent::inspect::inspect(&root.join("retry-fail")).unwrap();
    let steps = paired_steps("retry-fail", &events);
    assert_eq!(
        artifact_steps(&events),
        [
            (
                "screen-hierarchy/step-005-assertCondition-Fleet_never_shows_this.json",
                Some(4)
            ),
            (
                "screen-hierarchy/step-006-assertCondition-Fleet_never_shows_this.json",
                Some(5)
            ),
            ("screenshots/step-004-retry.png", Some(3)),
            (
                "screenshots/step-005-assertCondition-Fleet_never_shows_this.png",
                Some(4)
            ),
            (
                "screenshots/step-006-assertCondition-Fleet_never_shows_this.png",
                Some(5)
            ),
        ]
    );
    assert_eq!(steps[&3].0, "Retry 1 times");
    assert_eq!(steps[&3].1, "failed");

    // `features` warns at command 17; the skipped runFlow after it is command 18, so the warning's
    // screenshot is still step 17.
    let (_, events) = fleet_agent::inspect::inspect(&root.join("features")).unwrap();
    let steps = paired_steps("features", &events);
    assert_eq!(
        artifact_steps(&events),
        [
            (
                "screen-hierarchy/step-018-assertCondition-Fleet_optional_marker.json",
                Some(17)
            ),
            (
                "screenshots/step-018-assertCondition-Fleet_optional_marker.png",
                Some(17)
            ),
        ]
    );
    assert!(
        steps[&17]
            .0
            .starts_with("Assert that (Optional) \"Fleet optional marker\"")
    );
    assert_eq!(steps[&17].1, "warned");
}

/// Each `artifact.registered` as `(path, step index)`, sorted by path, with the path reduced to
/// the flow folder's relative path.
fn artifact_steps(events: &[fleet_protocol::journal::EventEnvelope]) -> Vec<(&str, Option<u32>)> {
    let mut found: Vec<_> = events
        .iter()
        .filter(|e| e.kind == "artifact.registered")
        .map(|e| {
            let path = e.data["path"].as_str().unwrap_or_default();
            let path = path.split_once('/').map_or(path, |(_, rest)| rest);
            (path, e.subject.step.as_ref().map(|s| s.index))
        })
        .collect();
    found.sort();
    found
}

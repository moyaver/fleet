// SPDX-License-Identifier: AGPL-3.0-only
//! `inspect` over every captured fixture: the live session's reading of real Maestro runs.

use std::{fs, path::Path};

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

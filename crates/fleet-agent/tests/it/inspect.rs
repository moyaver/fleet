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

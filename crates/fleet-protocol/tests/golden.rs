// SPDX-License-Identifier: AGPL-3.0-only
//! Golden WebSocket frames round-trip and conform to their generated schemas.

use fleet_protocol::{
    frame::{ClientFrame, ServerFrame},
    live::{LiveRequest, Routed},
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

fn check<T: DeserializeOwned + Serialize>(name: &str, text: &str, operations: &[&str]) {
    let schemas = fleet_protocol::schema::documents();
    let schema = &schemas
        .iter()
        .find(|(key, _)| *key == name)
        .expect("test fixture should be valid")
        .1;
    let validator = jsonschema::validator_for(schema).expect("test fixture should be valid");
    let frames: Vec<Value> = serde_json::from_str(text).expect("test fixture should be valid");
    let mut seen = Vec::new();
    for frame in frames {
        validator
            .validate(&frame)
            .unwrap_or_else(|e| panic!("{name}: {e}: {frame}"));
        seen.push(
            frame["op"]
                .as_str()
                .expect("test fixture should be valid")
                .to_owned(),
        );
        let typed: T = serde_json::from_value(frame.clone()).expect("test fixture should be valid");
        assert_eq!(
            serde_json::to_value(typed).expect("test fixture should be valid"),
            frame,
            "{name}"
        );
    }
    seen.sort();
    seen.dedup();
    let mut expected = operations.to_vec();
    expected.sort_unstable();
    assert_eq!(seen, expected);
}

#[test]
fn every_websocket_frame_shape_round_trips_and_matches_its_schema() {
    check::<ClientFrame>(
        "client-frame",
        include_str!("golden/client-frame.json"),
        &["hello", "events", "telemetry", "heartbeat", "bye"],
    );
    check::<ServerFrame>(
        "server-frame",
        include_str!("golden/server-frame.json"),
        &[
            "welcome",
            "reject",
            "ack",
            "telemetry_ack",
            "event_rejected",
            "error",
            "goaway",
        ],
    );
    check::<Routed>(
        "live-frame",
        include_str!("golden/live-frame.json"),
        &[
            "subscribed",
            "unsubscribed",
            "snapshot",
            "delta",
            "liveness",
            "samples",
            "resync",
            "error",
        ],
    );
    check::<LiveRequest>(
        "live-request",
        include_str!("golden/live-request.json"),
        &["subscribe", "unsubscribe"],
    );
}

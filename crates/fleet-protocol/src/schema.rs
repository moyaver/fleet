// SPDX-License-Identifier: AGPL-3.0-only
//! JSON Schema (draft 2020-12) of the wire types, generated with `schemars` (PLAN §8).
//!
//! `cargo xtask schema` writes [`documents`] into `docs/src/protocol/schema/`, and `cargo xtask ci`
//! fails when the committed files are stale. The golden frames in `tests/golden/` validate against
//! the same documents.

use schemars::{JsonSchema, Schema, SchemaGenerator, generate::SchemaSettings, json_schema};

use crate::{
    artifact::{UploadGrant, UploadRequest, UploadResult},
    event::EventBody,
    frame::{BatchRequest, BatchResponse, ClientFrame, ServerFrame},
    journal::EventEnvelope,
    live::{LiveFrame, LiveRequest},
    plan::PlanManifest,
};

/// Schema of an RFC 3339 timestamp (`time::serde::rfc3339` fields).
pub(crate) fn rfc3339(_: &mut SchemaGenerator) -> Schema {
    json_schema!({ "type": "string", "format": "date-time" })
}

fn root<T: JsonSchema>() -> serde_json::Value {
    let generator = SchemaSettings::draft2020_12().into_generator();
    generator.into_root_schema_for::<T>().to_value()
}

/// The schema documents: a file stem (`server-frame`) and the document, one per top-level message.
#[must_use]
pub fn documents() -> Vec<(&'static str, serde_json::Value)> {
    vec![
        ("client-frame", root::<ClientFrame>()),
        ("server-frame", root::<ServerFrame>()),
        ("batch-request", root::<BatchRequest>()),
        ("batch-response", root::<BatchResponse>()),
        ("event-envelope", root::<EventEnvelope>()),
        ("event-body", root::<EventBody>()),
        ("live-frame", root::<LiveFrame>()),
        ("live-request", root::<LiveRequest>()),
        ("upload-request", root::<UploadRequest>()),
        ("upload-grant", root::<UploadGrant>()),
        ("upload-result", root::<UploadResult>()),
        ("plan-manifest", root::<PlanManifest>()),
    ]
}

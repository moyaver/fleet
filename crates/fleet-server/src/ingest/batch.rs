// SPDX-License-Identifier: AGPL-3.0-only
//! `POST /api/v1/agent/batch`: the HTTPS fallback for networks that block web sockets (PLAN §6.6).
//!
//! One request is one short connection: `hello`, the events (deduplicated and applied in one
//! transaction exactly like a socket frame), the telemetry (stored at once: there is no later
//! checkpoint to wait for), then the run is detached until the next request. The response lists
//! the frames a socket would have carried.

use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode},
};
use fleet_protocol::frame::{BatchRequest, BatchResponse, ServerFrame};
use fleet_store::{ingest::Transport, telemetry::TelemetryBatch};

use super::{After, authenticate, bearer, events, open};
use crate::state::AppState;

pub(super) async fn post(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<BatchRequest>,
) -> (StatusCode, Json<BatchResponse>) {
    let reply = |status, frames| (status, Json(BatchResponse { frames }));
    let publisher = match authenticate(&state, bearer(&headers).as_deref()).await {
        Ok(p) => p,
        Err(frame) => return reply(StatusCode::UNAUTHORIZED, vec![frame]),
    };
    let (run, welcome) = match open(&state, &publisher, &req.hello, Transport::Batch).await {
        Ok(ok) => ok,
        Err(frame) => return reply(StatusCode::OK, vec![frame]),
    };
    let mut frames = vec![welcome];
    if !req.events.is_empty() {
        let (mut out, after, change) = events(&state, &run, &req.events).await;
        frames.append(&mut out);
        state.publish(change);
        if after != After::Continue {
            let _ = state.store.detach(&run, "batch").await;
            return reply(StatusCode::OK, frames);
        }
    }
    if !req.telemetry.is_empty() {
        let batches: Vec<TelemetryBatch> = req
            .telemetry
            .into_iter()
            .map(|batch| TelemetryBatch {
                agent_run_id: run.agent_run_id,
                project_id: run.project_id,
                worker_id: run.worker_id,
                batch,
            })
            .collect();
        match state.store.store_telemetry(&batches).await {
            Ok(through) => frames.extend(
                through
                    .into_iter()
                    .map(|(_, through)| ServerFrame::TelemetryAck { through }),
            ),
            Err(e) => tracing::warn!(error = %e, "storing batch telemetry; not acked"),
        }
    }
    if let Err(e) = state.store.detach(&run, "batch").await {
        tracing::warn!(error = %e, "recording a batch detach");
    }
    reply(StatusCode::OK, frames)
}

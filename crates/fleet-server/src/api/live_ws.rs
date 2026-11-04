// SPDX-License-Identifier: AGPL-3.0-only
//! `/api/v1/live`: the browser's live socket (PLAN §7.3; research 03 §5.3).
//!
//! The client sends `{"op":"subscribe","project":"<slug>","attempt":"<uuid>"?}`. The server
//! subscribes to the project's channel **first**, then (for an attempt) reads and sends its
//! snapshot, then forwards deltas: so nothing committed between the two is lost. Deltas for the
//! subscribed attempt at or below the snapshot's version are skipped. A subscriber that falls
//! behind gets `{"op":"resync"}` and re-subscribes. The `Origin` must be the dashboard's own
//! (cross-site WebSocket hijacking).

use std::sync::Arc;

use axum::{
    extract::{
        State,
        ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use tokio::sync::broadcast::{self, error::RecvError};
use uuid::Uuid;

use fleet_protocol::live::{LiveFrame, LiveRequest, ResyncReason};

use super::{error, is_viewer};
use crate::state::AppState;

/// Largest client message: subscriptions are tiny.
const MAX_CLIENT_MESSAGE: usize = 16 * 1024;

fn same_origin(state: &AppState, headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN).and_then(|o| o.to_str().ok()) else {
        // Not a browser: there is no ambient credential to hijack.
        return true;
    };
    let origin = origin.trim_end_matches('/');
    match &state.config.api.public_url {
        Some(public) => origin.eq_ignore_ascii_case(public.trim_end_matches('/')),
        None => headers
            .get(header::HOST)
            .and_then(|h| h.to_str().ok())
            .is_some_and(|host| {
                origin
                    .split_once("://")
                    .is_some_and(|(_, rest)| rest.eq_ignore_ascii_case(host))
            }),
    }
}

pub(crate) async fn upgrade(
    State(state): State<AppState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if !same_origin(&state, &headers) {
        return error(
            StatusCode::FORBIDDEN,
            "cross-origin live sockets are refused",
        );
    }
    if !is_viewer(&state, &headers) {
        return error(StatusCode::UNAUTHORIZED, "a viewer token is required");
    }
    let tracker = state.tracker.clone();
    ws.max_message_size(MAX_CLIENT_MESSAGE)
        .on_upgrade(move |socket| tracker.track_future(session(state, socket)))
        .into_response()
}

async fn send(socket: &mut WebSocket, frame: &LiveFrame) -> bool {
    let text = serde_json::to_string(frame).unwrap_or_default();
    socket.send(Message::Text(text.into())).await.is_ok()
}

async fn session(state: AppState, mut socket: WebSocket) {
    // The first message must be a subscription.
    let Some(Ok(Message::Text(first))) = socket.recv().await else {
        return;
    };
    let Ok(LiveRequest::Subscribe { project, attempt }) =
        serde_json::from_str::<LiveRequest>(first.as_str())
    else {
        let _ = send(
            &mut socket,
            &LiveFrame::Error {
                message: "expected {\"op\":\"subscribe\",\"project\":…}".into(),
            },
        )
        .await;
        return;
    };
    let Ok(Some(project_id)) = state.store.project_id(&project).await else {
        let _ = send(
            &mut socket,
            &LiveFrame::Error {
                message: format!("no such project {project}"),
            },
        )
        .await;
        return;
    };
    // Subscribe before the snapshot: a delta committed in between is buffered, not lost.
    let mut rx = state.hub.subscribe(project_id);
    let mut floor = None;
    if let Some(attempt) = attempt {
        let Ok(Some(mut snapshot)) = state.store.snapshot(project_id, attempt).await else {
            let _ = send(
                &mut socket,
                &LiveFrame::Error {
                    message: "no such attempt".into(),
                },
            )
            .await;
            return;
        };
        state.hub.overlay_snapshot(&mut snapshot);
        floor = Some((
            attempt,
            u64::try_from(snapshot.attempt.version).unwrap_or(0),
        ));
        if !send(
            &mut socket,
            &LiveFrame::Snapshot {
                snapshot: Box::new(snapshot),
            },
        )
        .await
        {
            return;
        }
    }
    if !send(&mut socket, &LiveFrame::Subscribed { project_id }).await {
        return;
    }
    loop {
        tokio::select! {
            next = next(&mut rx, floor) => {
                let text = match next {
                    Next::Frame(f) => f,
                    Next::Resync => serde_json::to_string(&LiveFrame::Resync { reason: ResyncReason::Lagged })
                        .unwrap_or_default()
                        .into(),
                    Next::Closed => return,
                };
                if socket.send(Message::Text(text.as_ref().into())).await.is_err() {
                    return;
                }
            }
            msg = socket.recv() => match msg {
                Some(Ok(Message::Close(_)) | Err(_)) | None => return,
                Some(Ok(_)) => {}
            },
            () = state.shutdown.cancelled() => {
                let _ = socket
                    .send(Message::Close(Some(CloseFrame { code: 1012, reason: "server restarting".into() })))
                    .await;
                return;
            }
        }
    }
}

/// What to send next.
#[derive(Debug, PartialEq, Eq)]
enum Next {
    Frame(Arc<str>),
    /// The subscriber fell behind the channel (its socket is slow): it must re-snapshot.
    Resync,
    Closed,
}

/// The next frame for this subscriber, skipping deltas its snapshot already has.
///
/// # Cancel safety
/// Cancel-safe: `broadcast::Receiver::recv` is.
async fn next(rx: &mut broadcast::Receiver<Arc<str>>, floor: Option<(Uuid, u64)>) -> Next {
    loop {
        match rx.recv().await {
            Ok(frame) if stale(&frame, floor) => {}
            Ok(frame) => return Next::Frame(frame),
            Err(RecvError::Lagged(n)) => {
                tracing::debug!(missed = n, "live subscriber lagged");
                return Next::Resync;
            }
            Err(RecvError::Closed) => return Next::Closed,
        }
    }
}

/// A delta the snapshot already contains.
fn stale(frame: &Arc<str>, floor: Option<(Uuid, u64)>) -> bool {
    #[derive(Deserialize)]
    struct Head {
        op: String,
        attempt_id: Option<Uuid>,
        version: Option<u64>,
    }
    let Some((attempt, version)) = floor else {
        return false;
    };
    serde_json::from_str::<Head>(frame).is_ok_and(|h| {
        h.op == "delta" && h.attempt_id == Some(attempt) && h.version.is_some_and(|v| v <= version)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_lagging_subscriber_resyncs_and_old_deltas_are_skipped() {
        let (tx, mut rx) = broadcast::channel::<Arc<str>>(2);
        let attempt = Uuid::now_v7();
        let delta = |v: u64| -> Arc<str> {
            serde_json::json!({"op": "delta", "attempt_id": attempt, "version": v})
                .to_string()
                .into()
        };
        for v in 1..=5 {
            tx.send(delta(v)).unwrap();
        }
        assert_eq!(next(&mut rx, None).await, Next::Resync, "3 of 5 were lost");
        assert_eq!(next(&mut rx, None).await, Next::Frame(delta(4)));
        assert_eq!(next(&mut rx, None).await, Next::Frame(delta(5)));
        tx.send(delta(6)).unwrap();
        tx.send(delta(7)).unwrap();
        assert_eq!(
            next(&mut rx, Some((attempt, 6))).await,
            Next::Frame(delta(7)),
            "6 is in the snapshot"
        );
        drop(tx);
        assert_eq!(next(&mut rx, None).await, Next::Closed);
    }
}

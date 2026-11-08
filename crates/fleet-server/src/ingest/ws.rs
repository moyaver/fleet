// SPDX-License-Identifier: AGPL-3.0-only
//! `/api/v1/agent/ws` (`fleet.v1+json`): one task per socket, the single writer for its run.
//!
//! Keepalive and timeouts follow research 05 §3.8: the server pings every `ping_interval_ms`;
//! any inbound frame (events, telemetry, heartbeat, pong) counts as liveness, and a socket that
//! is silent for `idle_timeout_ms` is closed (the run becomes `detached`). On shutdown the socket
//! gets `goaway{shutdown}`, the final telemetry ack, then close 1012 (§3.9).

use std::time::Duration;

use axum::{
    body::Bytes,
    extract::{
        State,
        ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade},
    },
    http::HeaderMap,
    response::Response,
};
use fleet_protocol::{
    brand::WS_SUBPROTOCOL,
    frame::{ClientFrame, ErrorCode, GoawayReason, ServerFrame},
};
use fleet_store::{
    auth::Publisher,
    ingest::{RunHandle, Transport},
};
use tokio::{sync::mpsc, time::Instant};

use super::{After, authenticate, bearer, events, open, reject};
use crate::{
    limits::{Bucket, too_many_requests},
    state::AppState,
};

pub(super) async fn upgrade(
    State(state): State<AppState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let max = usize::try_from(state.config.ingest.max_frame_bytes).unwrap_or(usize::MAX);
    // Authenticate before upgrading, but report the outcome as a `reject` frame: the agent then
    // stops for good instead of retrying a credential that will never work (fail-open, I2).
    let auth = authenticate(&state, bearer(&headers).as_deref()).await;
    if let Ok(publisher) = &auth
        && let Err(wait) = state
            .limits
            .credential
            .check(publisher.credential, std::time::Instant::now())
    {
        return too_many_requests(wait);
    }
    let tracker = state.tracker.clone();
    ws.protocols([WS_SUBPROTOCOL])
        .max_message_size(max)
        .max_frame_size(max)
        .on_upgrade(move |socket| tracker.track_future(connection(state, socket, auth)))
}

async fn send(socket: &mut WebSocket, frame: &ServerFrame) -> Result<(), axum::Error> {
    let text = serde_json::to_string(frame).unwrap_or_default();
    socket.send(Message::Text(text.into())).await
}

async fn close(socket: &mut WebSocket, code: u16, reason: &str) {
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code,
            reason: reason.into(),
        })))
        .await;
}

/// Why a connection ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ended {
    Bye,
    Closed,
    Idle,
    Shutdown,
    Fatal,
}

impl Ended {
    fn reason(self) -> &'static str {
        match self {
            Self::Bye => "bye",
            Self::Closed => "closed",
            Self::Idle => "idle_timeout",
            Self::Shutdown => "server_shutdown",
            Self::Fatal => "fatal_error",
        }
    }
}

async fn connection(state: AppState, mut socket: WebSocket, auth: Result<Publisher, ServerFrame>) {
    let Some(run) = handshake(&state, &mut socket, auth).await else {
        return;
    };
    state.hub.seen(&run);
    let ended = stream(&state, &mut socket, &run).await;
    state.hub.disconnected(&run, ended == Ended::Bye);
    tracing::info!(agent_run_id = %run.agent_run_id, epoch = run.epoch, reason = ended.reason(),
        "agent connection ended");
    // A detach that changes the run's liveness is published so other instances see it too.
    let recorded = if ended == Ended::Shutdown {
        state
            .store
            .connection_closed(&run, ended.reason())
            .await
            .map(|()| None)
    } else {
        state.store.detach_closed_socket(&run, ended.reason()).await
    };
    match recorded {
        Ok(change) => state.publish(change),
        Err(e) => {
            tracing::warn!(agent_run_id = %run.agent_run_id, error = %e, "recording a detach");
        }
    }
}

/// Subprotocol, credential, `hello` → `welcome`. `None` when the socket was refused and closed.
async fn handshake(
    state: &AppState,
    socket: &mut WebSocket,
    auth: Result<Publisher, ServerFrame>,
) -> Option<RunHandle> {
    if socket.protocol().is_none() {
        let _ = send(
            socket,
            &reject(
                ErrorCode::UnsupportedProtocol,
                "the fleet.v1+json subprotocol is required",
                true,
            ),
        )
        .await;
        close(socket, 1002, "subprotocol").await;
        return None;
    }
    let publisher = match auth {
        Ok(p) => p,
        Err(frame) => {
            let code = if matches!(frame, ServerFrame::Reject { fatal: true, .. }) {
                1008
            } else {
                1011
            };
            let _ = send(socket, &frame).await;
            close(socket, code, "unauthorized").await;
            return None;
        }
    };
    let i = &state.config.ingest;
    let hello = tokio::time::timeout(Duration::from_millis(i.hello_timeout_ms), async {
        while let Some(Ok(msg)) = socket.recv().await {
            if let Message::Text(t) = msg {
                return serde_json::from_str::<ClientFrame>(t.as_str()).ok();
            }
        }
        None
    })
    .await;
    let Ok(Some(ClientFrame::Hello(hello))) = hello else {
        let _ = send(socket, &reject(ErrorCode::Invalid, "expected hello", false)).await;
        close(socket, 1008, "expected hello").await;
        return None;
    };
    let (run, welcome) = match open(state, &publisher, &hello, Transport::WebSocket).await {
        Ok(ok) => ok,
        Err(frame) => {
            let code = match &frame {
                ServerFrame::Reject { fatal: true, .. } => 1008,
                _ => 1013,
            };
            let _ = send(socket, &frame).await;
            close(socket, code, "rejected").await;
            return None;
        }
    };
    if send(socket, &welcome).await.is_err() {
        let _ = state.store.detach(&run, Ended::Closed.reason()).await;
        return None;
    }
    Some(run)
}

/// The steady state: one frame at a time, so an in-flight batch always finishes (and is acked)
/// before shutdown or a close is handled.
async fn stream(state: &AppState, socket: &mut WebSocket, run: &RunHandle) -> Ended {
    let i = &state.config.ingest;

    let (acks_tx, mut acks) = mpsc::channel::<u64>(16);
    let idle_after = Duration::from_millis(i.idle_timeout_ms);
    let idle = tokio::time::sleep(idle_after);
    tokio::pin!(idle);
    let mut ping = tokio::time::interval(Duration::from_millis(i.ping_interval_ms));
    ping.reset();
    // Events are paced, never dropped: past the socket's rate the server stops reading until the
    // debt is repaid, and the agent's window and TCP do the rest (invariant I5).
    let quota = state.limits.event;
    let mut events_bucket = Bucket::new(quota, std::time::Instant::now());
    let mut reads_resume = Instant::now();
    loop {
        let paused = reads_resume > Instant::now();
        tokio::select! {
            msg = socket.recv(), if !paused => {
                let Some(Ok(msg)) = msg else { break Ended::Closed };
                idle.as_mut().reset(Instant::now() + idle_after);
                state.hub.seen(run);
                let text = match msg {
                    Message::Text(t) => t,
                    Message::Close(_) => break Ended::Closed,
                    Message::Binary(_) => {
                        let _ = send(socket, &invalid("binary frames are not part of fleet.v1+json")).await;
                        continue;
                    }
                    Message::Ping(_) | Message::Pong(_) => continue,
                };
                match serde_json::from_str::<ClientFrame>(text.as_str()) {
                    Ok(ClientFrame::Events { events: batch }) => {
                        let (frames, after, change) = events(state, run, &batch).await;
                        for f in &frames {
                            if send(socket, f).await.is_err() {
                                break;
                            }
                        }
                        state.publish(change);
                        if let After::Close(code) = after {
                            close(socket, code, "fatal").await;
                            break Ended::Fatal;
                        }
                        let count = u32::try_from(batch.len()).unwrap_or(u32::MAX);
                        let wait = events_bucket.spend(
                            quota,
                            f64::from(count),
                            std::time::Instant::now(),
                        );
                        reads_resume = Instant::now() + wait;
                    }
                    Ok(ClientFrame::Telemetry(t)) => state.telemetry.push(run, t, acks_tx.clone()),
                    Ok(ClientFrame::Heartbeat { .. }) => {}
                    Ok(ClientFrame::Bye) => {
                        close(socket, 1000, "bye").await;
                        break Ended::Bye;
                    }
                    Ok(ClientFrame::Hello(_)) => {
                        let _ = send(socket, &invalid("hello was already accepted")).await;
                    }
                    Err(e) => {
                        let _ = send(socket, &invalid(&format!("bad frame: {e}"))).await;
                    }
                }
            }
            Some(through) = acks.recv() => {
                if send(socket, &ServerFrame::TelemetryAck { through }).await.is_err() {
                    break Ended::Closed;
                }
            }
            _ = ping.tick() => {
                if socket.send(Message::Ping(Bytes::default())).await.is_err() {
                    break Ended::Closed;
                }
            }
            () = tokio::time::sleep_until(reads_resume), if paused => {}
            () = &mut idle => {
                close(socket, 1001, "idle timeout").await;
                break Ended::Idle;
            }
            () = state.shutdown.cancelled() => {
                drain(state, socket, &mut acks).await;
                break Ended::Shutdown;
            }
        }
    }
}

/// How long a draining socket waits for the peer's close frame before it gives up on it.
const CLOSE_HANDSHAKE: Duration = Duration::from_secs(1);

/// Graceful shutdown for one socket (research 05 §3.9): `goaway`, the last telemetry
/// checkpoint and its ack, then close 1012 (service restart), and the peer's close frame.
async fn drain(state: &AppState, socket: &mut WebSocket, acks: &mut mpsc::Receiver<u64>) {
    let goaway = ServerFrame::Goaway {
        reason: GoawayReason::Shutdown,
        reconnect_after_ms: jitter(state.config.ingest.reconnect_jitter_ms),
    };
    let _ = send(socket, &goaway).await;
    state.telemetry.flush(&state.store).await;
    while let Ok(through) = acks.try_recv() {
        let _ = send(socket, &ServerFrame::TelemetryAck { through }).await;
    }
    close(socket, 1012, "server restarting").await;
    close_handshake(socket).await;
}

/// Reads until the peer's close frame, so the socket is closed with nothing unread. A socket
/// dropped with inbound bytes still queued is reset by the kernel, and the peer can lose the
/// `goaway` and close frames it has not read yet (RFC 6455 §5.5.1). Frames that arrive now are
/// read and dropped: their batches were never acked, so the agent resends them on reconnect.
async fn close_handshake(socket: &mut WebSocket) {
    let peer_closes = async {
        while let Some(Ok(msg)) = socket.recv().await {
            if matches!(msg, Message::Close(_)) {
                break;
            }
        }
    };
    let _ = tokio::time::timeout(CLOSE_HANDSHAKE, peer_closes).await;
}

fn invalid(message: &str) -> ServerFrame {
    ServerFrame::Error {
        code: ErrorCode::Invalid,
        fatal: false,
        message: message.into(),
        expected_seq: None,
    }
}

/// A delay in `0..max` ms, so agents don't reconnect in lockstep.
fn jitter(max: u64) -> u64 {
    // OS randomness: clock bits are not (on macOS the nanoseconds are always a multiple of 1000).
    if max == 0 {
        0
    } else {
        getrandom::u64().unwrap_or(0) % max
    }
}

#[cfg(test)]
mod tests {
    use super::jitter;

    #[test]
    fn jitter_varies() {
        let draws: std::collections::HashSet<u64> = (0..64).map(|_| jitter(2_000)).collect();
        assert!(
            draws.len() > 8,
            "jitter takes {} values: {draws:?}",
            draws.len()
        );
    }
}

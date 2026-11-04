// SPDX-License-Identifier: AGPL-3.0-only
//! Agent ingestion (PLAN §7.2; research 05 §3.3–§3.9).
//!
//! Both transports share the same steps: authenticate the bearer, `hello` → [`Store::open_run`]
//! → `welcome`, then each `events` frame → [`Store::apply_batch`] (one transaction) → `ack`
//! after commit → publish to the live layer. Telemetry is buffered and acked at the next
//! checkpoint ([`telemetry`]).
//!
//! [`Store::open_run`]: fleet_store::Store::open_run
//! [`Store::apply_batch`]: fleet_store::Store::apply_batch

mod batch;
pub(crate) mod telemetry;
mod ws;

use axum::{
    Router,
    extract::DefaultBodyLimit,
    http::{HeaderMap, header},
    routing::{get, post},
};
use fleet_protocol::{
    PROTOCOL_VERSION,
    frame::{ErrorCode, Hello, Limits, ServerFrame, Welcome},
    journal::EventEnvelope,
};
use fleet_store::{
    auth::Publisher,
    ingest::{ApplyError, Change, OpenError, RunHandle, Transport},
};

use crate::state::AppState;

pub(crate) fn router(state: &AppState) -> Router<AppState> {
    let body_limit = usize::try_from(state.config.ingest.max_frame_bytes)
        .unwrap_or(usize::MAX)
        .saturating_mul(8);
    Router::new()
        .route("/api/v1/agent/ws", get(ws::upgrade))
        .route("/api/v1/agent/token", post(crate::oidc::token))
        .route(
            "/api/v1/agent/batch",
            post(batch::post).layer(DefaultBodyLimit::max(body_limit)),
        )
}

/// The bearer secret of a request, if any.
pub(crate) fn bearer(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.trim().to_owned())
}

/// Who is connecting, or the `reject` frame to send.
pub(crate) async fn authenticate(
    state: &AppState,
    bearer: Option<&str>,
) -> Result<Publisher, ServerFrame> {
    let Some(token) = bearer else {
        return Err(reject(
            ErrorCode::Unauthorized,
            "a bearer token is required",
            true,
        ));
    };
    match state.store.authenticate(token).await {
        Ok(Some(p)) => Ok(p),
        Ok(None) => Err(reject(
            ErrorCode::Unauthorized,
            "unknown, expired or revoked credential",
            true,
        )),
        Err(e) => {
            tracing::error!(error = %e, "authenticating an agent");
            Err(ServerFrame::Reject {
                code: ErrorCode::Internal,
                message: "the server could not check the credential".into(),
                retry_after_ms: Some(1000),
                fatal: false,
            })
        }
    }
}

pub(crate) fn reject(code: ErrorCode, message: &str, fatal: bool) -> ServerFrame {
    ServerFrame::Reject {
        code,
        message: message.into(),
        retry_after_ms: None,
        fatal,
    }
}

/// `hello` → `welcome`, or the `reject` to send.
pub(crate) async fn open(
    state: &AppState,
    publisher: &Publisher,
    hello: &Hello,
    transport: Transport,
) -> Result<(RunHandle, ServerFrame), ServerFrame> {
    if !(hello.protocol.min..=hello.protocol.max).contains(&PROTOCOL_VERSION) {
        return Err(reject(
            ErrorCode::UnsupportedProtocol,
            &format!("this server speaks protocol {PROTOCOL_VERSION}"),
            true,
        ));
    }
    match state.store.open_run(publisher, hello, transport).await {
        Ok(opened) => {
            let i = &state.config.ingest;
            let welcome = ServerFrame::Welcome(Welcome {
                protocol: PROTOCOL_VERSION,
                connection_id: opened.handle.connection_id,
                epoch: opened.handle.epoch,
                committed_through: opened.committed_through,
                resume: opened.resume,
                limits: Limits {
                    max_inflight_events: i.max_inflight_events,
                    max_frame_bytes: i.max_frame_bytes,
                    heartbeat_interval_ms: i.heartbeat_interval_ms,
                },
            });
            tracing::info!(agent_run_id = %hello.agent_run_id, epoch = opened.handle.epoch,
                committed = opened.committed_through, resume = ?opened.resume, "agent run opened");
            state.publish(opened.change);
            Ok((opened.handle, welcome))
        }
        Err(OpenError::Refused {
            code,
            message,
            fatal,
        }) => {
            tracing::warn!(agent_run_id = %hello.agent_run_id, ?code, %message, "hello refused");
            Err(reject(code, &message, fatal))
        }
        Err(OpenError::Store(e)) => {
            tracing::error!(error = %e, "opening an agent run");
            Err(ServerFrame::Reject {
                code: ErrorCode::Internal,
                message: "the server could not open the run".into(),
                retry_after_ms: Some(1000),
                fatal: false,
            })
        }
    }
}

/// What the connection does after a batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum After {
    /// Keep going.
    Continue,
    /// Close with this WebSocket code after sending the frames.
    Close(u16),
}

/// The result of one `events` frame: frames to send in order, what to do next, and the change
/// to publish once the ack is on its way (COMMIT → ack → hub, research 05 §9.4).
pub(crate) type Outcome = (Vec<ServerFrame>, After, Option<Change>);

/// Applies one `events` frame.
pub(crate) async fn events(state: &AppState, run: &RunHandle, events: &[EventEnvelope]) -> Outcome {
    match state.store.apply_batch(run, events).await {
        Ok(applied) => {
            let mut frames: Vec<ServerFrame> = applied
                .rejected
                .into_iter()
                .map(|r| ServerFrame::EventRejected {
                    seq: r.seq,
                    code: r.code,
                    permanent: true,
                    message: r.message,
                })
                .collect();
            frames.push(ServerFrame::Ack {
                through: applied.through,
                epoch: run.epoch,
                duplicates: applied.duplicates,
            });
            if let Some(expected) = applied.expected {
                frames.push(seq_gap(expected));
            }
            (frames, After::Continue, applied.change)
        }
        Err(ApplyError::SeqGap { expected }) => (vec![seq_gap(expected)], After::Continue, None),
        Err(ApplyError::Superseded) => (
            vec![ServerFrame::Error {
                code: ErrorCode::Superseded,
                fatal: true,
                message: "a newer connection took over this run".into(),
                expected_seq: None,
            }],
            After::Close(1008),
            None,
        ),
        Err(ApplyError::Store(e)) => {
            tracing::error!(agent_run_id = %run.agent_run_id, error = %e, "applying a batch");
            (
                vec![ServerFrame::Error {
                    code: ErrorCode::Internal,
                    fatal: true,
                    message: "the batch could not be stored; reconnect and resend".into(),
                    expected_seq: None,
                }],
                After::Close(1011),
                None,
            )
        }
    }
}

fn seq_gap(expected: u64) -> ServerFrame {
    ServerFrame::Error {
        code: ErrorCode::SeqGap,
        fatal: false,
        message: format!("resend from seq {expected}"),
        expected_seq: Some(expected),
    }
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    #[test]
    fn bearer_tokens_are_parsed_case_insensitively() {
        let mut h = HeaderMap::new();
        assert_eq!(bearer(&h), None);
        h.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("bearer  mf_live_x "),
        );
        assert_eq!(bearer(&h).as_deref(), Some("mf_live_x"));
        h.insert(header::AUTHORIZATION, HeaderValue::from_static("Basic abc"));
        assert_eq!(bearer(&h), None);
    }
}

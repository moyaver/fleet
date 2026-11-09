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

use std::time::{Duration, Instant};

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

use crate::{project_policy, state::AppState};

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
    // A published run from before the journal's retention would be removed by the next retention
    // pass, so it is refused now, with the reason, rather than accepted and dropped.
    if let Some(started) = hello.started_at {
        let days = state.config.retention.journal_days;
        let cutoff = time::OffsetDateTime::now_utc() - time::Duration::days(i64::from(days));
        if started < cutoff {
            return Err(reject(
                ErrorCode::Invalid,
                &format!(
                    "the run started before the {days} days the journal keeps, so it would be removed at once"
                ),
                true,
            ));
        }
    }
    // The project's sampling policy (#139) travels in the welcome: its own, else the configured one.
    let overrides = match state.store.project_overrides(publisher.project_id).await {
        Ok(o) => o,
        Err(e) => {
            tracing::error!(error = %e, "reading a project's policy");
            return Err(ServerFrame::Reject {
                code: ErrorCode::Internal,
                message: "the server could not open the run".into(),
                retry_after_ms: Some(1000),
                fatal: false,
            });
        }
    };
    let (sample_interval_ms, pressure_sample_interval_ms) =
        project_policy::sampling(&state.config, &overrides);
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
                    sample_interval_ms,
                    pressure_sample_interval_ms,
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

/// A busy refusal is logged at most once per this interval, with the refusals it covers.
const BUSY_LOG_INTERVAL: Duration = Duration::from_secs(10);

/// Counts busy refusals, so an overloaded database writes one warning per interval, not one per
/// batch. Shared by the server's connections (see [`AppState`]).
#[derive(Debug, Default)]
pub(crate) struct BusyLog {
    /// Refusals since the last warning.
    refused: u64,
    /// When the last warning was written.
    last: Option<Instant>,
}

impl BusyLog {
    /// Records one refusal. `Some(n)` when a warning is due, with the `n` refusals since the last
    /// one (this one included).
    pub(crate) fn refused(&mut self, now: Instant) -> Option<u64> {
        self.refused = self.refused.saturating_add(1);
        let due = self
            .last
            .is_none_or(|last| now.duration_since(last) >= BUSY_LOG_INTERVAL);
        due.then(|| {
            self.last = Some(now);
            std::mem::take(&mut self.refused)
        })
    }
}

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
        Err(ApplyError::Busy) => {
            // Nothing was committed, so the socket stays up and the agent resends from the
            // batch's first seq, after a pause. The lowest seq is the first one the batch needs.
            let first = events.iter().map(|e| e.seq).min();
            let refused = state
                .busy
                .lock()
                .ok()
                .and_then(|mut log| log.refused(Instant::now()));
            if let Some(refused) = refused {
                tracing::warn!(refused, agent_run_id = %run.agent_run_id,
                    "the database is busy: refusing batches for the agents to resend");
            }
            let message = match first {
                Some(seq) => format!("the server is busy; resend from seq {seq}"),
                None => "the server is busy; retry later".into(),
            };
            (
                vec![ServerFrame::Error {
                    code: ErrorCode::Overloaded,
                    fatal: false,
                    message,
                    expected_seq: first,
                }],
                After::Continue,
                None,
            )
        }
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
    use fleet_protocol::{
        event::EventBody,
        frame::{AgentInfo, CiContext, CiProvider, FlowCatalog, ResumeState, RunOrigin},
        journal::{ClockSample, Provenance, SourceKind, Subject},
    };
    use fleet_store::{
        PoolOptions, Store,
        auth::{Credential, TrustLevel},
        testing::hold_connection,
    };
    use time::OffsetDateTime;
    use uuid::Uuid;

    use super::*;
    use crate::config::Config;

    /// A publisher and an open run on a fresh database, with the run's handle.
    async fn open_run(store: &Store) -> RunHandle {
        let project_id = store.upsert_project("app", "App").await.unwrap();
        let (key, _) = store
            .create_api_key(
                project_id,
                "ci",
                TrustLevel::Trusted,
                OffsetDateTime::now_utc() + time::Duration::days(90),
            )
            .await
            .unwrap();
        let publisher = Publisher {
            project_id,
            credential: Credential::ApiKey(key),
            trust: TrustLevel::Trusted,
            claims: None,
        };
        let now = OffsetDateTime::now_utc();
        let hello = Hello {
            protocol: fleet_protocol::frame::ProtocolRange {
                min: PROTOCOL_VERSION,
                max: PROTOCOL_VERSION,
            },
            agent: AgentInfo {
                version: "0.0.0".into(),
                os: "linux".into(),
                arch: "x86_64".into(),
            },
            agent_run_id: Uuid::now_v7(),
            previous_agent_run_id: None,
            ci: CiContext {
                provider: CiProvider::Generic,
                repository: Some("o/r".into()),
                run_id: Some("1".into()),
                run_attempt: Some(1),
                job_key: "job".into(),
                job_name: Some("job".into()),
                sha: None,
                git_ref: None,
                runner: Some("runner".into()),
            },
            resume: ResumeState {
                next_seq: 1,
                oldest_unacked_seq: None,
            },
            clock: ClockSample {
                wall: now,
                mono_ms: 0,
            },
            origin: RunOrigin::Live,
            started_at: None,
            catalog: FlowCatalog::default(),
        };
        store
            .open_run(&publisher, &hello, Transport::WebSocket)
            .await
            .unwrap()
            .handle
    }

    /// A batch of `RunFinalizing` events with the given seqs, in the order given.
    fn batch(seqs: &[u64]) -> Vec<EventEnvelope> {
        seqs.iter()
            .map(|&seq| {
                EventEnvelope::new(
                    seq,
                    ClockSample {
                        wall: OffsetDateTime::now_utc(),
                        mono_ms: 0,
                    },
                    Subject::default(),
                    Provenance::new(SourceKind::AgentObserved),
                    &EventBody::RunFinalizing,
                )
            })
            .collect()
    }

    /// A state whose store is `url`'s database, with the given pool settings.
    fn state_with(url: &str, pool: PoolOptions) -> AppState {
        let store = Store::connect_lazy(url, pool).unwrap();
        AppState::new(store, Config::default()).unwrap()
    }

    #[tokio::test]
    async fn a_pool_that_times_out_refuses_the_batch_and_keeps_the_socket() {
        let db = fleet_store::testing::fresh_database().await.unwrap();
        let run = open_run(&Store::connect_lazy(db.url(), PoolOptions::default()).unwrap()).await;
        // The pool's only connection is held, so the batch waits out its acquire timeout.
        let state = state_with(
            db.url(),
            PoolOptions {
                max_connections: 1,
                acquire_timeout: Duration::from_millis(1_000),
            },
        );
        let _held = hold_connection(&state.store).await.unwrap();
        let (frames, after, change) = events(&state, &run, &batch(&[5, 4, 6])).await;
        assert_eq!(after, After::Continue, "the socket stays open");
        assert!(change.is_none(), "nothing was committed");
        assert!(
            matches!(
                frames.as_slice(),
                [ServerFrame::Error {
                    code: ErrorCode::Overloaded,
                    fatal: false,
                    expected_seq: Some(4),
                    ..
                }]
            ),
            "the refusal names the batch's first seq: {frames:?}"
        );
    }

    #[tokio::test]
    async fn another_store_error_is_still_fatal() {
        let db = fleet_store::testing::fresh_database().await.unwrap();
        let run = open_run(&Store::connect_lazy(db.url(), PoolOptions::default()).unwrap()).await;
        // A database that does not exist fails at once: not a timeout, so not busy.
        let missing = db.url().replacen(db.name(), "fleet_missing_database", 1);
        let state = state_with(&missing, PoolOptions::default());
        let (frames, after, _) = events(&state, &run, &batch(&[1])).await;
        assert_eq!(after, After::Close(1011));
        assert!(
            matches!(
                frames.as_slice(),
                [ServerFrame::Error {
                    code: ErrorCode::Internal,
                    fatal: true,
                    expected_seq: None,
                    ..
                }]
            ),
            "{frames:?}"
        );
    }

    #[test]
    fn busy_refusals_warn_once_per_interval_and_count_what_they_cover() {
        let mut log = BusyLog::default();
        let t0 = Instant::now();
        assert_eq!(log.refused(t0), Some(1), "the first refusal warns");
        assert_eq!(log.refused(t0 + Duration::from_secs(1)), None);
        assert_eq!(log.refused(t0 + Duration::from_secs(2)), None);
        assert_eq!(
            log.refused(t0 + BUSY_LOG_INTERVAL),
            Some(3),
            "the next warning counts the refusals since the last"
        );
        assert_eq!(
            log.refused(t0 + BUSY_LOG_INTERVAL + Duration::from_millis(1)),
            None
        );
    }

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

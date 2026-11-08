// SPDX-License-Identifier: AGPL-3.0-only
//! `/api/v1/live`: the browser's live socket (PLAN §7.3; research 03 §5.3).
//!
//! One socket carries several subscriptions, each under a key: the `id` the client chose, or the
//! topic when it chose none. A client sends `{"op":"subscribe","id"?,"project":"<slug>","attempt":
//! "<uuid>"?}` and `{"op":"unsubscribe","id":"<key>"}`. Every frame that belongs to a subscription
//! says so with `sub`; a request the socket could not read or refuse is answered without one.
//!
//! A subscription is authorized on its own: a project it names must be one the caller may read
//! (`ReadRuns`), and a refusal answers that subscription alone, so the socket stays open for the
//! others. The server subscribes to the project's channel **first**, then (for an attempt) reads
//! and sends its snapshot, then forwards deltas: so nothing committed between the two is lost.
//! Deltas for an attempt at or below its snapshot's version are skipped. A subscriber that falls
//! behind gets `resync` under each of its subscriptions and re-subscribes those. The socket
//! closes when the browser's sign-in ends, when the server shuts down, or when the client does.
//! The `Origin` must be the dashboard's own (cross-site WebSocket hijacking).

use std::{collections::HashMap, future::pending, sync::Arc, time::Duration};

use axum::{
    extract::{
        State,
        ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, Method, StatusCode},
    response::{IntoResponse, Response},
};
use fleet_core::policy::{Action, Principal, Resource};
use fleet_protocol::live::{LiveFrame, LiveRequest, MAX_SUBSCRIPTION_ID, ResyncReason};
use futures_util::future::select_all;
use serde::Serialize;
use tokio::sync::broadcast::{self, error::RecvError};
use uuid::Uuid;

use super::{error, require};
use crate::{
    session::{self, principal, same_origin},
    state::AppState,
};

/// Largest client message: subscriptions are tiny.
const MAX_CLIENT_MESSAGE: usize = 16 * 1024;
/// Subscriptions one socket may hold at once. A subscribe beyond it is refused alone.
pub(crate) const MAX_SUBSCRIPTIONS: usize = 32;

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
    let principal = match principal(&state, &Method::GET, &headers).await {
        Ok(p) => p,
        Err(r) => return *r,
    };
    // A browser sign-in is re-checked while the socket is open: logout or expiry ends it.
    let cookie = session::read(&headers);
    let tracker = state.tracker.clone();
    ws.max_message_size(MAX_CLIENT_MESSAGE)
        .on_upgrade(move |socket| tracker.track_future(session(state, principal, cookie, socket)))
        .into_response()
}

/// Whether the browser's sign-in named by `cookie` still exists: a session that was ended or has
/// expired is gone, and a sign-in transaction (no user yet) doesn't count.
async fn still_signed_in(state: &AppState, cookie: &str) -> Result<bool, fleet_store::Error> {
    Ok(state
        .store
        .session(cookie, crate::session::idle(&state.config))
        .await?
        .is_some_and(|s| s.user_id.is_some()))
}

/// A frame on the wire: the frame and the subscription it belongs to.
#[derive(Serialize)]
struct Out<'a> {
    sub: &'a str,
    #[serde(flatten)]
    frame: &'a LiveFrame,
}

/// Sends `frame` under `sub`. `false` when the socket is gone.
async fn send_under(socket: &mut WebSocket, sub: &str, frame: &LiveFrame) -> bool {
    let text = serde_json::to_string(&Out { sub, frame }).unwrap_or_default();
    socket.send(Message::Text(text.into())).await.is_ok()
}

/// Sends a frame that answers no subscription. `false` when the socket is gone.
async fn send_plain(socket: &mut WebSocket, frame: &LiveFrame) -> bool {
    let text = serde_json::to_string(frame).unwrap_or_default();
    socket.send(Message::Text(text.into())).await.is_ok()
}

/// Refuses one request: an error under its key, or without a key when it has none. The socket
/// stays open. `false` when the socket is gone.
async fn refuse(socket: &mut WebSocket, sub: Option<&str>, message: impl Into<String>) -> bool {
    let frame = LiveFrame::Error {
        message: message.into(),
    };
    match sub {
        Some(sub) => send_under(socket, sub, &frame).await,
        None => send_plain(socket, &frame).await,
    }
}

/// The key of a subscription the client named with no `id`: its topic.
fn topic_key(project: &str, attempt: Option<Uuid>) -> String {
    match attempt {
        Some(attempt) => format!("{project}/{attempt}"),
        None => project.to_owned(),
    }
}

/// One subscription on a socket.
#[derive(Debug)]
struct Sub {
    project: Uuid,
    /// The attempt it follows, with the version its snapshot had. `None` follows the project.
    attempt: Option<(Uuid, u64)>,
}

impl Sub {
    /// Whether this subscription forwards `frame`. An attempt follows its own deltas past its
    /// snapshot and its own liveness; a project follows everything, samples included.
    fn wants(&self, frame: &LiveFrame) -> bool {
        let Some((attempt, floor)) = self.attempt else {
            return true;
        };
        match frame {
            LiveFrame::Delta {
                attempt_id,
                version,
                ..
            } => *attempt_id == attempt && *version > floor,
            LiveFrame::Liveness { attempt_id, .. } => *attempt_id == attempt,
            LiveFrame::Samples { .. } => false,
            _ => true,
        }
    }
}

/// Every subscription of one socket, and the project channels they read.
#[derive(Default)]
struct Subscriptions {
    subs: HashMap<String, Sub>,
    /// One receiver per project, shared by that project's subscriptions.
    channels: HashMap<Uuid, broadcast::Receiver<Arc<LiveFrame>>>,
}

/// What [`Subscriptions::next`] found.
enum Next {
    /// A frame of a project's channel.
    Frame(Uuid, Arc<LiveFrame>),
    /// The project's channel fell behind: its subscriptions must re-subscribe.
    Lagged(Uuid),
    /// No channel can ever produce a frame again.
    Closed,
}

impl Subscriptions {
    /// Makes room for one more subscription under `key`, or says why not.
    fn admit(&self, key: &str) -> Result<(), String> {
        if self.subs.contains_key(key) || self.subs.len() < MAX_SUBSCRIPTIONS {
            Ok(())
        } else {
            Err(format!(
                "at most {MAX_SUBSCRIPTIONS} subscriptions per socket; unsubscribe one first"
            ))
        }
    }

    /// Drops the receivers of projects no subscription reads any more.
    fn prune(&mut self) {
        let subs = &self.subs;
        self.channels
            .retain(|project, _| subs.values().any(|s| s.project == *project));
    }

    /// The next frame from any project this socket reads.
    ///
    /// # Cancel safety
    /// Cancel-safe: every receiver's `recv` is, and a cancelled call drops only futures that
    /// have not yet taken a frame.
    async fn next(&mut self) -> Next {
        if self.channels.is_empty() {
            return pending().await;
        }
        let waits = self.channels.iter_mut().map(|(project, rx)| {
            let project = *project;
            Box::pin(async move { (project, rx.recv().await) })
        });
        match select_all(waits).await.0 {
            (project, Ok(frame)) => Next::Frame(project, frame),
            (project, Err(RecvError::Lagged(missed))) => {
                tracing::debug!(missed, "live subscriber lagged");
                Next::Lagged(project)
            }
            (_, Err(RecvError::Closed)) => Next::Closed,
        }
    }

    /// The keys of the subscriptions of `project`, in a stable order.
    fn keys_of(&self, project: Uuid) -> Vec<String> {
        let mut keys: Vec<String> = self
            .subs
            .iter()
            .filter(|(_, s)| s.project == project)
            .map(|(k, _)| k.clone())
            .collect();
        keys.sort();
        keys
    }
}

async fn session(
    state: AppState,
    principal: Principal,
    cookie: Option<String>,
    mut socket: WebSocket,
) {
    let mut subs = Subscriptions::default();
    let period = Duration::from_millis(state.config.api.session_recheck_ms.max(1));
    let mut recheck = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
    loop {
        tokio::select! {
            _ = recheck.tick(), if cookie.is_some() => {
                let live = match &cookie {
                    Some(c) => still_signed_in(&state, c).await.unwrap_or(false),
                    None => true,
                };
                if !live {
                    let _ = socket
                        .send(Message::Close(Some(CloseFrame { code: 1008, reason: "session ended".into() })))
                        .await;
                    return;
                }
            }
            next = subs.next() => {
                let ok = match next {
                    Next::Frame(project, frame) => forward(&mut socket, &subs, project, &frame).await,
                    Next::Lagged(project) => {
                        let resync = LiveFrame::Resync { reason: ResyncReason::Lagged };
                        let mut ok = true;
                        for key in subs.keys_of(project) {
                            ok &= send_under(&mut socket, &key, &resync).await;
                        }
                        ok
                    }
                    Next::Closed => return,
                };
                if !ok {
                    return;
                }
            }
            msg = socket.recv() => match msg {
                Some(Ok(Message::Text(text))) => {
                    if !request(&state, &principal, &mut socket, &mut subs, text.as_str()).await {
                        return;
                    }
                }
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

/// Sends one project frame to every subscription of the project that wants it. `false` when the
/// socket is gone.
async fn forward(
    socket: &mut WebSocket,
    subs: &Subscriptions,
    project: Uuid,
    frame: &LiveFrame,
) -> bool {
    for key in subs.keys_of(project) {
        let wanted = subs.subs.get(&key).is_some_and(|sub| sub.wants(frame));
        if wanted && !send_under(socket, &key, frame).await {
            return false;
        }
    }
    true
}

/// Handles one client request. `false` when the socket is gone.
async fn request(
    state: &AppState,
    principal: &Principal,
    socket: &mut WebSocket,
    subs: &mut Subscriptions,
    text: &str,
) -> bool {
    match serde_json::from_str::<LiveRequest>(text) {
        Ok(LiveRequest::Subscribe {
            id,
            project,
            attempt,
        }) => {
            if let Some(id) = &id
                && (id.is_empty() || id.len() > MAX_SUBSCRIPTION_ID)
            {
                return refuse(
                    socket,
                    None,
                    format!("a subscription id is 1 to {MAX_SUBSCRIPTION_ID} bytes"),
                )
                .await;
            }
            let key = id.unwrap_or_else(|| topic_key(&project, attempt));
            match subscribe(state, principal, socket, subs, &key, &project, attempt).await {
                Ok(ok) => ok,
                Err(message) => {
                    subs.prune();
                    refuse(socket, Some(&key), message).await
                }
            }
        }
        Ok(LiveRequest::Unsubscribe { id }) => {
            if subs.subs.remove(&id).is_none() {
                return refuse(socket, Some(&id), format!("no subscription {id}")).await;
            }
            subs.prune();
            send_under(socket, &id, &LiveFrame::Unsubscribed).await
        }
        Err(_) => refuse(
            socket,
            None,
            "expected {\"op\":\"subscribe\",\"project\":…} or {\"op\":\"unsubscribe\",\"id\":…}",
        )
        .await,
    }
}

/// Authorizes and starts one subscription: its snapshot (for an attempt), then its confirmation.
/// A refusal is an `Err` the caller reports under the key, and it leaves every other subscription
/// as it was. `Ok(false)` when the socket is gone.
async fn subscribe(
    state: &AppState,
    principal: &Principal,
    socket: &mut WebSocket,
    subs: &mut Subscriptions,
    key: &str,
    project: &str,
    attempt: Option<Uuid>,
) -> Result<bool, String> {
    let project_id = match state.store.project_id(project).await {
        Ok(Some(id)) => id,
        Ok(None) => return Err(format!("no such project {project}")),
        Err(err) => {
            tracing::warn!(error = %err, "loading live subscription");
            return Err("live data temporarily unavailable; retry".into());
        }
    };
    // A project the caller has no role on answers like one that doesn't exist.
    if require(principal, Action::ReadRuns, Resource::Project(project_id)).is_err() {
        return Err(format!("no such project {project}"));
    }
    subs.admit(key)?;
    // Subscribe before the snapshot: a delta committed in between is buffered, not lost.
    subs.channels
        .entry(project_id)
        .or_insert_with(|| state.hub.subscribe(project_id));
    let mut snapshot = None;
    let mut sub = Sub {
        project: project_id,
        attempt: None,
    };
    if let Some(attempt) = attempt {
        let mut loaded = match state.store.snapshot(project_id, attempt).await {
            Ok(Some(loaded)) => loaded,
            Ok(None) => return Err("no such attempt".into()),
            Err(err) => {
                tracing::warn!(error = %err, "loading live subscription");
                return Err("live data temporarily unavailable; retry".into());
            }
        };
        state.hub.overlay_snapshot(&mut loaded);
        let version = u64::try_from(loaded.attempt.version).unwrap_or(0);
        sub.attempt = Some((attempt, version));
        snapshot = Some(loaded);
    }
    // A subscription under a key the socket holds replaces it.
    subs.subs.insert(key.to_owned(), sub);
    if let Some(snapshot) = snapshot {
        let frame = LiveFrame::Snapshot {
            snapshot: Box::new(snapshot),
        };
        if !send_under(socket, key, &frame).await {
            return Ok(false);
        }
    }
    let subscribed = LiveFrame::Subscribed { project_id };
    Ok(send_under(socket, key, &subscribed).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delta(attempt: Uuid, version: u64) -> LiveFrame {
        LiveFrame::Delta {
            project_id: Uuid::nil(),
            attempt_id: attempt,
            version,
            entities: Vec::new(),
        }
    }

    #[test]
    fn an_attempt_wants_its_own_deltas_past_its_snapshot_and_no_samples() {
        let (attempt, other) = (Uuid::from_u128(1), Uuid::from_u128(2));
        let sub = Sub {
            project: Uuid::nil(),
            attempt: Some((attempt, 6)),
        };
        assert!(!sub.wants(&delta(attempt, 6)), "6 is in the snapshot");
        assert!(sub.wants(&delta(attempt, 7)));
        assert!(!sub.wants(&delta(other, 7)), "another attempt's change");
        assert!(!sub.wants(&LiveFrame::Samples {
            project_id: Uuid::nil()
        }));
        let resync = LiveFrame::Resync {
            reason: ResyncReason::Lagged,
        };
        assert!(sub.wants(&resync), "a resync is for every subscription");
    }

    #[test]
    fn a_project_wants_every_frame_of_its_channel() {
        let sub = Sub {
            project: Uuid::nil(),
            attempt: None,
        };
        assert!(sub.wants(&delta(Uuid::from_u128(9), 1)));
        assert!(sub.wants(&LiveFrame::Samples {
            project_id: Uuid::nil()
        }));
    }

    #[test]
    fn a_topic_names_its_subscription_when_the_client_chose_no_id() {
        let attempt = Uuid::from_u128(3);
        assert_eq!(topic_key("app", None), "app");
        assert_eq!(topic_key("app", Some(attempt)), format!("app/{attempt}"));
    }

    #[test]
    fn the_cap_refuses_a_new_key_but_not_a_replacement() {
        let mut subs = Subscriptions::default();
        for n in 0..MAX_SUBSCRIPTIONS {
            subs.subs.insert(
                format!("s{n}"),
                Sub {
                    project: Uuid::nil(),
                    attempt: None,
                },
            );
        }
        assert!(subs.admit("s0").is_ok(), "replacing one is not a new one");
        let refused = subs.admit("one-more").unwrap_err();
        assert!(refused.contains("32"), "{refused}");
    }

    #[test]
    fn a_frame_is_tagged_with_its_subscription_first() {
        let frame = LiveFrame::Unsubscribed;
        let text = serde_json::to_string(&Out {
            sub: "runner",
            frame: &frame,
        })
        .unwrap();
        assert_eq!(text, r#"{"sub":"runner","op":"unsubscribed"}"#);
    }

    #[tokio::test]
    async fn a_lagging_project_channel_is_reported_once_per_lag_and_frames_keep_their_order() {
        let (tx, rx) = broadcast::channel::<Arc<LiveFrame>>(2);
        let project = Uuid::from_u128(7);
        let mut subs = Subscriptions::default();
        subs.channels.insert(project, rx);
        for v in 1..=5 {
            tx.send(Arc::new(delta(Uuid::nil(), v))).unwrap();
        }
        assert!(
            matches!(subs.next().await, Next::Lagged(p) if p == project),
            "3 of 5 were lost"
        );
        for want in [4, 5] {
            let Next::Frame(_, frame) = subs.next().await else {
                panic!("the newest two frames are still buffered");
            };
            assert_eq!(*frame, delta(Uuid::nil(), want));
        }
        drop(tx);
        assert!(matches!(subs.next().await, Next::Closed));
    }
}

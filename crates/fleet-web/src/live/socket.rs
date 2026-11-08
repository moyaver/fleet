// SPDX-License-Identifier: AGPL-3.0-only
//! The browser's one live socket for the whole app (PLAN §10.1): every page that follows
//! something subscribes to it, and the subscription goes away when the page does.
//!
//! The first page that subscribes opens the socket to `/api/v1/live`, and it stays open for the
//! life of the app. Each subscription is a key on it ([`super::registry`]): the server names every
//! frame with the key, and the frame goes to that page's callback once per animation frame. A
//! reconnect (after the socket was lost, with the same backoff as before) subscribes every active
//! key again, so each page gets a fresh snapshot. A version gap or a `resync` on one key subscribes
//! that key alone.
//!
//! A socket the server refuses with `429` cannot say so: a browser does not show the status of a
//! refused upgrade. The reconnect that never got live therefore asks the same credential's REST
//! read how long to wait, and waits at least that long.

use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
};

use fleet_protocol::live::{LiveFrame, LiveRequest, Routed};
use futures_channel::mpsc::{UnboundedSender, unbounded};
use futures_util::{SinkExt as _, StreamExt as _};
use gloo_net::websocket::{Message, futures::WebSocket};
use gloo_timers::future::TimeoutFuture;
use leptos::{prelude::*, task::spawn_local};

use crate::{
    api::{self, ApiError},
    live::{
        backoff::Backoff,
        queue::FrameQueue,
        registry::Registry,
        versions::{Contiguity, VersionWatch},
    },
    paths::live_url,
};

pub use crate::live::registry::Subscription;

/// Where a subscription stands, for display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Connection {
    /// The first connection is being made.
    Connecting,
    /// Subscribed: frames are arriving.
    Live,
    /// The socket was lost; the page shows the last state it had until a new snapshot arrives.
    Reconnecting,
}

impl Connection {
    /// The words shown to the person.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Connecting => "Connecting",
            Self::Live => "Live",
            Self::Reconnecting => "Reconnecting",
        }
    }
}

/// What a subscription's owner is told.
#[derive(Debug)]
pub enum Event {
    /// Frames received since the last report, in arrival order.
    Frames(Vec<LiveFrame>),
    /// The socket closed: versions may have been missed.
    Lost,
}

/// A page's subscription. Dropping it has no effect: the page's cleanup stops the subscription,
/// which happens when the page unmounts.
#[derive(Debug, Clone, Copy)]
pub struct LiveHandle {
    /// Where this subscription stands.
    pub connection: ReadSignal<Connection>,
}

/// What the hub keeps for one active subscription.
struct Entry {
    connection: WriteSignal<Connection>,
    on_event: Rc<dyn Fn(Event)>,
    queue: Rc<RefCell<FrameQueue>>,
    versions: VersionWatch,
}

/// The app's one hub. The browser runs one thread, so it lives in a thread-local.
#[derive(Default)]
struct Hub {
    registry: RefCell<Registry>,
    entries: RefCell<BTreeMap<String, Entry>>,
    /// Requests for the open socket; `None` while there is no socket.
    outbox: RefCell<Option<UnboundedSender<String>>>,
    /// The connection loop has started. It starts once, with the first subscription.
    started: Cell<bool>,
    /// The open socket has confirmed a subscription, so the next reconnect starts its backoff over.
    reached_live: Cell<bool>,
}

thread_local! {
    static HUB: Hub = Hub::default();
}

/// Subscribes the calling page to `subscription`. The page's frames go to `on_event`, once per
/// animation frame. The subscription ends when the current reactive owner is cleaned up.
pub fn connect(subscription: Subscription, on_event: impl Fn(Event) + 'static) -> LiveHandle {
    let (connection, set_connection) = signal(Connection::Connecting);
    let on_event: Rc<dyn Fn(Event)> = Rc::new(on_event);
    let key = HUB.with(|hub| {
        let key = hub.registry.borrow_mut().issue_key();
        hub.registry.borrow_mut().add(key.clone(), subscription);
        hub.entries.borrow_mut().insert(
            key.clone(),
            Entry {
                connection: set_connection,
                on_event,
                queue: Rc::default(),
                versions: VersionWatch::default(),
            },
        );
        hub.send_subscribe(&key);
        key
    });
    start();
    on_cleanup(move || unsubscribe(&key));
    LiveHandle { connection }
}

/// Starts the connection loop, once.
fn start() {
    if HUB.with(|hub| !hub.started.replace(true)) {
        spawn_local(run());
    }
}

/// Stops `key`: its page is gone, so its frames go nowhere and the server stops sending them.
fn unsubscribe(key: &str) {
    HUB.with(|hub| {
        let was_active = hub.registry.borrow_mut().remove(key);
        hub.entries.borrow_mut().remove(key);
        if was_active {
            hub.send(&LiveRequest::Unsubscribe { id: key.to_owned() });
        }
    });
}

impl Hub {
    /// Sends `request` on the open socket, if there is one.
    fn send(&self, request: &LiveRequest) {
        let outbox = self.outbox.borrow();
        let Some(tx) = outbox.as_ref() else {
            return;
        };
        if let Ok(text) = serde_json::to_string(request) {
            // The socket's writer is gone only when the socket is: the next one subscribes again.
            let _ = tx.unbounded_send(text);
        }
    }

    /// Asks the open socket for the subscription under `key`, with a fresh snapshot.
    fn send_subscribe(&self, key: &str) {
        if let Some(request) = self.registry.borrow().request(key) {
            self.send(&request);
        }
    }

    /// Sends one frame of a subscription's stream to its page, or queues it for the next animation
    /// frame. Versions are checked first: a gap or a `resync` asks for a new snapshot of that key.
    fn route(&self, routed: Routed) {
        // A request the server could not read answers no subscription.
        let Some(key) = routed.sub else {
            return;
        };
        let frame = routed.frame;
        let resubscribe = {
            let mut entries = self.entries.borrow_mut();
            let Some(entry) = entries.get_mut(&key) else {
                // Frames of a subscription that has ended (an unsubscribe in flight) go nowhere.
                return;
            };
            match frame {
                LiveFrame::Unsubscribed => return,
                LiveFrame::Subscribed { .. } => {
                    self.reached_live.set(true);
                    let _ = entry.connection.try_set(Connection::Live);
                }
                _ => {}
            }
            let resync = matches!(frame, LiveFrame::Resync { .. })
                || entry.versions.observe(&frame) == Contiguity::Gap;
            let must_schedule = entry.queue.borrow_mut().push(frame);
            if must_schedule {
                let key = key.clone();
                request_animation_frame(move || flush(&key));
            }
            resync
        };
        if resubscribe {
            self.send_subscribe(&key);
        }
    }

    /// The socket is gone: every page gets what was queued, then `Lost`, and shows itself as
    /// reconnecting until its subscription is confirmed again.
    fn lost(&self) {
        // Gathered before any callback runs: a page's callback may not reach back into the hub.
        let pending: Vec<Pending> = self
            .entries
            .borrow_mut()
            .values_mut()
            .map(|entry| {
                entry.versions = VersionWatch::default();
                let frames = entry.queue.borrow_mut().take();
                (entry.connection, Rc::clone(&entry.on_event), frames)
            })
            .collect();
        for (connection, on_event, frames) in pending {
            let _ = connection.try_set(Connection::Reconnecting);
            if !frames.is_empty() {
                on_event(Event::Frames(frames));
            }
            on_event(Event::Lost);
        }
    }

    /// Every subscription shows `state`.
    fn show_all(&self, state: Connection) {
        let connections: Vec<WriteSignal<Connection>> = self
            .entries
            .borrow()
            .values()
            .map(|entry| entry.connection)
            .collect();
        for connection in connections {
            let _ = connection.try_set(state);
        }
    }
}

/// What [`Hub::lost`] hands to one page: its connection signal, its callback and its queued frames.
type Pending = (WriteSignal<Connection>, Rc<dyn Fn(Event)>, Vec<LiveFrame>);

/// Hands a subscription's queued frames to its page.
fn flush(key: &str) {
    let Some((on_event, queue)) = HUB.with(|hub| {
        hub.entries
            .borrow()
            .get(key)
            .map(|entry| (Rc::clone(&entry.on_event), Rc::clone(&entry.queue)))
    }) else {
        return;
    };
    let frames = queue.borrow_mut().take();
    if !frames.is_empty() {
        on_event(Event::Frames(frames));
    }
}

/// Keeps the socket open: connects, reads, and after a loss waits and connects again.
async fn run() {
    let mut backoff = Backoff::default();
    let mut first = true;
    loop {
        HUB.with(|hub| {
            hub.show_all(if first {
                Connection::Connecting
            } else {
                Connection::Reconnecting
            });
            hub.reached_live.set(false);
        });
        first = false;
        session().await;
        HUB.with(Hub::lost);
        let reached_live = HUB.with(|hub| hub.reached_live.get());
        if reached_live {
            backoff.reset();
        }
        let delay = backoff.next_delay_ms(js_sys::Math::random());
        let delay = if reached_live {
            delay
        } else {
            delay.max(refused_for_ms().await)
        };
        TimeoutFuture::new(u32::try_from(delay).unwrap_or(u32::MAX)).await;
    }
}

/// How long the server asks us to wait before the next connect, in milliseconds: the `Retry-After`
/// of a REST read with the same credential, which the server answers with the same `429` it gave
/// the refused upgrade. `0` when that read is not refused, or cannot be made.
async fn refused_for_ms() -> u64 {
    match api::projects().await {
        Err(ApiError::RateLimited { retry_after_secs }) => retry_after_secs.saturating_mul(1000),
        _ => 0,
    }
}

/// One socket, from its opening to its close. Every active subscription is sent first, under its
/// key, so each page gets a snapshot.
async fn session() {
    let location = window().location();
    let url = live_url(
        &location.protocol().unwrap_or_default(),
        &location.host().unwrap_or_default(),
    );
    let Ok(socket) = WebSocket::open(&url) else {
        return;
    };
    let (mut write, mut read) = socket.split();
    let (tx, mut rx) = unbounded::<String>();
    HUB.with(|hub| {
        for request in hub.registry.borrow().requests() {
            if let Ok(text) = serde_json::to_string(&request) {
                let _ = tx.unbounded_send(text);
            }
        }
        *hub.outbox.borrow_mut() = Some(tx);
    });
    // The writer ends when the outbox is dropped at the end of this session.
    spawn_local(async move {
        while let Some(text) = rx.next().await {
            if write.send(Message::Text(text)).await.is_err() {
                break;
            }
        }
    });
    while let Some(message) = read.next().await {
        let Ok(Message::Text(text)) = message else {
            if message.is_err() {
                break;
            }
            continue;
        };
        // A frame this build can't read is skipped: an older dashboard keeps working against a
        // server that learned a new frame (PLAN §8 compatibility).
        let Ok(routed) = serde_json::from_str::<Routed>(&text) else {
            continue;
        };
        HUB.with(|hub| hub.route(routed));
    }
    HUB.with(|hub| *hub.outbox.borrow_mut() = None);
}

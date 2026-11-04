// SPDX-License-Identifier: AGPL-3.0-only
//! The browser side of `/api/v1/live`: one socket for one subscription, reconnecting with
//! backoff, handing frames over once per animation frame.
//!
//! The server takes a single `subscribe` per socket (the first message), so a page that follows
//! several things opens several of these. A `resync` frame, a refusal or a closed socket all end
//! in a new socket and a new subscribe, which brings a fresh snapshot.

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use fleet_protocol::live::{LiveFrame, LiveRequest};
use futures_util::{
    SinkExt as _, StreamExt as _,
    future::{AbortHandle, abortable},
};
use gloo_net::websocket::{Message, futures::WebSocket};
use gloo_timers::future::TimeoutFuture;
use leptos::{prelude::*, task::spawn_local};
use uuid::Uuid;

use crate::{
    live::{backoff::Backoff, queue::FrameQueue},
    paths::live_url,
};

/// What a socket follows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subscription {
    /// The project's slug.
    pub project: String,
    /// The attempt to receive a snapshot of, if any; deltas of the whole project arrive anyway.
    pub attempt: Option<Uuid>,
}

/// Where the socket stands, for display.
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

/// What a socket reports to its owner.
#[derive(Debug)]
pub enum Event {
    /// Frames received since the last report, in arrival order.
    Frames(Vec<LiveFrame>),
    /// The socket closed: versions may have been missed.
    Lost,
}

/// A running socket; dropping it (or [`LiveHandle::close`]) stops it.
#[derive(Debug, Clone)]
pub struct LiveHandle {
    /// Where the socket stands.
    pub connection: ReadSignal<Connection>,
    abort: AbortHandle,
    closed: Arc<AtomicBool>,
}

impl LiveHandle {
    /// Closes the socket and stops reconnecting.
    pub fn close(&self) {
        // Frames already waiting for their animation frame must not reach a page that is gone.
        self.closed.store(true, Ordering::Relaxed);
        self.abort.abort();
    }
}

/// Opens the socket for `subscription` and keeps it open, calling `on_event` from the main loop
/// (frames once per animation frame). Closes when the current reactive owner is cleaned up.
pub fn connect(subscription: Subscription, on_event: impl Fn(Event) + 'static) -> LiveHandle {
    let (connection, set_connection) = signal(Connection::Connecting);
    let closed = Arc::new(AtomicBool::new(false));
    let guarded = {
        let closed = Arc::clone(&closed);
        move |event| {
            if !closed.load(Ordering::Relaxed) {
                on_event(event);
            }
        }
    };
    let on_event: Rc<dyn Fn(Event)> = Rc::new(guarded);
    let (task, abort) = abortable(run(subscription, on_event, set_connection));
    spawn_local(async move {
        let _ = task.await;
    });
    let handle = LiveHandle {
        connection,
        abort,
        closed,
    };
    let to_close = handle.clone();
    on_cleanup(move || to_close.close());
    handle
}

async fn run(
    subscription: Subscription,
    on_event: Rc<dyn Fn(Event)>,
    set_connection: WriteSignal<Connection>,
) {
    let queue = Rc::new(RefCell::new(FrameQueue::default()));
    let mut backoff = Backoff::default();
    let mut first = true;
    loop {
        set_connection.try_set(if first {
            Connection::Connecting
        } else {
            Connection::Reconnecting
        });
        first = false;
        let reached_live = Rc::new(Cell::new(false));
        let ended = session(
            &subscription,
            &queue,
            &on_event,
            set_connection,
            &reached_live,
        )
        .await;
        // Hand over what arrived before the socket went, then tell the owner it is gone.
        flush(&queue, &on_event);
        on_event(Event::Lost);
        if reached_live.get() {
            backoff.reset();
        }
        let delay = match ended {
            // A resync is the server asking for a new snapshot, not a failure: ask at once.
            Ended::Resync => 0,
            Ended::Closed => backoff.next_delay_ms(js_sys::Math::random()),
        };
        TimeoutFuture::new(u32::try_from(delay).unwrap_or(u32::MAX)).await;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ended {
    Resync,
    Closed,
}

async fn session(
    subscription: &Subscription,
    queue: &Rc<RefCell<FrameQueue>>,
    on_event: &Rc<dyn Fn(Event)>,
    set_connection: WriteSignal<Connection>,
    reached_live: &Rc<Cell<bool>>,
) -> Ended {
    let location = window().location();
    let url = live_url(
        &location.protocol().unwrap_or_default(),
        &location.host().unwrap_or_default(),
    );
    let Ok(socket) = WebSocket::open(&url) else {
        return Ended::Closed;
    };
    let (mut write, mut read) = socket.split();
    let request = LiveRequest::Subscribe {
        project: subscription.project.clone(),
        attempt: subscription.attempt,
    };
    let Ok(text) = serde_json::to_string(&request) else {
        return Ended::Closed;
    };
    if write.send(Message::Text(text)).await.is_err() {
        return Ended::Closed;
    }
    while let Some(message) = read.next().await {
        let Ok(Message::Text(text)) = message else {
            if message.is_err() {
                break;
            }
            continue;
        };
        // A frame this build can't read is skipped: an older dashboard keeps working against a
        // server that learned a new frame (PLAN §8 compatibility).
        let Ok(frame) = serde_json::from_str::<LiveFrame>(&text) else {
            continue;
        };
        if matches!(frame, LiveFrame::Subscribed { .. }) {
            reached_live.set(true);
            set_connection.try_set(Connection::Live);
        }
        let resync = matches!(frame, LiveFrame::Resync { .. });
        let must_schedule = queue.borrow_mut().push(frame);
        if must_schedule {
            let (queue, on_event) = (Rc::clone(queue), Rc::clone(on_event));
            request_animation_frame(move || flush(&queue, &on_event));
        }
        if resync {
            return Ended::Resync;
        }
    }
    Ended::Closed
}

fn flush(queue: &Rc<RefCell<FrameQueue>>, on_event: &Rc<dyn Fn(Event)>) {
    let frames = queue.borrow_mut().take();
    if !frames.is_empty() {
        on_event(Event::Frames(frames));
    }
}

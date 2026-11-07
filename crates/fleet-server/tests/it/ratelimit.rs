// SPDX-License-Identifier: AGPL-3.0-only
//! Rate limits over the wire (research 03 §11; issue #35): per-IP limits on the pre-auth token
//! exchange (trusted `X-Forwarded-For` only from a trusted proxy), per-credential limits on agent
//! connections and on viewer reads and live upgrades, and events paced on a socket, never dropped.

use std::{
    net::SocketAddr,
    time::{Duration, Instant},
};

use fleet_protocol::{brand::WS_SUBPROTOCOL, frame::BatchRequest};
use reqwest::{Response, StatusCode};
use tokio_tungstenite::tungstenite::{
    Error as WsError, client::IntoClientRequest, http::HeaderValue,
};

use crate::support::{Agent, Harness, VIEWER};

const TOKEN: &str = "/api/v1/agent/token";

/// Three requests from one client, then a fourth that is refused with `Retry-After`.
fn tight_ip_limit(c: &mut fleet_server::config::Config) {
    c.rate_limits.ip_burst = 3;
    c.rate_limits.ip_requests_per_minute = 1;
}

async fn token_request(h: &Harness, forwarded_for: Option<&str>) -> Response {
    let mut req = reqwest::Client::new().post(format!("http://{}{TOKEN}", h.addr()));
    if let Some(value) = forwarded_for {
        req = req.header("x-forwarded-for", value);
    }
    req.send().await.unwrap()
}

fn retry_after(res: &Response) -> u64 {
    res.headers()["retry-after"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap()
}

#[tokio::test]
async fn the_token_endpoint_limits_one_client_ip_and_says_when_to_retry() {
    let h = Harness::start_with(tight_ip_limit).await;
    for _ in 0..3 {
        // No credential: the request is answered, just not accepted.
        assert_eq!(
            token_request(&h, None).await.status(),
            StatusCode::UNAUTHORIZED
        );
    }
    let refused = token_request(&h, None).await;
    assert_eq!(refused.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(retry_after(&refused), 60, "one token a minute");
}

#[tokio::test]
async fn behind_a_trusted_proxy_each_forwarded_client_gets_its_own_bucket() {
    let h = Harness::start_with(|c| {
        tight_ip_limit(c);
        // The test client connects from 127.0.0.1, so that is the proxy.
        c.trusted_proxies = vec!["127.0.0.0/8".parse().unwrap()];
    })
    .await;
    for _ in 0..3 {
        assert_eq!(
            token_request(&h, Some("203.0.113.1")).await.status(),
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        token_request(&h, Some("203.0.113.1")).await.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "the first client is over its limit"
    );
    assert_eq!(
        token_request(&h, Some("203.0.113.2")).await.status(),
        StatusCode::UNAUTHORIZED,
        "a second client behind the same proxy is not"
    );
}

#[tokio::test]
async fn a_spoofed_forwarded_for_from_an_untrusted_peer_does_not_escape_the_limit() {
    let h = Harness::start_with(|c| {
        tight_ip_limit(c);
        // The peer, 127.0.0.1, is not a proxy: the header is ignored.
        c.trusted_proxies = vec!["10.0.0.0/8".parse().unwrap()];
    })
    .await;
    for n in 0..3 {
        let forged = format!("198.51.100.{n}");
        assert_eq!(
            token_request(&h, Some(&forged)).await.status(),
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        token_request(&h, Some("198.51.100.99")).await.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "every forged address is the same peer"
    );
}

#[tokio::test]
async fn a_credential_is_limited_on_batch_connections() {
    let h = Harness::start_with(|c| {
        c.rate_limits.credential_burst = 2;
        c.rate_limits.credential_requests_per_minute = 1;
    })
    .await;
    let body = serde_json::to_string(&BatchRequest {
        hello: Agent::new("rl", "rl").hello(None),
        events: Vec::new(),
        telemetry: Vec::new(),
    })
    .unwrap();
    let post = || {
        reqwest::Client::new()
            .post(format!("http://{}/api/v1/agent/batch", h.addr()))
            .bearer_auth(&h.key)
            .header("content-type", "application/json")
            .body(body.clone())
            .send()
    };
    assert_eq!(post().await.unwrap().status(), StatusCode::OK);
    assert_eq!(post().await.unwrap().status(), StatusCode::OK);
    let refused = post().await.unwrap();
    assert_eq!(refused.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(refused.headers().contains_key("retry-after"));
}

#[tokio::test]
async fn a_credential_is_limited_on_socket_upgrades() {
    let h = Harness::start_with(|c| {
        c.rate_limits.credential_burst = 2;
        c.rate_limits.credential_requests_per_minute = 1;
    })
    .await;
    let connect = || {
        let mut req = format!("ws://{}/api/v1/agent/ws", h.addr())
            .into_client_request()
            .unwrap();
        req.headers_mut().insert(
            "Sec-WebSocket-Protocol",
            HeaderValue::from_static(WS_SUBPROTOCOL),
        );
        req.headers_mut().insert(
            "Authorization",
            HeaderValue::from_str(&format!("Bearer {}", h.key)).unwrap(),
        );
        tokio_tungstenite::connect_async(req)
    };
    assert!(connect().await.is_ok());
    assert!(connect().await.is_ok());
    let Err(WsError::Http(refused)) = connect().await else {
        panic!("the third upgrade is refused");
    };
    assert_eq!(refused.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(refused.headers().contains_key("retry-after"));
}

#[tokio::test]
async fn a_fast_agent_is_slowed_and_no_event_is_dropped() {
    // 4 events a second after a burst of one. A frame is read only once the debt of the frames
    // before it is repaid, so the last frame cannot be read before (total - last - 1) / 4 s. The
    // unpaced server finishes in a fraction of that, even on a loaded machine.
    let h = Harness::start_with(|c| {
        c.rate_limits.events_per_second = 4;
        c.rate_limits.event_burst = 1;
    })
    .await;
    let mut agent = Agent::new("pace", "pace");
    let flows: Vec<(&str, bool)> = (0..20).map(|_| ("login", true)).collect();
    let events = agent.suite(&flows);
    let total = events.len();
    let last = events.chunks(16).last().unwrap().len();
    assert!(total > 40, "a long enough stream: {total}");

    let mut c = h.connect(None).await;
    c.hello(&agent, Some(1)).await;
    let started = Instant::now();
    let mut through = 0;
    for chunk in events.chunks(16) {
        let (acked, _) = c.events(chunk.to_vec()).await;
        through = acked;
    }
    let elapsed = started.elapsed();

    assert_eq!(through, agent.seq, "every event is acknowledged");
    // Four events a second: a quarter second each.
    let floor = Duration::from_millis(250 * u64::try_from(total - last - 1).unwrap());
    assert!(elapsed >= floor, "{elapsed:?} is under {floor:?}");
}

/// Viewer reads and live upgrades share one bucket per viewer token.
fn tight_viewer_limit(c: &mut fleet_server::config::Config) {
    c.rate_limits.viewer_burst = 2;
    c.rate_limits.viewer_requests_per_minute = 1;
}

/// Opens `/api/v1/live` as `token`; the socket is dropped at once.
async fn live_upgrade(addr: SocketAddr, token: &str) -> Result<(), WsError> {
    let mut req = format!("ws://{addr}/api/v1/live")
        .into_client_request()
        .unwrap();
    req.headers_mut().insert(
        "Authorization",
        HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
    );
    tokio_tungstenite::connect_async(req).await.map(|_| ())
}

#[tokio::test]
async fn a_viewer_token_is_limited_on_reads_and_says_when_to_retry() {
    let h = Harness::start_with(tight_viewer_limit).await;
    let read = || {
        reqwest::Client::new()
            .get(format!("http://{}/api/v1/projects", h.addr()))
            .bearer_auth(VIEWER)
            .send()
    };
    assert_eq!(read().await.unwrap().status(), StatusCode::OK);
    assert_eq!(read().await.unwrap().status(), StatusCode::OK);
    let refused = read().await.unwrap();
    assert_eq!(refused.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(retry_after(&refused), 60, "one token a minute");
}

#[tokio::test]
async fn a_viewer_token_is_limited_on_live_upgrades_and_says_when_to_retry() {
    let h = Harness::start_with(tight_viewer_limit).await;
    assert!(live_upgrade(h.addr(), VIEWER).await.is_ok());
    assert!(live_upgrade(h.addr(), VIEWER).await.is_ok());
    let Err(WsError::Http(refused)) = live_upgrade(h.addr(), VIEWER).await else {
        panic!("the third live upgrade is refused");
    };
    assert_eq!(refused.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(refused.headers().contains_key("retry-after"));
}

#[tokio::test]
async fn two_viewer_tokens_have_separate_buckets() {
    let h = Harness::start_with(|c| {
        tight_viewer_limit(c);
        c.api
            .viewer_tokens
            .push(fleet_server::config::Secret::from("second-viewer"));
    })
    .await;
    let read = |token: &'static str| {
        reqwest::Client::new()
            .get(format!("http://{}/api/v1/projects", h.addr()))
            .bearer_auth(token)
            .send()
    };
    for _ in 0..2 {
        assert_eq!(read(VIEWER).await.unwrap().status(), StatusCode::OK);
    }
    assert_eq!(
        read(VIEWER).await.unwrap().status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        read("second-viewer").await.unwrap().status(),
        StatusCode::OK,
        "the other viewer has its own bucket"
    );
}

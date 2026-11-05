// SPDX-License-Identifier: AGPL-3.0-only
//! The agent's credentials and network path (issues #24, #25): GitHub OIDC exchange, and a
//! WebSocket through an HTTP CONNECT proxy.

use std::sync::atomic::Ordering;

use serde_json::json;

use crate::support::{Issuer, Stack, connect_proxy, flows, run_agent};

#[tokio::test(flavor = "multi_thread")]
async fn the_agent_exchanges_its_github_oidc_token_and_claims_name_the_job() {
    let issuer = Issuer::start(json!({})).await;
    let stack = Stack::start(|c| issuer.configure(c)).await;
    let request_url = format!("{}/token?api-version=2.0", issuer.url);
    let ws = stack.ws_url();
    let out = tokio::task::spawn_blocking(move || {
        run_agent(
            "pass",
            &[
                ("FLEET_SERVER", &ws),
                ("GITHUB_ACTIONS", "true"),
                ("GITHUB_JOB", "e2e"),
                ("ACTIONS_ID_TOKEN_REQUEST_URL", &request_url),
                ("ACTIONS_ID_TOKEN_REQUEST_TOKEN", "runner-request-token"),
            ],
        )
    })
    .await
    .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "Maestro's exit status is untouched (I1)"
    );
    assert!(
        issuer.requests.load(Ordering::SeqCst) >= 1,
        "the agent asked the runner for a token"
    );
    let snap = stack.latest_snapshot().await;
    assert_eq!(
        snap["attempt"]["run_key"], "acme/app:9001",
        "from the verified claims"
    );
    assert_eq!(
        snap["jobs"][0]["job_key"], "31337",
        "check_run_id is the job key"
    );
    assert_eq!(snap["agent_runs"][0]["trust_level"], "trusted");
    assert_eq!(snap["agent_runs"][0]["state"], "ended");
    assert_ne!(flows(&snap), vec![], "the run's flows arrived");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_oidc_token_never_affects_maestro() {
    // The token says pull_request_target: refused for good. Telemetry stops; the run doesn't.
    let issuer = Issuer::start(json!({"event_name": "pull_request_target"})).await;
    let stack = Stack::start(|c| issuer.configure(c)).await;
    let request_url = format!("{}/token", issuer.url);
    let ws = stack.ws_url();
    let out = tokio::task::spawn_blocking(move || {
        run_agent(
            "fail",
            &[
                ("FLEET_SERVER", &ws),
                ("ACTIONS_ID_TOKEN_REQUEST_URL", &request_url),
                ("ACTIONS_ID_TOKEN_REQUEST_TOKEN", "runner-request-token"),
                ("FLEET_FLUSH_TIMEOUT", "3"),
            ],
        )
    })
    .await
    .unwrap();
    assert_eq!(
        out.status.code(),
        Some(1),
        "the failing fixture still fails, nothing more (I1, I2)"
    );
    let attempts = stack.get("/api/v1/projects/app/attempts").await;
    assert_eq!(attempts, json!([]), "nothing was published");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_agent_tunnels_through_an_http_proxy() {
    let stack = Stack::start(|_| {}).await;
    let (proxy, tunnels) = connect_proxy().await;
    let proxy_url = format!("http://{proxy}");
    let (ws, key) = (stack.ws_url(), stack.key.clone());
    let out = tokio::task::spawn_blocking(move || {
        run_agent(
            "pass",
            &[
                ("FLEET_SERVER", &ws),
                ("FLEET_TOKEN", &key),
                ("HTTP_PROXY", &proxy_url),
            ],
        )
    })
    .await
    .unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(
        tunnels.load(Ordering::SeqCst) >= 1,
        "the WebSocket went through CONNECT"
    );
    let snap = stack.latest_snapshot().await;
    assert_eq!(
        snap["agent_runs"][0]["state"], "ended",
        "the run arrived whole"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn agent_batch_fallback_reaches_the_real_server_and_finishes() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let stack = Stack::start(|_| {}).await;
    let upstream = stack.addr();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let telemetry_acks = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen_acks = std::sync::Arc::clone(&telemetry_acks);
    let proxy = tokio::spawn(async move {
        loop {
            let (mut client, _) = listener.accept().await.unwrap();
            let seen_acks = std::sync::Arc::clone(&seen_acks);
            tokio::spawn(async move {
                let mut head = Vec::new();
                while !head.ends_with(b"\r\n\r\n") {
                    match client.read_u8().await {
                        Ok(byte) => head.push(byte),
                        Err(_) => return,
                    }
                }
                if head.starts_with(b"GET ") {
                    let _ = client.write_all(b"HTTP/1.1 426 Upgrade Required\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
                } else {
                    let mut server = tokio::net::TcpStream::connect(upstream).await.unwrap();
                    server.write_all(&head).await.unwrap();
                    let (mut client_read, mut client_write) = client.split();
                    let (mut server_read, mut server_write) = server.split();
                    let forward = tokio::io::copy(&mut client_read, &mut server_write);
                    let response = async {
                        let mut buffer = [0; 8192];
                        let mut captured = Vec::new();
                        loop {
                            let n = server_read.read(&mut buffer).await.unwrap_or(0);
                            if n == 0 {
                                break;
                            }
                            captured.extend_from_slice(&buffer[..n]);
                            if String::from_utf8_lossy(&captured).contains("telemetry_ack") {
                                seen_acks.fetch_add(1, Ordering::SeqCst);
                                captured.clear();
                            }
                            if client_write.write_all(&buffer[..n]).await.is_err() {
                                break;
                            }
                        }
                    };
                    tokio::select! { _ = forward => {}, () = response => {} }
                }
            });
        }
    });
    let url = format!("ws://{address}/api/v1/agent/ws");
    let key = stack.key.clone();
    let out = tokio::task::spawn_blocking(move || {
        run_agent(
            "pass",
            &[
                ("FLEET_SERVER", &url),
                ("FLEET_TOKEN", &key),
                ("FLEET_FAKE_LINGER", "8"),
                ("FLEET_FLUSH_TIMEOUT", "15"),
                ("FLEET_UPLOAD_ARTIFACTS", "off"),
            ],
        )
    })
    .await
    .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let snapshot = stack.latest_snapshot().await;
    assert_eq!(snapshot["agent_runs"][0]["state"], "ended");
    assert!(!flows(&snapshot).is_empty());
    assert!(
        telemetry_acks.load(Ordering::SeqCst) > 0,
        "server acknowledged telemetry"
    );
    proxy.abort();
}

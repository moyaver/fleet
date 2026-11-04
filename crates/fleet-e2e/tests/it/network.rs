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

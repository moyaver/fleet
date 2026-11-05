// SPDX-License-Identifier: AGPL-3.0-only
//! Live subscribers recover after a committed delta cannot be loaded.

use std::sync::atomic::Ordering;

use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest as _, http::HeaderValue};

use super::rig::{Agent, VIEWER, rig};

#[tokio::test]
async fn a_failed_delta_load_resyncs_a_subscriber_to_the_committed_state() {
    let rig = rig().await;
    let mut agent = Agent::new("recovery", "job", "runner");
    agent.started();
    rig.publish(&mut agent, vec![]).await;
    let attempt = rig.attempt_id("app").await;
    let address = rig.app.server_address().unwrap();
    let connect = || async {
        let mut request = format!(
            "ws://{}:{}/api/v1/live",
            address.host_str().unwrap(),
            address.port().unwrap()
        )
        .into_client_request()
        .unwrap();
        request.headers_mut().insert(
            "Authorization",
            HeaderValue::from_str(&format!("Bearer {VIEWER}")).unwrap(),
        );
        let (mut ws, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        ws.send(Message::text(
            json!({"op":"subscribe", "project":"app", "attempt":attempt}).to_string(),
        ))
        .await
        .unwrap();
        ws
    };
    let next = |text: Message| -> Value { serde_json::from_str(text.to_text().unwrap()).unwrap() };
    let mut ws = connect().await;
    let before = next(ws.next().await.unwrap().unwrap());
    assert_eq!(before["op"], "snapshot");
    assert_eq!(next(ws.next().await.unwrap().unwrap())["op"], "subscribed");

    rig.state.hub.fail_entity_load.store(true, Ordering::SeqCst);
    agent.ended();
    rig.publish(&mut agent, vec![]).await;
    let frame = tokio::time::timeout(std::time::Duration::from_secs(10), ws.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(next(frame)["op"], "resync");

    let mut ws = connect().await;
    let after = next(ws.next().await.unwrap().unwrap());
    assert_eq!(after["snapshot"]["agent_runs"][0]["state"], "ended");
    assert!(
        after["snapshot"]["attempt"]["version"].as_u64().unwrap()
            > before["snapshot"]["attempt"]["version"].as_u64().unwrap()
    );
}

#[tokio::test]
async fn database_failures_are_not_reported_as_missing_live_resources() {
    let rig = rig().await;
    let mut agent = Agent::new("db-error", "job", "runner");
    agent.started();
    rig.publish(&mut agent, vec![]).await;
    let attempt = rig.attempt_id("app").await;
    let address = rig.app.server_address().unwrap();
    let db = sqlx::PgPool::connect(rig.state.config.database.url.as_ref().unwrap().expose())
        .await
        .unwrap();
    let subscribe = || async {
        let mut request = format!(
            "ws://{}:{}/api/v1/live",
            address.host_str().unwrap(),
            address.port().unwrap()
        )
        .into_client_request()
        .unwrap();
        request.headers_mut().insert(
            "Authorization",
            HeaderValue::from_str(&format!("Bearer {VIEWER}")).unwrap(),
        );
        let (mut ws, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        ws.send(Message::text(
            json!({"op":"subscribe", "project":"app", "attempt":attempt}).to_string(),
        ))
        .await
        .unwrap();
        let msg = tokio::time::timeout(std::time::Duration::from_secs(10), ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        serde_json::from_str::<Value>(msg.to_text().unwrap()).unwrap()
    };
    sqlx::query("ALTER TABLE projects RENAME TO projects_hidden")
        .execute(&db)
        .await
        .unwrap();
    let project_error = subscribe().await;
    sqlx::query("ALTER TABLE projects_hidden RENAME TO projects")
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(project_error["op"], "error");
    assert_eq!(
        project_error["message"],
        "live data temporarily unavailable; retry"
    );

    sqlx::query("ALTER TABLE pipeline_attempts RENAME TO attempts_hidden")
        .execute(&db)
        .await
        .unwrap();
    let attempt_error = subscribe().await;
    sqlx::query("ALTER TABLE attempts_hidden RENAME TO pipeline_attempts")
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(attempt_error["op"], "error");
    assert_eq!(
        attempt_error["message"],
        "live data temporarily unavailable; retry"
    );
}

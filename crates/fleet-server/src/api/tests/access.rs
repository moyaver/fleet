// SPDX-License-Identifier: AGPL-3.0-only
//! Who may read screens A–D: every route goes through the caller's role on the project, and a
//! project (or an object of a project) the caller can't read looks missing.

use fleet_core::policy::Role;
use fleet_protocol::event::{FlowOutcome, StepStatus};
use serde_json::{Value, json};

use super::rig::{Agent, Rig, rig, sample, telemetry};

/// Every route of the screens, with `{…}` standing for IDs that exist in project `app`.
struct Routes {
    /// Routes that name only a project.
    project: Vec<String>,
    /// Routes that also name an object of the project.
    objects: Vec<String>,
}

async fn routes(rig: &Rig) -> Routes {
    let mut agent = Agent::new("800", "build", "runner-a");
    agent.started();
    let shard = agent.shard("emulator-5554", &["login"]);
    let login = agent.flow_started(&shard, "login", 0);
    agent.step(&login, 0, "Launch app", StepStatus::Completed);
    agent.flow_ended(&login, FlowOutcome::Passed, None);
    let at = time::OffsetDateTime::now_utc() - time::Duration::minutes(1);
    rig.publish(&mut agent, vec![telemetry(1, vec![sample(at, 10.0)])])
        .await;
    let attempt = rig.attempt_id("app").await;
    let snapshot = rig
        .json(&format!("/api/v1/projects/app/attempts/{attempt}"))
        .await;
    let flow = snapshot["flows"][0]["id"].as_str().unwrap();
    let device = snapshot["devices"][0]["id"].as_str().unwrap();
    let worker = snapshot["agent_runs"][0]["worker_id"].as_str().unwrap();
    Routes {
        project: vec!["overview".into(), "history".into()],
        objects: vec![
            format!("attempts/{attempt}/timeline"),
            format!("attempts/{attempt}/plans"),
            format!("workers/{worker}"),
            format!("workers/{worker}/runs"),
            format!("workers/{worker}/metrics"),
            format!("devices/{device}"),
            format!("devices/{device}/metrics"),
            format!("flows/{flow}"),
            format!("flows/{flow}/artifacts"),
            "flows/login/stats".into(), // a live flow: known by its name,
            format!("compare?a={attempt}&b={attempt}"),
        ],
    }
}

#[tokio::test]
async fn without_a_credential_or_with_a_publisher_key_nothing_is_readable() {
    let rig = rig().await;
    let routes = routes(&rig).await;

    for route in routes.project.iter().chain(&routes.objects) {
        let path = format!("/api/v1/projects/app/{route}");
        let anonymous = rig.app.get(&path).await;
        assert_eq!(anonymous.status_code(), 401, "{path} without a credential");
        let publisher = rig.app.get(&path).authorization_bearer(&rig.key).await;
        assert_eq!(publisher.status_code(), 401, "{path} with a write-only key");
    }
}

#[tokio::test]
async fn a_user_with_a_role_on_another_project_sees_this_one_as_missing() {
    let rig = rig().await;
    let routes = routes(&rig).await;
    let missing = json!({"error": "no such project"});

    for route in routes.project.iter().chain(&routes.objects) {
        let path = format!("/api/v1/projects/app/{route}");
        let foreign = rig.get_as("other", Role::Admin, &path).await;
        assert_eq!(foreign.status_code(), 404, "{path}");
        assert_eq!(foreign.json::<Value>(), missing, "{path}");
    }
    let nowhere = rig.get("/api/v1/projects/nope/overview").await;
    assert_eq!(nowhere.status_code(), 404);
    assert_eq!(
        nowhere.json::<Value>(),
        missing,
        "indistinguishable from a project that exists but isn't theirs"
    );
}

#[tokio::test]
async fn a_viewer_of_the_project_reads_every_screen() {
    let rig = rig().await;
    let routes = routes(&rig).await;

    for route in routes.project.iter().chain(&routes.objects) {
        let path = format!("/api/v1/projects/app/{route}");
        let res = rig.get_as("app", Role::Viewer, &path).await;
        assert_eq!(res.status_code(), 200, "{path}: {}", res.text());
    }
}

#[tokio::test]
async fn an_object_of_one_project_is_missing_under_another_project() {
    let rig = rig().await;
    let routes = routes(&rig).await;

    for route in &routes.objects {
        let path = format!("/api/v1/projects/other/{route}");
        let res = rig.get(&path).await;
        assert_eq!(res.status_code(), 404, "{path}");
        let message = res.json::<Value>()["error"].as_str().unwrap().to_owned();
        assert!(message.starts_with("no such "), "{path}: {message}");
    }
}

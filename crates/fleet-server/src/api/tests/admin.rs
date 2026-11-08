// SPDX-License-Identifier: AGPL-3.0-only
//! Screen G over HTTP (spec §8; PLAN §10.3, M6): every administration area answers a global admin
//! with the data that was seeded, and refuses everyone else. The seeded rows come from the places
//! they really live: real ingest for runs, the store's own writes for keys, bindings and audit,
//! and the configuration for the policies the configuration sets.

use fleet_core::policy::{Grant, Role};
use fleet_protocol::event::{FlowOutcome, StepStatus};
use fleet_store::{auth::AuditActor, testing};
use serde_json::{Value, json};
use time::{Duration, OffsetDateTime};

use super::rig::{Agent, Rig, rig, rig_with, sample, telemetry};
use crate::{config::TrustPolicyConfig, session};

/// A signed-in user whose only grant is `role` on every project.
async fn signed_in_everywhere(rig: &Rig, role: Role, name: &str) -> String {
    let user = rig
        .store
        .upsert_user(
            "https://idp.test",
            name,
            Some(&format!("{name}@example.test")),
            Some(name),
        )
        .await
        .unwrap();
    let data = json!({
        "grants": [Grant { project: None, role }],
        "csrf": "csrf", "email": null, "name": null,
    });
    let cookie = rig
        .store
        .create_session(
            Some(user),
            &data,
            OffsetDateTime::now_utc() + Duration::hours(1),
        )
        .await
        .unwrap();
    format!("{}={}", session::COOKIE, cookie.expose())
}

/// Seeds one run of project `app`, left open, so the ingestion page has an open run to show.
async fn open_run(rig: &Rig) -> uuid::Uuid {
    let mut agent = Agent::new("900", "build", "runner-a");
    agent.started();
    let shard = agent.shard("emulator-5554", &["login"]);
    let login = agent.flow_started(&shard, "login", 0);
    agent.step(&login, 0, "Launch app", StepStatus::Completed);
    agent.flow_ended(&login, FlowOutcome::Passed, None);
    let at = OffsetDateTime::now_utc() - Duration::minutes(1);
    rig.publish(&mut agent, vec![telemetry(1, vec![sample(at, 10.0)])])
        .await;
    agent.id
}

/// A read as the signed-in person `cookie` names: the status and the JSON body.
async fn read(rig: &Rig, cookie: &str, path: &str) -> (u16, Value) {
    let res = rig
        .app
        .get(path)
        .add_header("cookie", cookie.to_owned())
        .await;
    (
        res.status_code().as_u16(),
        serde_json::from_str(&res.text()).unwrap_or(Value::Null),
    )
}

/// The rows of a page as `(key, value, source)` triples, for the configuration-backed pages.
fn settings(page: &Value) -> Vec<(String, String, String)> {
    page.as_array()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s["key"].as_str().unwrap().to_owned(),
                s["value"].as_str().unwrap().to_owned(),
                s["source"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one straight-line scenario: seed every area, then read each one"
)]
async fn a_global_admin_reads_every_area_with_the_seeded_data() {
    let rig = rig_with(|c| {
        c.projects[0].trust_policies = vec![TrustPolicyConfig {
            name: "release".into(),
            issuer: "https://token.actions.githubusercontent.com".into(),
            audience: "fleet".into(),
            repository_id: "1001".into(),
            repository_owner_id: "2002".into(),
            trusted_refs: vec!["refs/heads/main".into()],
            allowed_events: Vec::new(),
            pr_policy: "restricted".into(),
            job_workflow_ref: None,
            environment: Some("prod".into()),
        }];
    })
    .await;
    let cookie = signed_in_everywhere(&rig, Role::Admin, "ada").await;
    let run = open_run(&rig).await;
    let grace = rig
        .store
        .upsert_user("https://idp.test", "grace", None, Some("Grace"))
        .await
        .unwrap();
    testing::seed_role_binding(&rig.store, None, grace, "viewer")
        .await
        .unwrap();
    rig.store
        .sync_group_mappings(
            "https://idp.test",
            &[fleet_store::sessions::GroupMapping {
                project: None,
                group: "fleet-admins".into(),
                role: Role::Admin,
            }],
        )
        .await
        .unwrap();
    testing::seed_ingest_rejection(&rig.store, run, 7, "invalid", "nul in flow name")
        .await
        .unwrap();
    testing::seed_ingest_gap(&rig.store, run, "telemetry", 3, 5, "spool full")
        .await
        .unwrap();
    let ada = rig
        .store
        .upsert_user("https://idp.test", "ada", None, Some("Ada"))
        .await
        .unwrap();
    rig.store
        .audit(
            AuditActor::User(ada),
            rig.store.project_id("app").await.unwrap(),
            "failure.triage",
            json!({"fingerprint": "f1", "from": "unknown", "to": "known", "issue_url": "https://example.test/1"}),
        )
        .await
        .unwrap();

    let get = |path: &'static str| {
        let (rig, cookie) = (&rig, cookie.clone());
        async move {
            let (status, body) = read(rig, &cookie, path).await;
            assert_eq!(status, 200, "GET {path}: {body}");
            body
        }
    };

    // Projects: both, with the working key and policy counted on `app`.
    let projects = get("/api/v1/admin/projects").await;
    let app = projects
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["slug"] == "app")
        .unwrap();
    assert_eq!(
        (
            app["active_api_keys"].as_i64(),
            app["active_trust_policies"].as_i64()
        ),
        (Some(1), Some(1))
    );
    assert!(
        projects
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["slug"] == "other")
    );

    // Trust policies: the configured one, with its matches.
    let policies = get("/api/v1/admin/trust-policies").await;
    assert_eq!(policies[0]["project"], "app");
    assert_eq!(policies[0]["name"], "release");
    assert_eq!(policies[0]["repository_id"], "1001");
    assert_eq!(policies[0]["environment"], "prod");

    // API keys: the seeded key by name and prefix, and no secret or hash in any field.
    let keys = get("/api/v1/admin/api-keys").await;
    assert_eq!(keys[0]["name"], "ci");
    assert!(keys[0]["prefix"].as_str().unwrap().starts_with("mf_live_"));
    let text = keys.to_string();
    assert!(
        !text.contains("token_hash") && !text.contains("\"secret\""),
        "{text}"
    );

    // Role mappings: Grace's binding and the group that grants admin.
    let roles = get("/api/v1/admin/role-mappings").await;
    assert_eq!(roles["bindings"][0]["user"], "Grace");
    assert_eq!(roles["bindings"][0]["role"], "viewer");
    assert!(
        roles["bindings"][0]["project"].is_null(),
        "a binding for every project"
    );
    assert_eq!(roles["groups"][0]["group"], "fleet-admins");
    assert_eq!(roles["groups"][0]["role"], "admin");

    // Retention: six classes per project, and the configured windows.
    let retention = get("/api/v1/admin/retention").await;
    assert_eq!(
        retention["classes"].as_array().unwrap().len(),
        12,
        "two projects, six classes"
    );
    let windows = settings(&retention["windows"]);
    assert!(windows.contains(&(
        "retention.audit_days".into(),
        "365 days".into(),
        "config".into()
    )));

    // Artifact policy: from the configuration, nothing secret.
    let artifacts = get("/api/v1/admin/artifact-policy").await;
    let policy = settings(&artifacts);
    assert!(policy.contains(&(
        "artifacts.max_run_files".into(),
        "2000".into(),
        "config".into()
    )));
    assert!(policy.contains(&("storage.backend".into(), "local_fs".into(), "config".into())));
    assert!(
        !artifacts.to_string().contains("secret") && !artifacts.to_string().contains("access_key")
    );

    // Metric collection policy: the configured checkpoint, and the agents' own sampling.
    let metrics = get("/api/v1/admin/metric-policy").await;
    let metric = settings(&metrics);
    assert!(metric.contains(&(
        "ingest.telemetry_flush_ms".into(),
        "50".into(),
        "config".into()
    )));
    assert!(
        metric
            .iter()
            .any(|(k, _, s)| k == "agent.sample_interval" && s == "agent")
    );

    // Agents: this server's version and protocol, and the run's agent version as compatible.
    let agents = get("/api/v1/admin/agents").await;
    assert_eq!(agents["protocol_version"], 1);
    assert_eq!(agents["server_version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(agents["agents"][0]["agent_version"], "0.0.0");
    assert_eq!(agents["agents"][0]["compatible"], true);
    assert_eq!(agents["agents"][0]["open_runs"], 1);

    // Ingestion: the refusal, the loss and the open run with its lag.
    let ingestion = get("/api/v1/admin/ingestion").await;
    assert_eq!(ingestion["rejections"][0]["code"], "invalid");
    assert_eq!(ingestion["rejections"][0]["message"], "nul in flow name");
    assert_eq!(
        (
            ingestion["gaps"][0]["from_seq"].as_i64(),
            ingestion["gaps"][0]["to_seq"].as_i64()
        ),
        (Some(3), Some(5))
    );
    assert_eq!(ingestion["gaps"][0]["reason"], "spool full");
    assert_eq!(ingestion["open_runs"][0]["agent_run_id"], run.to_string());
    assert!(
        ingestion["open_runs"][0]["seconds_since_frame"]
            .as_i64()
            .unwrap()
            >= 0
    );

    // Audit: the triage change with its before and after, newest first.
    let audit = get("/api/v1/admin/audit").await;
    let event = &audit["items"][0];
    assert_eq!(event["action"], "failure.triage");
    assert_eq!(event["actor"], "Ada");
    assert_eq!(event["project"], "app");
    assert_eq!(event["before"], "unknown");
    assert_eq!(event["after"], "known");
    assert_eq!(event["details"]["issue_url"], "https://example.test/1");
}

#[tokio::test]
async fn the_audit_history_pages_newest_first_and_refuses_a_foreign_cursor() {
    let rig = rig().await;
    let cookie = signed_in_everywhere(&rig, Role::Admin, "ada").await;
    let ada = rig
        .store
        .upsert_user("https://idp.test", "ada", None, Some("Ada"))
        .await
        .unwrap();
    for n in 0..3 {
        rig.store
            .audit(
                AuditActor::User(ada),
                None,
                "failure.triage",
                json!({"fingerprint": format!("f{n}")}),
            )
            .await
            .unwrap();
    }
    let (status, first) = read(&rig, &cookie, "/api/v1/admin/audit?limit=2").await;
    assert_eq!(status, 200);
    assert_eq!(first["items"].as_array().unwrap().len(), 2);
    let cursor = first["next_cursor"].as_str().unwrap().to_owned();
    let (_, second) = read(
        &rig,
        &cookie,
        &format!("/api/v1/admin/audit?limit=2&cursor={cursor}"),
    )
    .await;
    assert_eq!(second["items"].as_array().unwrap().len(), 1);
    assert!(
        second["next_cursor"].is_null(),
        "the last page has no cursor"
    );
    let (status, body) = read(&rig, &cookie, "/api/v1/admin/audit?cursor=nonsense").await;
    assert_eq!(status, 400, "{body}");
}

#[tokio::test]
async fn a_project_admin_or_a_viewer_reads_nothing_of_screen_g() {
    let rig = rig().await;
    for path in [
        "/api/v1/admin/projects",
        "/api/v1/admin/trust-policies",
        "/api/v1/admin/api-keys",
        "/api/v1/admin/role-mappings",
        "/api/v1/admin/retention",
        "/api/v1/admin/artifact-policy",
        "/api/v1/admin/metric-policy",
        "/api/v1/admin/agents",
        "/api/v1/admin/ingestion",
        "/api/v1/admin/audit",
    ] {
        let res = rig.get_as("app", Role::Admin, path).await;
        assert_eq!(
            res.status_code(),
            403,
            "{path}: a project admin is not a global admin"
        );
        assert_eq!(
            res.json::<Value>()["error"],
            "your role does not allow this"
        );
        let res = rig.get_as("app", Role::Viewer, path).await;
        assert_eq!(res.status_code(), 403, "{path}: a viewer");
        let res = rig.get(path).await;
        assert_eq!(
            res.status_code(),
            403,
            "{path}: the viewer token is a viewer"
        );
    }
}

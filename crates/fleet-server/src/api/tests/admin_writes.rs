// SPDX-License-Identifier: AGPL-3.0-only
//! Screen G's writes over HTTP (issue #139): every change a global administrator makes, as an API
//! client and as a browser form; every refusal, which changes nothing; the CSRF checks; the last
//! global administrator; and the configuration's locked rows.

use fleet_core::policy::{Grant, Role};
use fleet_protocol::frame::{BatchRequest, BatchResponse, Limits, ServerFrame};
use fleet_store::{auth::AuditActor, testing};
use serde_json::{Value, json};
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use super::rig::{Agent, Rig, rig, rig_with};
use crate::{config::TrustPolicyConfig, session};

const CSRF: &str = "csrf-for-writes";

/// A signed-in session whose only grants are `grants`, with the CSRF token `CSRF`. Returns the
/// cookie header value.
async fn signed_in(rig: &Rig, name: &str, grants: Vec<Grant>) -> String {
    let user = rig
        .store
        .upsert_user("https://idp.test", name, None, Some(name))
        .await
        .unwrap();
    let data = json!({"grants": grants, "csrf": CSRF, "email": null, "name": name});
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

async fn global_admin(rig: &Rig) -> String {
    signed_in(
        rig,
        "ada",
        vec![Grant {
            project: None,
            role: Role::Admin,
        }],
    )
    .await
}

/// The number of audit events, and the names of the projects, as a check that nothing changed.
async fn snapshot(rig: &Rig) -> (i64, Vec<String>) {
    let audit = testing::audit_count(&rig.store).await.unwrap();
    let projects = rig
        .store
        .admin_projects()
        .await
        .unwrap()
        .into_iter()
        .map(|p| format!("{}={}", p.slug, p.name))
        .collect();
    (audit, projects)
}

/// A read as the signed-in person `cookie` names.
async fn read(rig: &Rig, cookie: &str, path: &str) -> axum_test::TestResponse {
    rig.app
        .get(path)
        .add_header("cookie", cookie.to_owned())
        .await
}

/// A JSON write as a session holder, with the CSRF header.
fn json_write(
    rig: &Rig,
    method: &str,
    path: &str,
    cookie: &str,
    body: Option<&Value>,
) -> axum_test::TestRequest {
    let req = match method {
        "POST" => rig.app.post(path),
        "PUT" => rig.app.put(path),
        "DELETE" => rig.app.delete(path),
        other => panic!("{other}"),
    };
    let req = req
        .add_header("cookie", cookie.to_owned())
        .add_header("x-csrf-token", CSRF);
    match body {
        Some(b) => req.json(b),
        None => req,
    }
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one straight-line scenario: each change in turn, then the audit history that shows them"
)]
async fn a_global_admin_makes_each_change_over_json_and_each_writes_one_audit_event() {
    let rig = rig().await;
    let cookie = global_admin(&rig).await;

    // Project: create, then rename. The slug stays.
    let before = snapshot(&rig).await.0;
    let res = json_write(
        &rig,
        "POST",
        "/api/v1/admin/projects",
        &cookie,
        Some(&json!({"slug": "mobile", "name": "Mobile"})),
    )
    .await;
    assert_eq!(res.status_code(), 201, "{}", res.text());
    let project_id: Uuid = res.json::<Value>()["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(
        snapshot(&rig).await.0,
        before + 1,
        "one audit event per change"
    );
    let res = json_write(
        &rig,
        "PUT",
        "/api/v1/admin/projects/mobile",
        &cookie,
        Some(&json!({"name": "Mobile QA"})),
    )
    .await;
    assert_eq!(res.status_code(), 200, "{}", res.text());
    assert_eq!(res.json::<Value>()["id"], project_id.to_string());
    let (audit, projects) = snapshot(&rig).await;
    assert_eq!(audit, before + 2);
    assert!(
        projects.contains(&"mobile=Mobile QA".to_owned()),
        "{projects:?}"
    );

    // API key: the secret is in the answer, and works; the key list shows it without it.
    let res = json_write(
        &rig,
        "POST",
        "/api/v1/admin/api-keys",
        &cookie,
        Some(&json!({"project": "mobile", "name": "ci", "days": 30})),
    )
    .await;
    assert_eq!(res.status_code(), 201, "{}", res.text());
    let created: Value = res.json();
    let secret = created["secret"].as_str().unwrap().to_owned();
    assert!(secret.starts_with("mf_live_"));
    let key_id = created["key"]["id"].as_str().unwrap().to_owned();
    assert!(rig.store.authenticate(&secret).await.unwrap().is_some());
    let listed = read(&rig, &cookie, "/api/v1/admin/api-keys").await.text();
    assert!(!listed.contains(&secret), "the secret is not listed");

    // Expire in 1 day, then revoke: the key stops authenticating (and so does its tokens).
    let res = json_write(
        &rig,
        "POST",
        &format!("/api/v1/admin/api-keys/{key_id}/expire"),
        &cookie,
        Some(&json!({"days": 1})),
    )
    .await;
    assert_eq!(res.status_code(), 200, "{}", res.text());
    let res = json_write(
        &rig,
        "DELETE",
        &format!("/api/v1/admin/api-keys/{key_id}"),
        &cookie,
        None,
    )
    .await;
    assert_eq!(res.status_code(), 200, "{}", res.text());
    assert!(rig.store.authenticate(&secret).await.unwrap().is_none());
    let res = json_write(
        &rig,
        "DELETE",
        &format!("/api/v1/admin/api-keys/{key_id}"),
        &cookie,
        None,
    )
    .await;
    assert_eq!(
        res.status_code(),
        409,
        "a second revoke has nothing to change"
    );

    // Role binding: add for a person, then remove (204).
    let person = rig
        .store
        .upsert_user("https://idp.test", "grace", None, Some("Grace"))
        .await
        .unwrap();
    let res = json_write(
        &rig,
        "POST",
        "/api/v1/admin/role-bindings",
        &cookie,
        Some(&json!({"project": "mobile", "person": person, "role": "maintainer"})),
    )
    .await;
    assert_eq!(res.status_code(), 201, "{}", res.text());
    let binding: Uuid = res.json::<Value>()["id"].as_str().unwrap().parse().unwrap();
    let res = json_write(
        &rig,
        "DELETE",
        &format!("/api/v1/admin/role-bindings/{binding}"),
        &cookie,
        None,
    )
    .await;
    assert_eq!(res.status_code(), 204, "{}", res.text());

    // Trust policy: add, edit (PUT), revoke, remove (DELETE).
    let policy = json!({
        "project": "mobile", "name": "release", "repository_id": "1001", "repository_owner_id": "2002",
        "trusted_refs": "refs/heads/main, refs/tags/*", "environment": "prod", "pr_policy": "deny"
    });
    let res = json_write(
        &rig,
        "POST",
        "/api/v1/admin/trust-policies",
        &cookie,
        Some(&policy),
    )
    .await;
    assert_eq!(res.status_code(), 201, "{}", res.text());
    let pid: Uuid = res.json::<Value>()["id"].as_str().unwrap().parse().unwrap();
    let mut edited = policy.clone();
    edited["environment"] = json!("");
    edited["name"] = json!("release-2");
    let res = json_write(
        &rig,
        "PUT",
        &format!("/api/v1/admin/trust-policies/{pid}"),
        &cookie,
        Some(&edited),
    )
    .await;
    assert_eq!(res.status_code(), 200, "{}", res.text());
    let res = json_write(
        &rig,
        "POST",
        &format!("/api/v1/admin/trust-policies/{pid}/revoke"),
        &cookie,
        None,
    )
    .await;
    assert_eq!(res.status_code(), 200, "{}", res.text());
    let res = json_write(
        &rig,
        "DELETE",
        &format!("/api/v1/admin/trust-policies/{pid}"),
        &cookie,
        None,
    )
    .await;
    assert_eq!(res.status_code(), 204, "{}", res.text());

    // Seven changes after the first snapshot: one audit event each, none for a refusal.
    let (audit, _) = snapshot(&rig).await;
    assert_eq!(
        audit,
        before + 2 + 1 + 1 + 1 + 1 + 1 + 1 + 1 + 1 + 1,
        "one event per successful change"
    );

    // The audit history shows them, with before and after where recorded, and never a secret.
    let history = read(&rig, &cookie, "/api/v1/admin/audit?limit=200").await;
    assert_eq!(history.status_code(), 200);
    let text = history.text();
    assert!(
        !text.contains(&secret),
        "the secret is never in the audit history"
    );
    let actions: Vec<String> = history.json::<Value>()["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["action"].as_str().unwrap().to_owned())
        .collect();
    for action in [
        "project.create",
        "project.rename",
        "api_key.create",
        "api_key.expire",
        "api_key.revoke",
        "role_binding.add",
        "role_binding.remove",
        "trust_policy.add",
        "trust_policy.edit",
        "trust_policy.revoke",
        "trust_policy.remove",
    ] {
        assert!(
            actions.contains(&action.to_owned()),
            "{action} in {actions:?}"
        );
    }
    let rename = read(&rig, &cookie, "/api/v1/admin/audit?limit=200")
        .await
        .json::<Value>()["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["action"] == "project.rename")
        .cloned()
        .unwrap();
    assert_eq!(rename["before"]["name"], "Mobile");
    assert_eq!(rename["after"]["name"], "Mobile QA");
}

#[tokio::test]
async fn a_browser_form_makes_the_change_and_is_sent_back_to_its_section() {
    let rig = rig().await;
    let cookie = global_admin(&rig).await;
    let form = |path: &str, fields: &str| {
        rig.app
            .post(path)
            .add_header("cookie", cookie.clone())
            .text(format!("{fields}&csrf={CSRF}"))
            .content_type("application/x-www-form-urlencoded")
    };

    let res = form("/api/v1/admin/projects", "slug=web&name=Web").await;
    assert_eq!(res.status_code(), 303, "{}", res.text());
    assert_eq!(res.header("location"), "/admin#projects");
    assert!(rig.store.project_id("web").await.unwrap().is_some());

    // A key's secret is on the page the form answers with, once, and that page is never cached.
    let res = form(
        "/api/v1/admin/api-keys",
        "project=web&name=ci&days=90&trust=trusted",
    )
    .await;
    assert_eq!(res.status_code(), 201, "{}", res.text());
    assert_eq!(res.header("cache-control"), "no-store");
    let page = res.text();
    let start = page.find("<pre id=\"secret\">").unwrap() + "<pre id=\"secret\">".len();
    let secret = &page[start..start + page[start..].find('<').unwrap()];
    assert!(
        secret.starts_with("mf_live_") && rig.store.authenticate(secret).await.unwrap().is_some()
    );
    assert_eq!(
        page.matches("mf_live_").count(),
        1,
        "the secret appears once"
    );

    // A role on a project, a trust policy, and a rename: each goes back to its section.
    let person = rig
        .store
        .upsert_user("https://idp.test", "grace", None, Some("Grace"))
        .await
        .unwrap();
    let res = form(
        "/api/v1/admin/role-bindings",
        &format!("project=web&person={person}&role=viewer"),
    )
    .await;
    assert_eq!(res.status_code(), 303, "{}", res.text());
    assert_eq!(res.header("location"), "/admin#roles");
    let res = form("/api/v1/admin/trust-policies", "project=web&name=main&repository_id=1&repository_owner_id=2&trusted_refs=refs%2Fheads%2Fmain&pr_policy=restricted").await;
    assert_eq!(res.status_code(), 303, "{}", res.text());
    assert_eq!(res.header("location"), "/admin#trust");
    let project = rig.store.project_id("web").await.unwrap().unwrap();
    let res = form("/api/v1/admin/projects/web/rename", "name=Web+app").await;
    assert_eq!(res.status_code(), 303, "{}", res.text());
    assert_eq!(
        rig.store
            .admin_projects()
            .await
            .unwrap()
            .iter()
            .find(|p| p.id == project)
            .unwrap()
            .name,
        "Web app"
    );

    // A refusal is a page that says why, with the status, and nothing changes.
    let (audit, _) = snapshot(&rig).await;
    let res = form("/api/v1/admin/projects", "slug=web&name=Again").await;
    assert_eq!(res.status_code(), 409);
    assert_eq!(res.header("content-type"), "text/html; charset=utf-8");
    assert!(res.text().contains("already exists"), "{}", res.text());
    assert_eq!(
        snapshot(&rig).await.0,
        audit,
        "a refused form writes nothing"
    );
}

#[tokio::test]
async fn anyone_who_is_not_a_global_administrator_is_refused_and_changes_nothing() {
    let rig = rig().await;
    let project = rig.store.project_id("app").await.unwrap().unwrap();
    let other = rig.store.project_id("other").await.unwrap().unwrap();
    let (audit, projects) = snapshot(&rig).await;
    let who = [
        (
            "viewer",
            vec![Grant {
                project: Some(project),
                role: Role::Viewer,
            }],
        ),
        (
            "maintainer",
            vec![Grant {
                project: Some(project),
                role: Role::Maintainer,
            }],
        ),
        (
            "project admin",
            vec![Grant {
                project: Some(project),
                role: Role::Admin,
            }],
        ),
        (
            "other project's admin",
            vec![Grant {
                project: Some(other),
                role: Role::Admin,
            }],
        ),
    ];
    for (name, grants) in who {
        let cookie = signed_in(&rig, name, grants).await;
        let res = json_write(
            &rig,
            "POST",
            "/api/v1/admin/projects",
            &cookie,
            Some(&json!({"slug": "sneaky", "name": "Sneaky"})),
        )
        .await;
        assert_eq!(res.status_code(), 403, "{name}: {}", res.text());
        let res = json_write(
            &rig,
            "PUT",
            "/api/v1/admin/projects/app",
            &cookie,
            Some(&json!({"name": "Hacked"})),
        )
        .await;
        assert_eq!(res.status_code(), 403, "{name}");
        let res = json_write(
            &rig,
            "POST",
            "/api/v1/admin/api-keys",
            &cookie,
            Some(&json!({"project": "app", "name": "x", "days": 1})),
        )
        .await;
        assert_eq!(res.status_code(), 403, "{name}");
        let res = json_write(
            &rig,
            "DELETE",
            &format!("/api/v1/admin/trust-policies/{}", Uuid::now_v7()),
            &cookie,
            None,
        )
        .await;
        assert_eq!(res.status_code(), 403, "{name}");
    }
    assert_eq!(snapshot(&rig).await, (audit, projects), "nothing changed");
}

#[tokio::test]
async fn a_write_needs_a_session_and_its_csrf_token_from_the_dashboards_own_origin() {
    let rig = rig().await;
    let cookie = global_admin(&rig).await;
    let (audit, projects) = snapshot(&rig).await;
    let body = json!({"slug": "forged", "name": "Forged"});

    // No session at all.
    let res = rig.app.post("/api/v1/admin/projects").json(&body).await;
    assert_eq!(res.status_code(), 401);
    // A session, but no token: a forged cross-site form carries the cookie and no token.
    let res = rig
        .app
        .post("/api/v1/admin/projects")
        .add_header("cookie", cookie.clone())
        .json(&body)
        .await;
    assert_eq!(res.status_code(), 403, "{}", res.text());
    // A wrong token.
    let res = rig
        .app
        .post("/api/v1/admin/projects")
        .add_header("cookie", cookie.clone())
        .add_header("x-csrf-token", "guessed")
        .json(&body)
        .await;
    assert_eq!(res.status_code(), 403);
    // The right token, from another site.
    let res = rig
        .app
        .post("/api/v1/admin/projects")
        .add_header("cookie", cookie.clone())
        .add_header("x-csrf-token", CSRF)
        .add_header("origin", "https://evil.example")
        .json(&body)
        .await;
    assert_eq!(
        res.status_code(),
        403,
        "a foreign origin is refused: {}",
        res.text()
    );
    // A form from another site, with the token it guessed (it can't know the real one).
    let res = rig
        .app
        .post("/api/v1/admin/projects")
        .add_header("cookie", cookie.clone())
        .add_header("origin", "https://evil.example")
        .text("slug=forged&name=Forged&csrf=guessed")
        .content_type("application/x-www-form-urlencoded")
        .await;
    assert_eq!(res.status_code(), 403);
    // A form with no token at all.
    let res = rig
        .app
        .post("/api/v1/admin/projects")
        .add_header("cookie", cookie.clone())
        .text("slug=forged&name=Forged")
        .content_type("application/x-www-form-urlencoded")
        .await;
    assert_eq!(res.status_code(), 403);
    assert_eq!(
        snapshot(&rig).await,
        (audit, projects),
        "nothing was written"
    );

    // A dashboard form under `no-referrer` sends `Origin: null`; its Fetch Metadata says it is the
    // dashboard's own, so it is accepted with the right token...
    let res = rig
        .app
        .post("/api/v1/admin/projects")
        .add_header("cookie", cookie.clone())
        .add_header("origin", "null")
        .add_header("sec-fetch-site", "same-origin")
        .text(format!("slug=dashboard&name=Dashboard&csrf={CSRF}"))
        .content_type("application/x-www-form-urlencoded")
        .await;
    assert_eq!(res.status_code(), 303, "{}", res.text());
    // ...and refused from another site, even with the right token.
    let res = rig
        .app
        .post("/api/v1/admin/projects")
        .add_header("cookie", cookie.clone())
        .add_header("origin", "null")
        .add_header("sec-fetch-site", "cross-site")
        .text(format!("slug=cross&name=Cross&csrf={CSRF}"))
        .content_type("application/x-www-form-urlencoded")
        .await;
    assert_eq!(res.status_code(), 403, "{}", res.text());
    assert!(rig.store.project_id("cross").await.unwrap().is_none());
}

#[tokio::test]
async fn an_invalid_change_is_refused_with_what_is_wrong_and_writes_nothing() {
    let rig = rig().await;
    let cookie = global_admin(&rig).await;
    let (audit, projects) = snapshot(&rig).await;
    for (path, body, status) in [
        (
            "/api/v1/admin/projects",
            json!({"slug": "Bad Slug", "name": "x"}),
            422,
        ),
        (
            "/api/v1/admin/projects",
            json!({"slug": "app", "name": "Again"}),
            409,
        ),
        (
            "/api/v1/admin/projects",
            json!({"slug": "fine", "name": "   "}),
            422,
        ),
        (
            "/api/v1/admin/projects",
            json!({"slug": "fine", "name": "a\u{1b}[31m"}),
            422,
        ),
        (
            "/api/v1/admin/api-keys",
            json!({"project": "app", "name": "k", "days": 0}),
            422,
        ),
        (
            "/api/v1/admin/api-keys",
            json!({"project": "app", "name": "k", "days": 9999}),
            422,
        ),
        (
            "/api/v1/admin/api-keys",
            json!({"project": "app", "name": "k", "trust": "root"}),
            422,
        ),
        (
            "/api/v1/admin/api-keys",
            json!({"project": "nope", "name": "k"}),
            404,
        ),
        (
            "/api/v1/admin/trust-policies",
            json!({"project": "app", "name": "p", "repository_id": "1", "repository_owner_id": "2", "pr_policy": "maybe"}),
            422,
        ),
        (
            "/api/v1/admin/trust-policies",
            json!({"project": "app", "name": "p", "repository_id": "1", "repository_owner_id": "2", "issuer": "https://elsewhere.example"}),
            422,
        ),
        (
            "/api/v1/admin/trust-policies",
            json!({"project": "app", "name": "p", "repository_id": "", "repository_owner_id": "2"}),
            422,
        ),
        (
            "/api/v1/admin/role-bindings",
            json!({"project": "app", "person": Uuid::now_v7(), "role": "root"}),
            422,
        ),
        (
            "/api/v1/admin/group-mappings",
            json!({"project": "app", "group": "g", "role": "viewer"}),
            422,
        ),
    ] {
        let res = json_write(&rig, "POST", path, &cookie, Some(&body)).await;
        assert_eq!(res.status_code(), status, "{path} {body}: {}", res.text());
    }
    assert_eq!(snapshot(&rig).await, (audit, projects), "nothing changed");
}

#[tokio::test]
async fn the_last_global_administrator_cannot_be_removed_over_http() {
    let rig = rig().await;
    let cookie = global_admin(&rig).await;
    // The administrator who signs in is also the only one with a global binding.
    let ada = rig
        .store
        .upsert_user("https://idp.test", "ada-bound", None, Some("Ada"))
        .await
        .unwrap();
    let binding = rig
        .store
        .add_role_binding(AuditActor::User(ada), None, ada, Role::Admin)
        .await
        .unwrap();
    let (audit, _) = snapshot(&rig).await;
    let res = json_write(
        &rig,
        "DELETE",
        &format!("/api/v1/admin/role-bindings/{binding}"),
        &cookie,
        None,
    )
    .await;
    assert_eq!(res.status_code(), 409, "{}", res.text());
    assert!(
        res.text().contains("no global administrator"),
        "{}",
        res.text()
    );
    let res = rig
        .app
        .post(&format!("/api/v1/admin/role-bindings/{binding}/remove"))
        .add_header("cookie", cookie.clone())
        .add_header("x-csrf-token", CSRF)
        .await;
    assert_eq!(
        res.status_code(),
        409,
        "the form's route refuses the same way"
    );
    assert_eq!(
        snapshot(&rig).await.0,
        audit,
        "nothing was removed or written"
    );
    assert_eq!(
        testing::binding_of(&rig.store, ada, None).await.unwrap(),
        Some(binding),
        "the binding stands"
    );
}

#[tokio::test]
async fn the_configuration_locks_its_trust_policies_on_the_dashboard() {
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
            environment: None,
        }];
    })
    .await;
    let cookie = global_admin(&rig).await;
    let config_id: Uuid = read(&rig, &cookie, "/api/v1/admin/trust-policies")
        .await
        .json::<Value>()[0]["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let (audit, _) = snapshot(&rig).await;
    let body = json!({"project": "app", "name": "release", "repository_id": "9", "repository_owner_id": "9"});
    let res = json_write(
        &rig,
        "POST",
        "/api/v1/admin/trust-policies",
        &cookie,
        Some(&body),
    )
    .await;
    assert_eq!(res.status_code(), 409, "{}", res.text());
    assert!(res.text().contains("configuration"), "{}", res.text());
    let res = json_write(
        &rig,
        "PUT",
        &format!("/api/v1/admin/trust-policies/{config_id}"),
        &cookie,
        Some(&body),
    )
    .await;
    assert_eq!(res.status_code(), 409, "{}", res.text());
    let res = json_write(
        &rig,
        "POST",
        &format!("/api/v1/admin/trust-policies/{config_id}/revoke"),
        &cookie,
        None,
    )
    .await;
    assert_eq!(res.status_code(), 409, "{}", res.text());
    let res = json_write(
        &rig,
        "DELETE",
        &format!("/api/v1/admin/trust-policies/{config_id}"),
        &cookie,
        None,
    )
    .await;
    assert_eq!(res.status_code(), 409, "{}", res.text());
    assert!(res.text().contains("configuration"), "{}", res.text());
    assert_eq!(
        snapshot(&rig).await.0,
        audit,
        "a refused change writes nothing"
    );
    // The policy is still there, live.
    let listed = read(&rig, &cookie, "/api/v1/admin/trust-policies")
        .await
        .json::<Value>();
    assert_eq!(listed[0]["source"], "config");
    assert!(listed[0]["revoked_at"].is_null());
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one straight-line scenario: a dashboard mapping, the configured one, and the plain server"
)]
async fn group_mappings_are_added_edited_and_removed_and_the_configured_ones_are_locked() {
    use crate::config::{GroupRole, Login};
    let rig = rig_with(|c| {
        c.api.public_url = Some("https://fleet.test".into());
        c.login = Some(Login {
            issuer: "https://idp.test".into(),
            client_id: "fleet-web".into(),
            client_secret: None,
            scopes: Vec::new(),
            group_claims: vec!["groups".into()],
            session_ttl_minutes: 60,
            session_idle_minutes: 60,
            group_roles: vec![GroupRole {
                group: "qa".into(),
                role: Role::Viewer,
                project: Some("app".into()),
            }],
            jwks_hosts: Vec::new(),
        });
    })
    .await;
    let cookie = global_admin(&rig).await;
    let (audit, _) = snapshot(&rig).await;

    // Add a dashboard mapping; the same group again is refused.
    let res = json_write(
        &rig,
        "POST",
        "/api/v1/admin/group-mappings",
        &cookie,
        Some(&json!({"project": "app", "group": "mobile-leads", "role": "viewer"})),
    )
    .await;
    assert_eq!(res.status_code(), 201, "{}", res.text());
    let mapping: Uuid = res.json::<Value>()["id"].as_str().unwrap().parse().unwrap();
    let res = json_write(
        &rig,
        "POST",
        "/api/v1/admin/group-mappings",
        &cookie,
        Some(&json!({"project": "app", "group": "mobile-leads", "role": "admin"})),
    )
    .await;
    assert_eq!(res.status_code(), 409, "{}", res.text());

    // A group the configuration maps is refused, and its row is locked.
    let res = json_write(
        &rig,
        "POST",
        "/api/v1/admin/group-mappings",
        &cookie,
        Some(&json!({"project": "app", "group": "qa", "role": "admin"})),
    )
    .await;
    assert_eq!(res.status_code(), 409, "{}", res.text());
    let listed = read(&rig, &cookie, "/api/v1/admin/role-mappings")
        .await
        .json::<Value>();
    let configured = listed["groups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["group"] == "qa")
        .unwrap();
    assert_eq!(configured["source"], "config");
    let config_id = configured["id"].as_str().unwrap().to_owned();
    let res = json_write(
        &rig,
        "PUT",
        &format!("/api/v1/admin/group-mappings/{config_id}"),
        &cookie,
        Some(&json!({"role": "admin"})),
    )
    .await;
    assert_eq!(res.status_code(), 409, "{}", res.text());
    let res = json_write(
        &rig,
        "DELETE",
        &format!("/api/v1/admin/group-mappings/{config_id}"),
        &cookie,
        None,
    )
    .await;
    assert_eq!(res.status_code(), 409, "{}", res.text());

    // The dashboard's own mapping is edited and removed.
    let res = json_write(
        &rig,
        "PUT",
        &format!("/api/v1/admin/group-mappings/{mapping}"),
        &cookie,
        Some(&json!({"role": "maintainer"})),
    )
    .await;
    assert_eq!(res.status_code(), 200, "{}", res.text());
    let res = rig
        .app
        .post(&format!("/api/v1/admin/group-mappings/{mapping}/edit"))
        .add_header("cookie", cookie.clone())
        .text(format!("role=maintainer&csrf={CSRF}"))
        .content_type("application/x-www-form-urlencoded")
        .await;
    assert_eq!(
        res.status_code(),
        409,
        "already a maintainer: nothing to change"
    );
    let res = json_write(
        &rig,
        "DELETE",
        &format!("/api/v1/admin/group-mappings/{mapping}"),
        &cookie,
        None,
    )
    .await;
    assert_eq!(res.status_code(), 204, "{}", res.text());
    assert_eq!(
        snapshot(&rig).await.0,
        audit + 3,
        "the add, the edit and the removal, one event each"
    );

    // Without sign-in configured there is no issuer to map a group for.
    let plain = super::rig::rig().await;
    let cookie = global_admin(&plain).await;
    let res = json_write(
        &plain,
        "POST",
        "/api/v1/admin/group-mappings",
        &cookie,
        Some(&json!({"group": "g", "role": "viewer"})),
    )
    .await;
    assert_eq!(res.status_code(), 422, "{}", res.text());
}

/// A policy write as a JSON `PUT`, and the same change as the dashboard's form `POST` to its
/// `/edit` path. Every one names project `app`: only the caller decides the answer.
fn policy_writes(n: u32) -> Vec<(&'static str, Value, String)> {
    let (days, max, quota, seconds) = (n, 1_000 * n, 500 * n, 2 * n);
    vec![
        (
            "/api/v1/admin/retention/app/failure_evidence",
            json!({"days": days}),
            format!("days={days}"),
        ),
        (
            "/api/v1/admin/artifact-classes/app/junit",
            json!({"accepted": true, "max_bytes": max}),
            format!("accepted=on&max_bytes={max}"),
        ),
        (
            "/api/v1/admin/artifact-quota/app",
            json!({"bytes_per_day": quota, "files_per_day": 5}),
            format!("bytes_per_day={quota}&files_per_day=5"),
        ),
        (
            "/api/v1/admin/metric-policy/app",
            json!({"sample_interval_seconds": seconds, "pressure_sample_interval_seconds": 30}),
            format!("sample_interval_seconds={seconds}&pressure_sample_interval_seconds=30"),
        ),
    ]
}

/// Every retention window as the store holds it: `(project, class, seconds)`.
async fn retention_of(rig: &Rig) -> Vec<(String, String, Option<i64>)> {
    rig.store
        .admin_retention_classes()
        .await
        .unwrap()
        .into_iter()
        .map(|c| (c.project, c.class, c.ttl_seconds))
        .collect()
}

/// A dashboard form posted with `cookie`, as a browser sends it.
fn form_write(rig: &Rig, path: &str, cookie: &str, body: &str) -> axum_test::TestRequest {
    rig.app
        .post(path)
        .add_header("cookie", cookie.to_owned())
        .text(body.to_owned())
        .content_type("application/x-www-form-urlencoded")
}

#[tokio::test]
async fn the_policy_writes_are_refused_for_anyone_but_a_global_administrator() {
    let rig = rig().await;
    let project = rig.store.project_id("app").await.unwrap().unwrap();
    let other = rig.store.project_id("other").await.unwrap().unwrap();
    let (audit, projects) = snapshot(&rig).await;
    let overrides = rig.store.admin_project_overrides().await.unwrap();
    let retention = retention_of(&rig).await;
    let who = [
        (
            "viewer",
            vec![Grant {
                project: Some(project),
                role: Role::Viewer,
            }],
        ),
        (
            "maintainer",
            vec![Grant {
                project: Some(project),
                role: Role::Maintainer,
            }],
        ),
        (
            "project admin",
            vec![Grant {
                project: Some(project),
                role: Role::Admin,
            }],
        ),
        (
            "other project's admin",
            vec![Grant {
                project: Some(other),
                role: Role::Admin,
            }],
        ),
    ];
    for (name, grants) in who {
        let cookie = signed_in(&rig, name, grants).await;
        for (path, body, form) in policy_writes(1) {
            // A JSON change, and the form with its right CSRF token: both refused by the role.
            let res = json_write(&rig, "PUT", path, &cookie, Some(&body)).await;
            assert_eq!(res.status_code(), 403, "{name} PUT {path}: {}", res.text());
            let res = form_write(
                &rig,
                &format!("{path}/edit"),
                &cookie,
                &format!("{form}&csrf={CSRF}"),
            )
            .await;
            assert_eq!(
                res.status_code(),
                403,
                "{name} POST {path}/edit: {}",
                res.text()
            );
        }
    }
    assert_eq!(snapshot(&rig).await, (audit, projects), "nothing changed");
    assert_eq!(
        rig.store.admin_project_overrides().await.unwrap(),
        overrides
    );
    assert_eq!(retention_of(&rig).await, retention);
}

#[tokio::test]
async fn a_policy_form_needs_the_csrf_token_of_its_own_session() {
    let rig = rig().await;
    let cookie = global_admin(&rig).await;
    let (audit, projects) = snapshot(&rig).await;
    for (path, _, form) in policy_writes(1) {
        let edit = format!("{path}/edit");
        // A forged cross-site form carries the cookie and no token, or a guessed one.
        let res = form_write(&rig, &edit, &cookie, &form).await;
        assert_eq!(
            res.status_code(),
            403,
            "{path} without a token: {}",
            res.text()
        );
        let res = form_write(&rig, &edit, &cookie, &format!("{form}&csrf=guessed")).await;
        assert_eq!(
            res.status_code(),
            403,
            "{path} with a wrong token: {}",
            res.text()
        );
    }
    assert_eq!(
        snapshot(&rig).await,
        (audit, projects),
        "nothing was written"
    );
}

#[tokio::test]
async fn a_global_admin_sets_each_policy_over_json_and_as_a_form_and_each_change_is_audited() {
    let rig = rig().await;
    let cookie = global_admin(&rig).await;
    let (audit, _) = snapshot(&rig).await;

    // JSON: 200 with the project's ID, one audit event each.
    for (path, body, _) in policy_writes(1) {
        let res = json_write(&rig, "PUT", path, &cookie, Some(&body)).await;
        assert_eq!(res.status_code(), 200, "{path}: {}", res.text());
        assert!(res.json::<Value>()["id"].is_string(), "{path}");
    }
    assert_eq!(snapshot(&rig).await.0, audit + 4, "one event per change");
    let retention = read(&rig, &cookie, "/api/v1/admin/retention")
        .await
        .json::<Value>();
    let failure = retention["classes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["project"] == "app" && c["class"] == "failure_evidence")
        .unwrap();
    assert_eq!(failure["ttl_seconds"], 86_400, "the new window is in force");

    // A form: sent back to its section of /admin, one event each. Other values, so it is a change.
    for (path, _, form) in policy_writes(2) {
        let res = form_write(
            &rig,
            &format!("{path}/edit"),
            &cookie,
            &format!("{form}&csrf={CSRF}"),
        )
        .await;
        assert_eq!(res.status_code(), 303, "{path}: {}", res.text());
        assert!(
            res.header("location")
                .to_str()
                .unwrap()
                .starts_with("/admin#")
        );
    }
    assert_eq!(snapshot(&rig).await.0, audit + 8);

    // A change the server or the store refuses writes nothing.
    let before = snapshot(&rig).await.0;
    let refused: [(&str, Value); 4] = [
        (
            "/api/v1/admin/retention/app/report",
            json!({"days": 36_501}),
        ),
        (
            "/api/v1/admin/artifact-classes/app/junit",
            json!({"accepted": true, "max_bytes": u64::MAX}),
        ),
        (
            "/api/v1/admin/artifact-quota/app",
            json!({"bytes_per_day": u64::MAX}),
        ),
        (
            "/api/v1/admin/metric-policy/app",
            json!({"sample_interval_seconds": 301}),
        ),
    ];
    for (path, body) in refused {
        let res = json_write(&rig, "PUT", path, &cookie, Some(&body)).await;
        assert_eq!(res.status_code(), 422, "{path} {body}: {}", res.text());
    }
    assert_eq!(
        snapshot(&rig).await.0,
        before,
        "a refused change writes nothing"
    );
}

/// The welcome's limits for a run of `agent` in project `app`, as the agent is told them.
async fn welcome_limits(rig: &Rig, agent: &Agent) -> Limits {
    let res = rig
        .app
        .post("/api/v1/agent/batch")
        .authorization_bearer(rig.key.as_str())
        .json(&BatchRequest {
            hello: agent.hello(),
            events: Vec::new(),
            telemetry: Vec::new(),
        })
        .await;
    assert_eq!(res.status_code(), 200, "{}", res.text());
    let body: BatchResponse = res.json();
    match &body.frames[0] {
        ServerFrame::Welcome(w) => w.limits,
        other => panic!("expected a welcome, got {other:?}"),
    }
}

#[tokio::test]
async fn the_welcome_carries_the_projects_sampling_intervals_and_none_when_nothing_sets_them() {
    let rig = rig().await;
    let agent = Agent::new("1200", "build", "runner-a");
    let limits = welcome_limits(&rig, &agent).await;
    assert_eq!(
        limits.sample_interval_ms, None,
        "unset: the agent's own default"
    );
    assert_eq!(limits.pressure_sample_interval_ms, None);

    // The project's own interval, set on the dashboard, is what the agent is told.
    let cookie = global_admin(&rig).await;
    let res = json_write(
        &rig,
        "PUT",
        "/api/v1/admin/metric-policy/app",
        &cookie,
        Some(&json!({"sample_interval_seconds": 2})),
    )
    .await;
    assert_eq!(res.status_code(), 200, "{}", res.text());
    let limits = welcome_limits(&rig, &agent).await;
    assert_eq!(limits.sample_interval_ms, Some(2_000));
    assert_eq!(limits.pressure_sample_interval_ms, None);
}

// SPDX-License-Identifier: AGPL-3.0-only
//! Screen G in a real browser (spec §8 "Screen G — Administration"; PLAN §10.3, M6): an
//! administrator sees every section with seeded data and the audit history; a viewer gets no
//! navigation entry, and a refusal on each administration URL.

use std::{fmt::Write as _, time::Duration};

use fleet_loadgen::Options;
use fleet_server::config::TrustPolicyConfig;
use fleet_store::{
    PoolOptions, Store,
    auth::{AuditActor, TrustLevel},
    testing,
};
use serde_json::json;
use time::{Duration as TimeDuration, OffsetDateTime};
use uuid::Uuid;

use crate::{
    browser::Browser,
    dashboard_ab::{PATIENCE, Page, count_of, q, run_options, sign_in_session},
    support::Stack,
};

/// A connection to the stack's database, as the tests seed rows through the store.
fn store_of(stack: &Stack) -> Store {
    let url = stack.config.database.url.as_ref().unwrap().expose();
    Store::connect_lazy(url, PoolOptions::default()).unwrap()
}

/// Every section of the administration page, by its test ID.
const SECTIONS: [&str; 10] = [
    "admin-projects",
    "admin-trust",
    "admin-keys",
    "admin-roles",
    "admin-retention",
    "admin-artifacts",
    "admin-metrics",
    "admin-agents",
    "admin-ingestion",
    "admin-audit",
];

#[tokio::test(flavor = "multi_thread")]
#[expect(
    clippy::too_many_lines,
    reason = "one straight-line scenario: seed, then a viewer's refusals and an administrator's pages"
)]
async fn an_administrator_sees_every_section_with_seeded_data_and_a_viewer_is_refused() {
    let stack = Stack::start(|c| {
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
    // One finished run, so the agents section has a version to show.
    fleet_loadgen::run(Options {
        pace: Duration::ZERO,
        devices: 1,
        flows: 2,
        ..run_options(&stack, "admin-a")
    })
    .await
    .unwrap();
    let ended = stack.await_run_ended("job-0").await;
    let run: Uuid = ended["agent_runs"][0]["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();

    // Seed through the store: a key, a person with a role, a refusal, a loss and one audited change.
    let store = store_of(&stack);
    let project = store.project_id("app").await.unwrap().unwrap();
    store
        .create_api_key(
            project,
            "deploy-bot",
            TrustLevel::Trusted,
            OffsetDateTime::now_utc() + TimeDuration::days(30),
        )
        .await
        .unwrap();
    let ada = store
        .upsert_user(
            "https://idp.example",
            "ada",
            Some("ada@example.com"),
            Some("Ada"),
        )
        .await
        .unwrap();
    testing::seed_role_binding(&store, Some(project), ada, "viewer")
        .await
        .unwrap();
    testing::seed_ingest_rejection(&store, run, 99, "invalid", "nul byte in flow name")
        .await
        .unwrap();
    testing::seed_ingest_gap(&store, run, "telemetry", 10, 12, "spool full")
        .await
        .unwrap();
    store
        .audit(
            AuditActor::User(ada),
            Some(project),
            "failure.triage",
            json!({
                "fingerprint": "abc123",
                "from": "unknown",
                "to": "known",
                "issue_url": "https://github.com/moyaver/fleet/issues/139"
            }),
        )
        .await
        .unwrap();

    let browser = Browser::launch().await;
    let page = Page {
        client: &browser.client,
    };
    let origin = format!("http://{}", stack.addr());

    // A viewer: no entry in the navigation, and a refusal in every section of the page.
    let viewer = sign_in_session(&stack, "viewer").await;
    browser
        .set_secure_cookie(&origin, "__Host-fleet_session", &viewer)
        .await;
    browser.client.goto(&format!("{origin}/")).await.unwrap();
    page.wait(
        "the viewer's name",
        "document.querySelector('[data-testid=whoami]')?.textContent === 'Ada'",
    )
    .await;
    assert_eq!(
        page.count(&q("nav-admin")).await,
        0,
        "a viewer sees no Administration link"
    );
    browser
        .client
        .goto(&format!("{origin}/admin"))
        .await
        .unwrap();
    page.wait(
        "the refusal in the projects section",
        "document.querySelector('[data-testid=admin-projects] [data-testid=error]')?.textContent.includes('your role does not allow this')",
    )
    .await;
    for id in SECTIONS.iter().filter(|id| **id != "admin-audit") {
        let refused = format!(
            "document.querySelector('[data-testid={id}] [data-testid=error]')?.textContent.includes('your role does not allow this')"
        );
        page.wait(&format!("the refusal in {id}"), &refused).await;
    }
    assert_eq!(
        page.count("[data-testid=admin-projects] table").await,
        0,
        "no data for a viewer"
    );
    assert_eq!(
        page.count("form[aria-label]").await,
        0,
        "a viewer is shown no form to change Screen G"
    );
    browser
        .client
        .goto(&format!("{origin}/admin/audit"))
        .await
        .unwrap();
    page.wait(
        "the refusal on the audit history",
        "document.querySelector('[data-testid=error]')?.textContent.includes('your role does not allow this')",
    )
    .await;
    assert_eq!(
        page.count("table").await,
        0,
        "the audit history shows no rows to a viewer"
    );

    // An administrator: the link, every section with its data, and the audit history.
    let admin = sign_in_session(&stack, "admin").await;
    browser
        .set_secure_cookie(&origin, "__Host-fleet_session", &admin)
        .await;
    browser.client.goto(&format!("{origin}/")).await.unwrap();
    page.wait("the navigation link", &count_of(&q("nav-admin")))
        .await;
    browser
        .client
        .goto(&format!("{origin}/admin"))
        .await
        .unwrap();
    for id in SECTIONS.iter().filter(|id| **id != "admin-audit") {
        page.wait(
            &format!("{id} to show its section"),
            &format!("document.querySelector('[data-testid={id}]') !== null && document.querySelector('[data-testid={id}] [data-testid=error]') === null"),
        )
        .await;
    }
    // Each section's rows arrive on their own fetch: wait for them before reading them.
    for (section, cell) in [
        ("admin-projects", "td[data-label=Slug]"),
        ("admin-trust", "td[data-label=Name]"),
        ("admin-keys", "td[data-label=Name]"),
        ("admin-roles", "td[data-label=Person]"),
        ("admin-retention", "td[data-label=Kept]"),
        ("admin-artifacts", "td[data-label=Setting]"),
        ("admin-metrics", "td[data-label=Setting]"),
        ("admin-agents", "td[data-label=Compatible]"),
        ("admin-ingestion", "td[data-label=Message]"),
    ] {
        page.wait(
            &format!("the rows of {section}"),
            &format!(
                "{} >= 1",
                count_of(&format!("[data-testid={section}] {cell}"))
            ),
        )
        .await;
    }
    assert!(
        page.texts("[data-testid=admin-projects] td[data-label=Slug]")
            .await
            .contains(&"app".to_owned())
    );
    assert!(
        page.texts("[data-testid=admin-trust] td[data-label=Name]")
            .await
            .contains(&"release".to_owned())
    );
    let keys = page
        .texts("[data-testid=admin-keys] td[data-label=Name]")
        .await;
    assert!(
        keys.contains(&"deploy-bot".to_owned()) && keys.contains(&"e2e".to_owned()),
        "{keys:?}"
    );
    assert!(
        page.texts("[data-testid=admin-roles] td[data-label=Person]")
            .await
            .contains(&"Ada".to_owned())
    );
    assert!(
        page.texts("[data-testid=admin-retention] td[data-label=Kept]")
            .await
            .contains(&"90 days".to_owned())
    );
    assert!(
        page.texts("[data-testid=admin-artifacts] td[data-label=Setting]")
            .await
            .contains(&"artifacts.max_run_files".to_owned())
    );
    assert!(
        page.texts("[data-testid=admin-metrics] td[data-label=Setting]")
            .await
            .contains(&"agent.sample_interval".to_owned())
    );
    assert!(
        page.texts("[data-testid=admin-agents] td[data-label=Compatible]")
            .await
            .contains(&"yes".to_owned())
    );
    assert!(
        page.texts("[data-testid=admin-ingestion] td[data-label=Message]")
            .await
            .contains(&"nul byte in flow name".to_owned())
    );
    assert!(
        page.texts("[data-testid=admin-ingestion] td[data-label=Reason]")
            .await
            .contains(&"spool full".to_owned())
    );

    browser
        .client
        .goto(&format!("{origin}/admin/audit"))
        .await
        .unwrap();
    page.wait(
        "the audit history",
        &format!("{} >= 1", count_of("td[data-label=What]")),
    )
    .await;
    assert!(
        page.texts("td[data-label=What]")
            .await
            .contains(&"failure.triage".to_owned())
    );
    assert!(
        page.texts("td[data-label=Before]")
            .await
            .contains(&"unknown".to_owned())
    );
    assert!(
        page.texts("td[data-label=After]")
            .await
            .contains(&"known".to_owned())
    );
    assert!(
        page.texts("td[data-label=Who]")
            .await
            .contains(&"Ada".to_owned())
    );
    assert_eq!(browser.csp_violations().await, Vec::<String>::new());
    browser.quit().await;
}

/// Waits until `script` is true on the current page. The page may be navigating (a form was just
/// sent), so a script that fails while the page changes is retried until the deadline.
async fn until(client: &fantoccini::Client, what: &str, script: &str) {
    let expression = format!("return Boolean({script});");
    let deadline = std::time::Instant::now() + PATIENCE;
    loop {
        if let Ok(value) = client.execute(&expression, Vec::new()).await
            && value == json!(true)
        {
            return;
        }
        if std::time::Instant::now() >= deadline {
            let url = client
                .current_url()
                .await
                .map(|u| u.to_string())
                .unwrap_or_default();
            let body = run(client, "return document.body.innerText.slice(0, 2000);").await;
            panic!("timed out waiting for {what} at {url}; the page says:\n{body}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Runs `script` on the current page, retrying while it navigates.
async fn run(client: &fantoccini::Client, script: &str) -> serde_json::Value {
    let deadline = std::time::Instant::now() + PATIENCE;
    loop {
        match client.execute(script, Vec::new()).await {
            Ok(v) => return v,
            Err(e) => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the page did not answer: {e}"
                );
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

/// Fills a form's fields by name and sends it with its submit button. The form is found by its
/// accessible name, the way the dashboard labels it.
async fn send(client: &fantoccini::Client, form: &str, fields: &[(&str, &str)]) {
    let mut set = String::new();
    for (name, value) in fields {
        let value = serde_json::to_string(value).unwrap();
        let _ = write!(set, "f.querySelector('[name=\"{name}\"]').value = {value};");
    }
    let script = format!(
        "const f = document.querySelector('form[aria-label=\"{form}\"]'); {set} \
         const csrf = f.querySelector('input[name=csrf]'); \
         if (!csrf || csrf.value !== 'e2e-csrf-token') {{ return 'csrf is ' + (csrf ? JSON.stringify(csrf.value) + ' type ' + csrf.type + ' attr ' + csrf.getAttribute('value') : 'missing'); }} \
         f.querySelector('button[type=submit]').click(); return 'sent';"
    );
    let outcome = run(client, &script).await;
    assert_eq!(
        outcome,
        json!("sent"),
        "the form {form} carries the session's token"
    );
}

/// Sends the row action whose form is labelled `action`, in the row that shows `row_text`.
async fn act_on_row(client: &fantoccini::Client, row_text: &str, action: &str) {
    let script = format!(
        "const row = Array.from(document.querySelectorAll('tr')).find(r => r.textContent.includes({})); \
         row.querySelector('form[aria-label=\"{action}\"] button').click(); return true;",
        serde_json::to_string(row_text).unwrap()
    );
    run(client, &script).await;
}

#[tokio::test(flavor = "multi_thread")]
#[expect(
    clippy::too_many_lines,
    reason = "one straight-line scenario: each form in turn, then the audit history that shows them"
)]
async fn an_administrator_changes_administration_through_its_forms_and_each_change_is_audited() {
    let stack = Stack::start(|_| {}).await;
    let store = store_of(&stack);
    // A second person, to be given a role: the administrator's own role would end her session.
    let grace = store
        .upsert_user("https://idp.example", "grace", None, Some("Grace"))
        .await
        .unwrap();

    let browser = Browser::launch().await;
    let origin = format!("http://{}", stack.addr());
    let admin = sign_in_session(&stack, "admin").await;
    browser
        .set_secure_cookie(&origin, "__Host-fleet_session", &admin)
        .await;
    let client = &browser.client;

    // Project: the form creates it, and the page is sent back to its section.
    client.goto(&format!("{origin}/admin")).await.unwrap();
    until(
        client,
        "the projects section",
        "document.querySelector('form[aria-label=\"Add a project\"]') !== null",
    )
    .await;
    send(
        client,
        "Add a project",
        &[("slug", "mobile"), ("name", "Mobile")],
    )
    .await;
    until(client, "the new project", "Array.from(document.querySelectorAll('td[data-label=Slug]')).some(td => td.textContent.trim() === 'mobile')").await;

    // API key: the secret is shown on the page the form answers with, once.
    send(
        client,
        "Create an API key",
        &[
            ("project", "mobile"),
            ("name", "deploy-bot"),
            ("days", "30"),
            ("trust", "trusted"),
        ],
    )
    .await;
    until(
        client,
        "the secret page",
        "document.querySelector('#secret') !== null",
    )
    .await;
    let secret = run(
        client,
        "return document.querySelector('#secret').textContent;",
    )
    .await;
    let secret = secret.as_str().unwrap().to_owned();
    assert!(secret.starts_with("mf_live_"), "{secret}");
    client.goto(&format!("{origin}/admin")).await.unwrap();
    until(client, "the key in the list", "Array.from(document.querySelectorAll('td[data-label=Name]')).some(td => td.textContent.trim() === 'deploy-bot')").await;
    assert!(
        !run(client, "return document.body.textContent;")
            .await
            .as_str()
            .unwrap()
            .contains(&secret),
        "the secret is not shown again"
    );

    // A role on the project for Grace, and a trust policy with its own name.
    send(
        client,
        "Give a person a role",
        &[
            ("person", &grace.to_string()),
            ("project", "mobile"),
            ("role", "viewer"),
        ],
    )
    .await;
    until(client, "Grace's role", "Array.from(document.querySelectorAll('td[data-label=Person]')).some(td => td.textContent.trim() === 'Grace')").await;
    send(
        client,
        "Add a trust policy",
        &[
            ("project", "mobile"),
            ("name", "release"),
            ("repository_id", "1001"),
            ("repository_owner_id", "2002"),
            ("trusted_refs", "refs/heads/main"),
        ],
    )
    .await;
    until(client, "the trust policy", "Array.from(document.querySelectorAll('td[data-label=Name]')).some(td => td.textContent.trim() === 'release')").await;

    // Revoke the key: its row says so.
    act_on_row(client, "deploy-bot", "Revoke the API key").await;
    until(client, "the key revoked", "Array.from(document.querySelectorAll('tr')).some(r => r.textContent.includes('deploy-bot') && r.textContent.includes('revoked'))").await;

    // The audit history shows each change, with the values it changed.
    client.goto(&format!("{origin}/admin/audit")).await.unwrap();
    until(
        client,
        "the audit history",
        "document.querySelectorAll('td[data-label=What]').length >= 5",
    )
    .await;
    let actions = page_texts(client, "td[data-label=What]").await;
    for action in [
        "project.create",
        "api_key.create",
        "role_binding.add",
        "trust_policy.add",
        "api_key.revoke",
    ] {
        assert!(
            actions.contains(&action.to_owned()),
            "{action} in {actions:?}"
        );
    }
    let revoke = run(
        client,
        "const row = Array.from(document.querySelectorAll('tr')).find(r => r.querySelector('td[data-label=What]')?.textContent.trim() === 'api_key.revoke'); \
         return {before: row.querySelector('td[data-label=Before]').textContent, after: row.querySelector('td[data-label=After]').textContent};",
    )
    .await;
    assert!(
        revoke["before"]
            .as_str()
            .unwrap()
            .contains("\"revoked\":false"),
        "{revoke}"
    );
    assert!(
        revoke["after"]
            .as_str()
            .unwrap()
            .contains("\"revoked\":true"),
        "{revoke}"
    );
    let created = run(
        client,
        "const row = Array.from(document.querySelectorAll('tr')).find(r => r.querySelector('td[data-label=What]')?.textContent.trim() === 'project.create'); \
         return row.querySelector('td[data-label=After]').textContent;",
    )
    .await;
    assert!(created.as_str().unwrap().contains("mobile"), "{created}");
    assert!(
        !run(client, "return document.body.textContent;")
            .await
            .as_str()
            .unwrap()
            .contains(&secret),
        "the audit history never shows the secret"
    );
    assert_eq!(browser.csp_violations().await, Vec::<String>::new());
    browser.quit().await;
}

/// The text of every element matching `css`, in document order.
async fn page_texts(client: &fantoccini::Client, css: &str) -> Vec<String> {
    let v = run(
        client,
        &format!(
            "return Array.from(document.querySelectorAll({})).map(e => e.textContent.trim());",
            serde_json::to_string(css).unwrap()
        ),
    )
    .await;
    serde_json::from_value(v).unwrap()
}

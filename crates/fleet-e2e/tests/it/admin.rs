// SPDX-License-Identifier: AGPL-3.0-only
//! Screen G in a real browser (spec §8 "Screen G — Administration"; PLAN §10.3, M6): an
//! administrator sees every section with seeded data and the audit history; a viewer gets no
//! navigation entry, and a refusal on each administration URL.

use std::time::Duration;

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
    dashboard_ab::{Page, count_of, q, run_options, sign_in_session},
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
    page.wait(
        "the projects table",
        &format!("{} >= 1", count_of("[data-testid=admin-projects] tbody tr")),
    )
    .await;
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

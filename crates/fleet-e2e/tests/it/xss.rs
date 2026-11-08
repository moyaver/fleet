// SPDX-License-Identifier: AGPL-3.0-only
//! The XSS corpus (threat T8, issue #140): hostile strings in every user-controlled field that
//! reaches the dashboard, rendered in a real browser on every screen that shows them.
//!
//! The fields are flow names, step text, failure messages, the crash exception and the branch or
//! tag of a run (seeded by `fleet-loadgen` with [`CORPUS`]), the flow catalog's tags on screen E
//! and a device's model and OS version on its page (issue #85), and the artifacts a flow uploads:
//! the Maestro log with ANSI and OSC sequences, the device logcat excerpt and a UI hierarchy, whose
//! file names are hostile too (uploaded here over the artifact protocol, as an agent would).
//!
//! Screen G (the administration pages, issue #139) shows the user-controlled fields of the
//! installation too: a project's name, a publisher policy's name, an API key's name, a person's
//! display name (on a role binding and in the audit history), an identity-provider group, a refused
//! event's message, a loss's reason, an agent's version and an audited change's recorded values.
//! [`seed_administration`] writes them with the corpus.
//!
//! Every payload sets `window.__xss = 1` when it runs, so a screen fails when a payload ran, when
//! an injected element or an `on*` attribute exists, when a script is not the app's own, when a
//! hostile string is not shown as literal text, or when the page reported a CSP violation.

use std::net::TcpListener;
use std::time::Duration;

use fleet_core::policy::Role;
use fleet_loadgen::Options;
use fleet_server::config::{Secret, TrustPolicyConfig};
use fleet_store::{
    PoolOptions, Store,
    auth::{AuditActor, PolicyRow, TrustLevel},
    sessions::GroupMapping,
    testing,
};
use serde_json::{Value, json};
use time::{Duration as TimeDuration, OffsetDateTime};
use uuid::Uuid;

use crate::{
    artifacts::temp,
    browser::Browser,
    dashboard_ab::{Page, count_of, q, run_options, sign_in_session, stack},
    dashboard_cd::worker_of,
    dashboard_ef::attempt_of,
    m5_exit::sha256_hex,
    support::{self, Stack},
};

/// Hostile strings for every user-visible loadgen field. Each one runs `window.__xss = 1` if it is
/// ever parsed as markup or script, and each one stays the same string as text.
const CORPUS: &[&str] = &[
    "</script><script>window.__xss = 1</script>",
    "<img src=x onerror=\"window.__xss = 1\">",
    "<svg onload=\"window.__xss = 1\"></svg>",
    "\"><script>window.__xss = 1</script>",
    "' onmouseover='window.__xss = 1' x='",
    "&lt;b&gt;bold&lt;/b&gt; &amp; &#60;i&#62;",
    "{{ 7*7 }} ${7*7} {{constructor.constructor('window.__xss = 1')()}}",
    "<iframe srcdoc=\"<script>window.__xss = 1</script>\"></iframe>",
    "\u{1b}[31mred\u{1b}[0m \u{1b}]8;;javascript:window.__xss = 1\u{7}link\u{1b}]8;;\u{7} \u{1b}]0;title\u{7}",
    "\u{1b}[2J\u{1b}[H\u{1b}[?25l\u{1b}P1$t\u{1b}\\ dcs",
];

/// The corpus items that are visible as plain text once any escape sequences are gone: each must
/// appear literally, character for character, where it is shown.
const LITERAL: &[&str] = &[
    "</script><script>window.__xss = 1</script>",
    "<img src=x onerror=\"window.__xss = 1\">",
    "<svg onload=\"window.__xss = 1\"></svg>",
    "&lt;b&gt;bold&lt;/b&gt; &amp; &#60;i&#62;",
    "{{ 7*7 }} ${7*7}",
];

/// Maestro log lines, as the page shows them after the ANSI parser: each escape sequence is styling
/// or control, so only the text around it is visible.
const LOG_LINES: &[&str] = &[
    "I/ActivityManager: <img src=x onerror=\"window.__xss = 1\">",
    "\u{1b}[1;31mE/AndroidRuntime\u{1b}[0m: </script><script>window.__xss = 1</script>",
    "\u{1b}]8;;javascript:window.__xss = 1\u{7}click me\u{1b}]8;;\u{7} and \u{1b}]0;pwned\u{7} ok",
    "\u{1b}[2J\u{1b}[H cursor reset ${7*7} {{ 7*7 }}",
    "&lt;b&gt; stays entities",
];

/// A logcat line of the failed flow, as the logcat viewer shows it: the logcat prefix, then the
/// corpus item. Only the logcat carries this prefix with an `<svg` in it.
const LOGCAT_MARK: &str = "E/AndroidRuntime: <svg onload=\"window.__xss = 1\"></svg>";

/// Branch of run A (a `refs/heads/` name shown by the pipeline header and the history) and tag of
/// run B (shown the same way by `ref_name`). Git refs have no spaces, so the payloads have none.
const BRANCH: &str = "refs/heads/feat/<img/src=x/onerror=\"window.__xss = 1\">";
const TAG: &str = "refs/tags/javascript:window.__xss=1";

/// Seeds one finished run of the corpus on the project's one commit: every flow fails, with the
/// corpus as its names, steps, failure messages and crashes.
fn corpus_run(stack: &Stack, run_id: &str, git_ref: &str) -> Options {
    Options {
        pace: Duration::ZERO,
        commit: Some("abc1234def5678".into()),
        git_ref: Some(git_ref.into()),
        corpus: CORPUS.iter().map(|s| (*s).to_owned()).collect(),
        devices: 2,
        flows: 4,
        ..run_options(stack, run_id)
    }
}

/// A stack whose dashboard is `http://127.0.0.1:<port>` and whose usercontent origin is
/// `http://localhost:<port>`, as the flow page needs to fetch its log. Returns the port. The
/// project's name and one publisher policy's name are corpus items, which Screen G shows.
async fn corpus_stack() -> (Stack, u16) {
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let stack = stack(|c| {
        c.listen = ([127, 0, 0, 1], port).into();
        c.api.public_url = Some(format!("http://127.0.0.1:{port}"));
        c.usercontent.url = Some(format!("http://localhost:{port}"));
        c.usercontent.signing_key = Some(Secret::from("0123456789abcdef0123456789abcdef-xss"));
        c.storage.path = temp("xss-blobs");
        c.artifacts.spool_dir = temp("xss-spool");
        c.projects[0].name = LITERAL[0].into();
        c.projects[0].trust_policies = vec![TrustPolicyConfig {
            name: LITERAL[2].into(),
            issuer: "https://token.actions.githubusercontent.com".into(),
            audience: "fleet".into(),
            repository_id: "1001".into(),
            repository_owner_id: "2002".into(),
            trusted_refs: Vec::new(),
            allowed_events: Vec::new(),
            pr_policy: "restricted".into(),
            job_workflow_ref: None,
            environment: None,
        }];
    })
    .await;
    (stack, port)
}

/// A connection to the stack's database, to seed rows through the store.
fn store_of(stack: &Stack) -> Store {
    let url = stack.config.database.url.as_ref().unwrap().expose();
    Store::connect_lazy(url, PoolOptions::default()).unwrap()
}

/// Screen G's user-controlled fields, written with the corpus: an API key's name, a person's
/// display name (on a role binding and as the actor of an audited change), an identity-provider
/// group, a refused event's message, a loss's reason, an agent's version, and the before, after and
/// recorded details of the change. The project's name and a publisher policy's name are set by
/// [`corpus_stack`]. The literal items among them must come out as text.
async fn seed_administration(stack: &Stack, run: Uuid) {
    let store = store_of(stack);
    let project = store.project_id("app").await.unwrap().unwrap();
    let person = store
        .upsert_user("https://idp.example", "xss-person", None, Some(LITERAL[3]))
        .await
        .unwrap();
    // The changes go through Screen G's own writes, so each one is audited as it would be live.
    let actor = AuditActor::User(person);
    store
        .create_api_key_as(
            actor,
            project,
            LITERAL[1],
            TrustLevel::Trusted,
            OffsetDateTime::now_utc() + TimeDuration::days(90),
        )
        .await
        .unwrap();
    // A project made on the dashboard, with a hostile name, then renamed to another one: the
    // rename's before and after are in the audit history.
    let made = store
        .create_project(actor, "xss-dash", CORPUS[1])
        .await
        .unwrap();
    store.rename_project(actor, made, CORPUS[7]).await.unwrap();
    testing::seed_role_binding(&store, Some(project), person, "viewer")
        .await
        .unwrap();
    store
        .sync_group_mappings(
            "https://idp.example",
            &[GroupMapping {
                project: Some(project),
                group: LITERAL[4].into(),
                role: Role::Viewer,
            }],
        )
        .await
        .unwrap();
    // A group added on the dashboard, and a publisher policy added there with hostile fields.
    store
        .add_group_mapping(
            actor,
            "https://idp.example",
            Some(project),
            CORPUS[4],
            Role::Viewer,
        )
        .await
        .unwrap();
    store
        .add_trust_policy(
            actor,
            project,
            &PolicyRow {
                name: LITERAL[3].into(),
                issuer: "https://token.actions.githubusercontent.com".into(),
                audience: "fleet".into(),
                policy: fleet_core::trust::Policy {
                    repository_id: "3003".into(),
                    repository_owner_id: "4004".into(),
                    trusted_refs: Vec::new(),
                    allowed_events: Vec::new(),
                    pr_policy: fleet_core::trust::PrPolicy::Restricted,
                    job_workflow_ref: Some(CORPUS[3].into()),
                    environment: Some(CORPUS[6].into()),
                },
            },
        )
        .await
        .unwrap();
    testing::seed_ingest_rejection(&store, run, 1, "invalid", CORPUS[6])
        .await
        .unwrap();
    testing::seed_ingest_gap(&store, run, "telemetry", 1, 2, CORPUS[7])
        .await
        .unwrap();
    testing::seed_agent_version(&store, run, CORPUS[3])
        .await
        .unwrap();
    store
        .audit(
            actor,
            Some(project),
            "failure.triage",
            json!({
                "fingerprint": "xss",
                "from": CORPUS[1],
                "to": CORPUS[2],
                "issue_url": CORPUS[0],
            }),
        )
        .await
        .unwrap();
}

/// `prefix` and each item, one per line.
fn lines<'a>(items: impl Iterator<Item = &'a str>, prefix: &str) -> String {
    let mut out = String::new();
    for item in items {
        out.push_str(prefix);
        out.push_str(item);
        out.push('\n');
    }
    out
}

/// Uploads one artifact of `flow` over the artifact protocol, as an agent with the project's key
/// would: request a grant, send the bytes, complete. Asserts each step succeeds.
async fn upload(
    stack: &Stack,
    run: &str,
    flow: &str,
    (class, path, content_type): (&str, &str, &str),
    bytes: &[u8],
    step: Option<u32>,
) {
    let request = json!({
        "agent_run_id": run, "flow_execution_id": flow, "class": class, "step": step,
        "path": path, "content_type": content_type,
        "size": bytes.len(), "sha256": sha256_hex(bytes),
    });
    let key = Some(stack.key.as_str());
    let (status, body) = support::http(
        stack.addr(),
        "POST",
        "/api/v1/agent/artifacts",
        key,
        &request.to_string(),
    )
    .await;
    assert_eq!(status, 201, "{class}: {body}");
    let grant: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(grant["disposition"], "upload", "{class}: {grant}");
    let text = String::from_utf8_lossy(bytes).into_owned();
    let (status, body) = support::http(
        stack.addr(),
        "PUT",
        grant["url"].as_str().unwrap(),
        key,
        &text,
    )
    .await;
    assert_eq!(status, 204, "{class}: {body}");
    let (status, body) = support::http(
        stack.addr(),
        "POST",
        grant["complete_url"].as_str().unwrap(),
        key,
        "",
    )
    .await;
    assert_eq!(status, 200, "{class}: {body}");
    assert!(body.contains("\"available\""), "{class}: {body}");
}

/// The evidence of the failed flow `flow` of run `run`: a Maestro log, a logcat excerpt and a UI
/// hierarchy, each with a hostile file name, and the log and hierarchy with hostile content.
async fn upload_evidence(stack: &Stack, run: &str, flow: &str) {
    let log = lines(LOG_LINES.iter().copied(), "");
    upload(
        stack,
        run,
        flow,
        ("maestro_log", "logs/maestro.log", "text/plain"),
        log.as_bytes(),
        None,
    )
    .await;
    let logcat = lines(
        CORPUS.iter().copied(),
        "11-04 02:25:28.000 E/AndroidRuntime: ",
    );
    upload(
        stack,
        run,
        flow,
        (
            "device_logcat",
            "logcat/<svg onload=\"window.__xss = 1\"></svg>.txt",
            "text/plain",
        ),
        logcat.as_bytes(),
        None,
    )
    .await;
    let hierarchy = json!({
        "class": "android.widget.TextView",
        "text": CORPUS[1],
        "contentDescription": CORPUS[2],
        "children": [{ "text": CORPUS[0], "resourceId": CORPUS[3] }],
    });
    upload(
        stack,
        run,
        flow,
        (
            "failure_hierarchy",
            "screen-hierarchy/step-001-\"><img src=x onerror=\"window.__xss = 1\">.json",
            "application/json",
        ),
        hierarchy.to_string().as_bytes(),
        Some(0),
    )
    .await;
}

/// The id of the newest failed flow of `attempt`, and its name and failure message.
async fn failed_flow(stack: &Stack, attempt: &str) -> (String, String, String) {
    let snap = stack
        .get(&format!("/api/v1/projects/app/attempts/{attempt}"))
        .await;
    let flow = snap["flows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["state"] == "failed")
        .unwrap_or_else(|| panic!("no failed flow: {snap}"));
    let id = flow["id"].as_str().unwrap().to_owned();
    let detail = stack.get(&format!("/api/v1/projects/app/flows/{id}")).await;
    (
        id,
        flow["name"].as_str().unwrap().to_owned(),
        detail["failure"]["message"].as_str().unwrap().to_owned(),
    )
}

/// JS, run on every screen: what in the document is an injection, as a list of problems. An `on*`
/// attribute, an inline script that carries a payload, a script from another origin, a frame or an
/// object, an `img` with `src="x"`, and a `javascript:` or `data:text/html` URL are all injections:
/// the app never renders any of them, so none may exist.
const SCAN: &str = r"
const problems = [];
const origin = location.origin;
for (const el of document.querySelectorAll('*')) {
    const tag = el.tagName.toLowerCase();
    for (const a of Array.from(el.attributes)) {
        if (/^on/i.test(a.name)) problems.push(tag + ' carries ' + a.name);
    }
    if (['iframe', 'frame', 'frameset', 'object', 'embed', 'applet', 'base'].includes(tag)) {
        problems.push('injected <' + tag + '>');
    }
    if (tag === 'script') {
        if (!el.src && /__xss/.test(el.textContent)) problems.push('inline script carries a payload');
        else if (el.src && !el.src.startsWith(origin + '/')) problems.push('script from ' + el.src);
    }
    if (tag === 'img' && el.getAttribute('src') === 'x') problems.push('img src=x');
    for (const attr of ['href', 'src', 'xlink:href', 'action', 'formaction']) {
        const v = (el.getAttribute(attr) || '').trim().toLowerCase();
        if (v.startsWith('javascript:') || v.startsWith('data:text/html')) {
            problems.push(tag + ' ' + attr + ' is a script URL');
        }
    }
}
return { problems, xss: window.__xss ?? null };
";

/// JS: which of `arguments[0]` the page does not show as text.
const MISSING: &str = r"const text = document.body.textContent;
return arguments[0].filter(n => !text.includes(n));";

/// One screen to render, its address, the condition that says it has loaded, and the literal
/// texts it must show.
struct Screen {
    name: &'static str,
    path: String,
    ready: String,
    shows: Vec<String>,
}

fn literal_shows() -> Vec<String> {
    LITERAL.iter().map(|s| (*s).to_owned()).collect()
}

#[expect(
    clippy::too_many_lines,
    reason = "one straight-line scenario: seed, then render every screen"
)]
#[tokio::test(flavor = "multi_thread")]
async fn every_user_field_renders_as_text_on_every_screen_and_no_payload_runs() {
    let (stack, port) = corpus_stack().await;
    // Run A has a branch, run B a tag; the second run makes the comparison page possible.
    fleet_loadgen::run(corpus_run(&stack, "xss-a", BRANCH))
        .await
        .unwrap();
    let ended_a = stack.await_run_ended("job-0").await;
    let a = attempt_of(&stack, "xss-a").await;
    let (flow, flow_name, failure) = failed_flow(&stack, &a).await;
    upload_evidence(
        &stack,
        ended_a["agent_runs"][0]["id"].as_str().unwrap(),
        &flow,
    )
    .await;
    fleet_loadgen::run(corpus_run(&stack, "xss-b", TAG))
        .await
        .unwrap();
    stack.await_run_ended("job-0").await;
    let b = attempt_of(&stack, "xss-b").await;

    let snapshot = stack
        .get(&format!("/api/v1/projects/app/attempts/{a}"))
        .await;
    let device = snapshot["devices"][0]["id"].as_str().unwrap().to_owned();
    let worker = worker_of(&stack, &snapshot).await;
    let branch = BRANCH.trim_start_matches("refs/heads/").to_owned();
    let tag = TAG.trim_start_matches("refs/tags/").to_owned();
    // The device's model and OS version came from the corpus (issue #85): the device page shows
    // them in its header, so they are checked there as text.
    let detail = stack
        .get(&format!("/api/v1/projects/app/devices/{device}"))
        .await;
    let profile = &detail["session"]["profile"];
    let model = profile["model"]
        .as_str()
        .expect("the corpus gives the device a model")
        .to_owned();
    let os_version = profile["os_version"]
        .as_str()
        .expect("the corpus gives the device an OS version")
        .to_owned();

    // The flow page shows the flow's own name, its failure message and its artifacts, and the
    // log lines whose text is plain.
    let mut flow_shows = vec![flow_name.clone(), failure.clone()];
    // Its steps are the corpus items at its index: loadgen gives step `s` of flow `k` item `2k + s`.
    let k: usize = {
        let (device, flow) = flow_name
            .rsplit(' ')
            .next()
            .unwrap()
            .split_once('-')
            .unwrap();
        device.parse::<usize>().unwrap() * 4 + flow.parse::<usize>().unwrap()
    };
    flow_shows.extend((0..2).map(|s| CORPUS[(2 * k + s) % CORPUS.len()].to_owned()));
    flow_shows.extend(
        LOG_LINES
            .iter()
            .filter(|l| !l.contains('\u{1b}'))
            .map(|s| (*s).to_owned()),
    );
    // The escape-free text of the log lines with escapes is shown, the escapes themselves are not.
    flow_shows.extend(
        [
            "E/AndroidRuntime: </script><script>window.__xss = 1</script>",
            "click me",
            "cursor reset ${7*7} {{ 7*7 }}",
        ]
        .map(String::from),
    );
    // The hierarchy's file name is shown; the logcat's is not (it downloads under its own label).
    flow_shows.push("step-001-\"><img src=x onerror=\"window.__xss = 1\">.json".into());
    let screens = [
        Screen {
            name: "overview",
            path: "/".into(),
            ready: format!("{} >= 1", count_of(&q("recent-row"))),
            shows: vec![branch.clone()],
        },
        Screen {
            name: "pipeline",
            path: format!("/p/app/pipelines/{a}"),
            ready: format!("{} >= 1", count_of(&q("flow-row"))),
            shows: [literal_shows(), vec![branch.clone()]].concat(),
        },
        Screen {
            name: "flow",
            path: format!("/flows/{flow}?project=app"),
            // The logcat's own line is the one that shows it has loaded: the Maestro log has no
            // `<svg` after its logcat prefix.
            ready: format!(
                "{} === 1 && {} >= 2 && {} === 1 && document.body.textContent.includes({}) && document.body.textContent.includes({})",
                count_of(&q("flow-page")),
                count_of("[role=treeitem]"),
                count_of(&q("crash-logcat")),
                serde_json::to_string(LOG_LINES[0]).unwrap(),
                serde_json::to_string(LOGCAT_MARK).unwrap(),
            ),
            shows: [flow_shows, literal_shows()].concat(),
        },
        Screen {
            name: "history",
            path: "/history?project=app".into(),
            ready: format!("{} >= 1", count_of(&q("history-row"))),
            shows: [literal_shows(), vec![branch.clone(), tag.clone()]].concat(),
        },
        // Screen E's flow catalog lists the workspace's flows with their tags. Loadgen tags the
        // first four corpus items that fit a tag (64 bytes), which here are the first four items.
        Screen {
            name: "flow catalog",
            path: "/history?project=app&flow=flows%2F".into(),
            ready: format!("{} >= 1", count_of(&q("catalog-row"))),
            shows: CORPUS[..4].iter().map(|s| (*s).to_owned()).collect(),
        },
        Screen {
            name: "compare",
            path: format!("/compare?project=app&a={a}&b={b}"),
            ready: format!("{} === 1", count_of(&q("comparison"))),
            shows: literal_shows(),
        },
        Screen {
            name: "worker",
            path: format!("/workers/{worker}?project=app"),
            ready: format!("{} === 1", count_of(&q("worker-page"))),
            shows: Vec::new(),
        },
        Screen {
            name: "administration",
            path: "/admin".into(),
            ready: format!(
                "{} >= 1 && document.body.textContent.includes({})",
                count_of("[data-testid=admin-keys] tbody tr"),
                serde_json::to_string(CORPUS[7]).unwrap(),
            ),
            shows: [
                literal_shows(),
                vec![
                    CORPUS[0].to_owned(),
                    CORPUS[1].to_owned(),
                    CORPUS[2].to_owned(),
                    CORPUS[3].to_owned(),
                    CORPUS[4].to_owned(),
                    CORPUS[6].to_owned(),
                    CORPUS[7].to_owned(),
                ],
            ]
            .concat(),
        },
        Screen {
            name: "audit history",
            path: "/admin/audit".into(),
            ready: format!(
                "document.body.textContent.includes({})",
                serde_json::to_string(CORPUS[2]).unwrap(),
            ),
            shows: vec![
                LITERAL[3].to_owned(),
                CORPUS[0].to_owned(),
                CORPUS[1].to_owned(),
                CORPUS[2].to_owned(),
            ],
        },
        Screen {
            name: "device",
            path: format!("/devices/{device}?project=app"),
            ready: format!(
                "{} === 1 && {} >= 1",
                count_of(&q("device-page")),
                count_of(&q("device-flow"))
            ),
            // The device's flows are the first shard's: the first corpus items among them. Its
            // header names its model and OS version.
            shows: [literal_shows()[..3].to_vec(), vec![model, os_version]].concat(),
        },
    ];

    let base = format!("http://127.0.0.1:{port}");
    // Screen G is for global administrators: the browser signs in as one for the administration
    // pages (and as one reads every other page, which is what the other screens expect).
    seed_administration(
        &stack,
        ended_a["agent_runs"][0]["id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap(),
    )
    .await;
    let browser = Browser::launch().await;
    let admin = sign_in_session(&stack, "admin").await;
    browser
        .set_secure_cookie(&base, "__Host-fleet_session", &admin)
        .await;
    let page = Page {
        client: &browser.client,
    };
    let mut problems = Vec::new();
    for screen in &screens {
        browser
            .client
            .goto(&format!("{base}{}", screen.path))
            .await
            .unwrap();
        page.wait(&format!("the {} screen", screen.name), &screen.ready)
            .await;
        let scan = page.eval(SCAN, vec![]).await;
        for problem in scan["problems"].as_array().unwrap() {
            problems.push(format!("{}: {problem}", screen.name));
        }
        if !scan["xss"].is_null() {
            problems.push(format!(
                "{}: a payload ran: window.__xss is {}",
                screen.name, scan["xss"]
            ));
        }
        let missing = page.eval(MISSING, vec![json!(screen.shows)]).await;
        for text in missing.as_array().unwrap() {
            problems.push(format!("{}: not shown as text: {text}", screen.name));
        }
        for violation in browser.csp_violations().await {
            problems.push(format!("{}: CSP violation {violation}", screen.name));
        }
    }
    browser.quit().await;
    assert!(
        problems.is_empty(),
        "hostile text reached the DOM as markup or was not shown literally:\n{}",
        problems.join("\n")
    );
}

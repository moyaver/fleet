// SPDX-License-Identifier: AGPL-3.0-only
//! The authorization matrix (research 06 T16, issue #140): every REST and live route against every
//! principal (anonymous, a publisher key, a stranger with a role on another project only, and the
//! viewer, maintainer and admin of this project).
//!
//! The route list is read from the router's own source: every `.route("…", method(…))` literal in
//! the files that build it. A route added without a row in [`class_of`] fails
//! `every_route_has_a_row`, and every route found is exercised against every principal.

use std::collections::HashMap;

use axum::http::Method;
use axum_test::{TestRequest, TestResponse, WsMessage};
use fleet_core::policy::{Grant, Role};
use fleet_protocol::brand::WS_SUBPROTOCOL;
use fleet_protocol::event::{FlowOutcome, StepStatus};
use serde_json::{Value, json};
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use super::rig::{Agent, Rig, rig, sample, telemetry};
use crate::session;

/// The CSRF token of every signed-in session the matrix creates, and the header that echoes it.
const CSRF: &str = "csrf";
const CSRF_HEADER: &str = "x-csrf-token";

/// Who is asking. `Stranger` is signed in with an admin role on the other project only, `Admin`
/// is an admin of this project, and `GlobalAdmin` holds the admin role for every project (the
/// only grant that reaches installation-wide administration, Screen G).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Who {
    Anonymous,
    Publisher,
    Stranger,
    Viewer,
    Maintainer,
    Admin,
    GlobalAdmin,
}

const WHO: [Who; 7] = [
    Who::Anonymous,
    Who::Publisher,
    Who::Stranger,
    Who::Viewer,
    Who::Maintainer,
    Who::Admin,
    Who::GlobalAdmin,
];

/// How a route is authorized. Every route found in the router must have one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    /// A project's screens: viewer and up read; no role on the project reads as missing (404).
    ProjectRead,
    /// The project list: any grant; the answer is filtered to the caller's projects.
    ProjectList,
    /// The browser's live socket: refused at upgrade without a credential, then per project.
    Live,
    /// Publisher credentials only: humans and anonymous callers never get a success.
    Agent,
    /// Who am I: anonymous is refused, any signed-in user gets an answer.
    WhoAmI,
    /// Sign-out: needs a session, and the CSRF token that a bare request lacks.
    Logout,
    /// Failure triage (a write): a viewer is refused, a maintainer or admin gets past the role
    /// check and reaches the handler, which has no such failure to change.
    ProjectTriage,
    /// A flow's screen D by its ID alone: a reader of the flow's project is redirected to the page
    /// (303); anyone else gets the answer for a missing flow.
    FlowPage,
    /// Not authorized by role: the same answer for everyone (public documents and probes, the
    /// sign-in flow, capability links).
    RoleIndependent,
    /// Screen G (installation-wide administration and the audit history): anonymous callers and
    /// publishers are refused as unauthenticated, and every other signed-in caller without a
    /// global administrator grant is `403`, project admins included. Only a global admin reads.
    Admin,
}

/// The class of a route as the router names it, or `None` for a route nobody has classified yet.
fn class_of(route: &str) -> Option<Class> {
    Some(match route {
        "/api/v1/projects" => Class::ProjectList,
        "/api/v1/live" => Class::Live,
        "/api/v1/auth/session" => Class::WhoAmI,
        "/api/v1/auth/logout" => Class::Logout,
        "/api/v1/plans" => Class::Agent,
        "/api/v1/projects/{project}/failures/{fingerprint}/triage" => Class::ProjectTriage,
        "/api/v1/flows/{execution_id}/page" => Class::FlowPage,
        // The rerun manifest (#82): a download a project reader gets, as the project's screens.
        "/api/v1/projects/{project}/attempts/{attempt}/failed-flows" => Class::ProjectRead,
        "/api/v1/admin/projects"
        | "/api/v1/admin/trust-policies"
        | "/api/v1/admin/api-keys"
        | "/api/v1/admin/role-mappings"
        | "/api/v1/admin/retention"
        | "/api/v1/admin/artifact-policy"
        | "/api/v1/admin/metric-policy"
        | "/api/v1/admin/agents"
        | "/api/v1/admin/ingestion"
        | "/api/v1/admin/audit" => Class::Admin,
        "/api/v1/openapi.json"
        | "/livez"
        | "/readyz"
        | "/api/v1/auth/login"
        | "/api/v1/auth/callback" => Class::RoleIndependent,
        r if r.starts_with("/api/v1/agent/") => Class::Agent,
        r if r.starts_with("/api/v1/projects/") => Class::ProjectRead,
        r if r.starts_with("/u/") => Class::RoleIndependent,
        _ => return None,
    })
}

/// The files that build the router, as `include_str!` sees them from this module.
const ROUTER_SOURCES: [(&str, &str); 6] = [
    ("api.rs", include_str!("../../api.rs")),
    ("ingest.rs", include_str!("../../ingest.rs")),
    ("artifacts.rs", include_str!("../../artifacts.rs")),
    ("login.rs", include_str!("../../login.rs")),
    ("health.rs", include_str!("../../health.rs")),
    (
        "artifacts/usercontent.rs",
        include_str!("../../artifacts/usercontent.rs"),
    ),
];

/// The methods a `.route(…)` call may chain, as `method(handler)`.
const METHODS: [&str; 5] = ["get", "post", "put", "patch", "delete"];

/// The `(path, method)` pairs of every `.route(…)` call in `src`, one per chained method, and how
/// many calls have a path that is a string literal. A call whose path is not a literal is not
/// returned, so the caller compares the count with the number of calls.
fn routes_in(src: &str) -> (Vec<(String, String)>, usize) {
    let mut found = Vec::new();
    let mut literal_paths = 0;
    for (at, _) in src.match_indices(".route(") {
        let rest = src[at + ".route(".len()..].trim_start();
        let Some(literal) = rest.strip_prefix('"') else {
            continue;
        };
        let Some(end) = literal.find('"') else {
            continue;
        };
        literal_paths += 1;
        let path = &literal[..end];
        let args = call_args(&literal[end + 1..]);
        let mut methods: Vec<&str> = METHODS
            .into_iter()
            .filter(|m| calls_method(args, m))
            .collect();
        // A route with no method the matrix knows stays in the list, so `request` refuses it.
        if methods.is_empty() {
            methods.push("");
        }
        found.extend(methods.into_iter().map(|m| (path.to_owned(), m.to_owned())));
    }
    (found, literal_paths)
}

/// The arguments of a call whose `(` was just before `s`: the text up to its matching `)`.
fn call_args(s: &str) -> &str {
    let mut depth = 0_usize;
    for (i, c) in s.char_indices() {
        match c {
            '(' => depth += 1,
            ')' if depth == 0 => return &s[..i],
            ')' => depth -= 1,
            _ => {}
        }
    }
    s
}

/// Whether `args` calls `method(` as a whole word, as in `put(a).delete(b)`.
fn calls_method(args: &str, method: &str) -> bool {
    args.match_indices(&format!("{method}(")).any(|(at, _)| {
        args[..at]
            .chars()
            .next_back()
            .is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_'))
    })
}

/// Every route of the router, from its sources. Fails if one of them isn't a literal.
fn derived_routes() -> Vec<(String, String)> {
    let mut all = Vec::new();
    for (name, src) in ROUTER_SOURCES {
        let (found, literal_paths) = routes_in(src);
        assert_eq!(
            literal_paths,
            src.matches(".route(").count(),
            "{name}: every .route() path must be a string literal the matrix can read"
        );
        all.extend(found);
    }
    all
}

#[test]
fn every_route_has_a_row() {
    let routes = derived_routes();
    // The scan is the security test's input: an empty or truncated list would pass vacuously.
    assert!(routes.len() >= 30, "found only {} routes", routes.len());
    let unclassified: Vec<_> = routes
        .iter()
        .filter(|(path, _)| class_of(path).is_none())
        .map(|(path, method)| format!("{method} {path}"))
        .collect();
    assert!(
        unclassified.is_empty(),
        "routes without an authorization row in class_of: {unclassified:?}"
    );
}

/// The IDs the matrix substitutes into route templates, from a run that exists in project `app`.
struct Ids {
    attempt: String,
    worker: String,
    device: String,
    flow: String,
}

async fn seed(rig: &Rig) -> Ids {
    let mut agent = Agent::new("800", "build", "runner-a");
    agent.started();
    let shard = agent.shard("emulator-5554", &["login"]);
    let login = agent.flow_started(&shard, "login", 0);
    agent.step(&login, 0, "Launch app", StepStatus::Completed);
    agent.flow_ended(&login, FlowOutcome::Passed, None);
    // A failed flow, so the rerun manifest (#82) has something to answer with.
    let checkout = agent.flow_started(&shard, "checkout", 1);
    agent.flow_ended(&checkout, FlowOutcome::Failed, Some("Element not found"));
    let at = OffsetDateTime::now_utc() - Duration::minutes(1);
    rig.publish(&mut agent, vec![telemetry(1, vec![sample(at, 10.0)])])
        .await;
    let attempt = rig.attempt_id("app").await;
    let snapshot = rig
        .json(&format!("/api/v1/projects/app/attempts/{attempt}"))
        .await;
    Ids {
        attempt,
        worker: snapshot["agent_runs"][0]["worker_id"]
            .as_str()
            .unwrap()
            .to_owned(),
        device: snapshot["devices"][0]["id"].as_str().unwrap().to_owned(),
        flow: snapshot["flows"][0]["id"].as_str().unwrap().to_owned(),
    }
}

/// Fills a route template with real IDs. Placeholders with no ID of their own take a dummy value.
fn concrete(template: &str, ids: &Ids) -> String {
    let mut path = template
        .replace("{project}", "app")
        .replace("{attempt}", &ids.attempt)
        .replace("{worker}", &ids.worker)
        .replace("{device}", &ids.device)
        .replace("{execution_id}", &ids.flow)
        .replace("{name}", "login");
    while let Some(start) = path.find('{') {
        let Some(len) = path[start..].find('}') else {
            break;
        };
        path.replace_range(start..=start + len, "0");
    }
    if path.contains("/agent/artifacts/") {
        path = path.replace("/0/", &format!("/{}/", Uuid::nil()));
    }
    if path.ends_with("/compare") {
        path = format!("{path}?a={}&b={}", ids.attempt, ids.attempt);
    }
    path
}

/// Session cookies for the signed-in principals, one per role.
async fn cookies(rig: &Rig) -> HashMap<Who, String> {
    let app = rig.store.project_id("app").await.unwrap().unwrap();
    let other = rig.store.project_id("other").await.unwrap().unwrap();
    let grants = [
        (
            Who::Stranger,
            vec![Grant {
                project: Some(other),
                role: Role::Admin,
            }],
        ),
        (
            Who::Viewer,
            vec![Grant {
                project: Some(app),
                role: Role::Viewer,
            }],
        ),
        (
            Who::Maintainer,
            vec![Grant {
                project: Some(app),
                role: Role::Maintainer,
            }],
        ),
        (
            Who::Admin,
            vec![Grant {
                project: Some(app),
                role: Role::Admin,
            }],
        ),
        (
            Who::GlobalAdmin,
            vec![Grant {
                project: None,
                role: Role::Admin,
            }],
        ),
    ];
    let mut out = HashMap::new();
    for (who, grants) in grants {
        let user = rig
            .store
            .upsert_user("https://idp.test", &format!("matrix-{who:?}"), None, None)
            .await
            .unwrap();
        let data = json!({"grants": grants, "csrf": CSRF, "email": null, "name": null});
        let cookie = rig
            .store
            .create_session(
                Some(user),
                &data,
                OffsetDateTime::now_utc() + Duration::hours(1),
            )
            .await
            .unwrap();
        out.insert(who, format!("{}={}", session::COOKIE, cookie.expose()));
    }
    out
}

/// A request as `who` makes it. Publisher keys go as bearer tokens; sessions as the cookie.
/// One request as `who`. `csrf` says whether a signed-in request echoes the session's CSRF token.
fn request(
    rig: &Rig,
    cookies: &HashMap<Who, String>,
    who: Who,
    method: &str,
    path: &str,
    csrf: bool,
) -> TestRequest {
    let method: Method = method
        .to_ascii_uppercase()
        .parse()
        .unwrap_or_else(|_| panic!("the matrix knows no method for {path}"));
    let req = rig.app.method(method.clone(), path);
    // The only PUT route is triage, which takes this body (a known failure needs its issue).
    let req = if method == Method::PUT {
        req.json(&json!({ "state": "known", "issue_url": "https://example.test/issues/1" }))
    } else {
        req
    };
    match who {
        Who::Anonymous => req,
        Who::Publisher => req.authorization_bearer(&rig.key),
        signed_in => {
            let req = req.add_header("cookie", cookies[&signed_in].clone());
            if csrf {
                req.add_header(CSRF_HEADER, CSRF)
            } else {
                req
            }
        }
    }
}

/// What the route answered and what the class requires of `who`, as a message on mismatch.
fn check(class: Class, who: Who, status: u16, body: &Value, anonymous: u16) -> Result<(), String> {
    let signed_in = !matches!(who, Who::Anonymous | Who::Publisher);
    let want = |ok: bool, what: &str| {
        if ok {
            Ok(())
        } else {
            Err(format!("{who:?}: got {status} ({what})"))
        }
    };
    match class {
        Class::ProjectRead => match who {
            Who::Anonymous | Who::Publisher => want(status == 401, "401 expected"),
            Who::Stranger => want(
                status == 404 && body["error"] == "no such project",
                "404 expected",
            ),
            _ => want(status == 200, "200 expected"),
        },
        Class::FlowPage => match who {
            Who::Anonymous | Who::Publisher => want(status == 401, "401 expected"),
            Who::Stranger => want(
                status == 404 && body["error"] == "no such flow",
                "404 expected",
            ),
            _ => want(status == 303, "303 expected"),
        },
        Class::ProjectList => match who {
            Who::Anonymous | Who::Publisher => want(status == 401, "401 expected"),
            Who::Stranger => {
                let slugs: Vec<&str> = body
                    .as_array()
                    .map(|a| a.iter().filter_map(|p| p["slug"].as_str()).collect())
                    .unwrap_or_default();
                want(
                    status == 200 && slugs == ["other"],
                    "200 with only its own project",
                )
            }
            _ => want(status == 200, "200 expected"),
        },
        Class::WhoAmI => want(
            if signed_in {
                status == 200
            } else {
                status == 401
            },
            "200 when signed in, else 401",
        ),
        Class::Logout => match who {
            Who::Anonymous | Who::Publisher => want(status == 401, "401 expected"),
            // A signed-in request without the CSRF header is a forged-looking one: refused.
            _ => want(status == 403, "403 expected without the CSRF token"),
        },
        Class::ProjectTriage => match who {
            Who::Anonymous | Who::Publisher => want(status == 401, "401 expected"),
            Who::Stranger => want(
                status == 404 && body["error"] == "no such project",
                "404 expected",
            ),
            Who::Viewer => want(status == 403, "403 expected: a viewer may not triage"),
            // Past the role check the handler answers for the failure, and none has this one.
            _ => want(
                status == 404 && body["error"] == "no such failure",
                "404 for the unknown failure",
            ),
        },
        Class::Agent => match who {
            // Publisher keys are the only callers these routes take; their own answers are not
            // authorization decisions, so the matrix checks only what humans get.
            Who::Publisher => Ok(()),
            _ => want(
                (400..500).contains(&status) && status != 200,
                "a client error, never a success",
            ),
        },
        Class::Live => match who {
            Who::Anonymous | Who::Publisher => want(status == 401, "refused at upgrade"),
            _ => Ok(()),
        },
        Class::RoleIndependent => want(status == anonymous, "the same answer as anonymous"),
        Class::Admin => match who {
            Who::Anonymous | Who::Publisher => want(status == 401, "401 expected"),
            Who::GlobalAdmin => want(status == 200, "200 expected for a global admin"),
            _ => want(
                status == 403,
                "403 expected: only a global admin may read Screen G",
            ),
        },
    }
}

/// Whether the route is a WebSocket route, which needs an upgrade request to reach its handler.
fn is_socket(path: &str) -> bool {
    path == "/api/v1/live" || path == "/api/v1/agent/ws"
}

/// An upgrade request to a socket route, from the server's own origin (the cross-site check
/// refuses any other), as `who`.
fn socket(rig: &Rig, cookies: &HashMap<Who, String>, who: Who, path: &str) -> TestRequest {
    let origin = rig
        .app
        .server_address()
        .expect("the test server listens on a socket")
        .origin()
        .ascii_serialization();
    let req = rig.app.get_websocket(path).add_header("origin", origin);
    match who {
        Who::Anonymous => req,
        Who::Publisher => req.authorization_bearer(&rig.key),
        signed_in => req.add_header("cookie", cookies[&signed_in].clone()),
    }
}

/// The agent socket upgrades for anyone and authenticates in the handshake: without a publisher
/// credential it sends a `reject` and closes, so no human gets past the handshake.
async fn agent_socket_as(
    rig: &Rig,
    cookies: &HashMap<Who, String>,
    who: Who,
) -> Result<(), String> {
    if who == Who::Publisher {
        return Ok(());
    }
    let req = socket(rig, cookies, who, "/api/v1/agent/ws")
        .add_header("sec-websocket-protocol", WS_SUBPROTOCOL);
    let res = req.await;
    if (400..500).contains(&res.status_code().as_u16()) {
        return Ok(());
    }
    let mut agent = res.into_websocket().await;
    match agent.receive_message().await {
        WsMessage::Text(text) if text.contains("reject") => Ok(()),
        WsMessage::Close(_) => Ok(()),
        other => Err(format!(
            "{who:?}: the agent socket sent {other:?} before any credential"
        )),
    }
}

/// A live-socket walk: upgrade, then subscribe to the project. Returns what the socket said.
async fn live_as(rig: &Rig, cookies: &HashMap<Who, String>, who: Who) -> Result<(), String> {
    let res = socket(rig, cookies, who, "/api/v1/live").await;
    if matches!(who, Who::Anonymous | Who::Publisher) {
        return check(
            Class::Live,
            who,
            res.status_code().as_u16(),
            &Value::Null,
            0,
        );
    }
    let mut live = res.into_websocket().await;
    live.send_json(&json!({"op": "subscribe", "id": "first", "project": "app"}))
        .await;
    let frame: Value = live.receive_json().await;
    check_subscription(who, &frame, "first")?;
    // Each subscription on the socket is authorized on its own: a second one gets its own answer,
    // and a refusal does not close the socket that the first one is on.
    live.send_json(&json!({"op": "subscribe", "id": "second", "project": "app"}))
        .await;
    let frame: Value = live.receive_json().await;
    check_subscription(who, &frame, "second")
}

/// The answer one subscription of `who` must get: a confirmation for a principal with a role on
/// the project, and the answer for a missing project for a stranger.
fn check_subscription(who: Who, frame: &Value, key: &str) -> Result<(), String> {
    match who {
        Who::Stranger if frame["message"] == "no such project app" && frame["sub"] == key => Ok(()),
        Who::Stranger => Err(format!("Stranger: subscribe {key} answered {frame}")),
        _ if frame["op"] == "subscribed" && frame["sub"] == key => Ok(()),
        _ => Err(format!("{who:?}: subscribe {key} answered {frame}")),
    }
}

#[tokio::test]
async fn every_rest_and_live_route_answers_every_principal_as_its_class_allows() {
    let rig = rig().await;
    let ids = seed(&rig).await;
    let cookies = cookies(&rig).await;
    let mut failures = Vec::new();

    for (path, method) in derived_routes() {
        let class = class_of(&path).expect("every route has a row (every_route_has_a_row)");
        let target = concrete(&path, &ids);
        if path == "/api/v1/agent/ws" {
            for who in WHO {
                if let Err(e) = agent_socket_as(&rig, &cookies, who).await {
                    failures.push(format!("{method} {target}: {e}"));
                }
            }
            continue;
        }
        if class == Class::Live {
            for who in WHO {
                if let Err(e) = live_as(&rig, &cookies, who).await {
                    failures.push(format!("{method} {target}: {e}"));
                }
            }
            continue;
        }
        // Only triage is checked past its CSRF check; the logout row checks the refusal without it.
        let csrf = class == Class::ProjectTriage;
        // The anonymous answer is the baseline for role-independent routes.
        let anonymous = request(&rig, &cookies, Who::Anonymous, &method, &target, csrf)
            .await
            .status_code()
            .as_u16();
        for who in WHO {
            let res: TestResponse = if is_socket(&path) {
                socket(&rig, &cookies, who, &target).await
            } else {
                request(&rig, &cookies, who, &method, &target, csrf).await
            };
            let status = res.status_code().as_u16();
            let body: Value = serde_json::from_str(&res.text()).unwrap_or(Value::Null);
            if let Err(e) = check(class, who, status, &body, anonymous) {
                failures.push(format!("{method} {target} as {e}"));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "authorization matrix:\n{}",
        failures.join("\n")
    );
}

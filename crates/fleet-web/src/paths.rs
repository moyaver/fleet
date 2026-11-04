// SPDX-License-Identifier: AGPL-3.0-only
//! The dashboard's URLs: where pages live (PLAN §10.1), where the API and the live socket are,
//! and the sign-in redirect. Pure string work, so it is tested natively.

use uuid::Uuid;

/// The REST path prefix of the server.
pub const API: &str = "/api/v1";

/// A pipeline attempt's page: `/p/:project/pipelines/:id`.
#[must_use]
pub fn pipeline_href(project: &str, attempt: Uuid) -> String {
    format!("/p/{}/pipelines/{attempt}", escape(project))
}

/// A worker's page. REST reads are per project, so the slug travels as `?project=`.
#[must_use]
pub fn worker_href(project: &str, worker: Uuid) -> String {
    format!("/workers/{worker}?project={}", escape(project))
}

/// A device session's page (see [`worker_href`] for the query).
#[must_use]
pub fn device_href(project: &str, device: Uuid) -> String {
    format!("/devices/{device}?project={}", escape(project))
}

/// A flow execution's page (see [`worker_href`] for the query).
#[must_use]
pub fn flow_href(project: &str, execution: Uuid) -> String {
    format!("/flows/{execution}?project={}", escape(project))
}

/// The server's sign-in route, asking to come back to `return_to` (a path and query on this site).
#[must_use]
pub fn login_url(return_to: &str) -> String {
    format!("{API}/auth/login?return_to={}", escape(return_to))
}

/// The live socket's URL for a page served over `protocol` (`http:` or `https:`) from `host`.
#[must_use]
pub fn live_url(protocol: &str, host: &str) -> String {
    let scheme = if protocol == "https:" { "wss" } else { "ws" };
    format!("{scheme}://{host}{API}/live")
}

/// The CI provider's page for the run behind an attempt, when Fleet can tell where it is.
/// Only GitHub Actions on github.com is known: its `run_key` is `<repository>:<run_id>`.
#[must_use]
pub fn ci_run_url(provider: &str, repository: Option<&str>, run_key: &str) -> Option<String> {
    if provider != "github_actions" {
        return None;
    }
    let repository = repository?;
    let (_, run_id) = run_key.rsplit_once(':')?;
    let well_formed = !run_id.is_empty() && run_id.bytes().all(|b| b.is_ascii_digit());
    well_formed.then(|| format!("https://github.com/{repository}/actions/runs/{run_id}"))
}

/// Percent-encodes everything but the RFC 3986 unreserved characters, so the result is safe in
/// one path segment and in a query value.
pub(crate) fn escape(text: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            _ = write!(out, "%{b:02X}");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: Uuid = Uuid::from_u128(0x0190_0000_0000_7000_8000_0000_0000_0001);

    #[test]
    fn pages_live_where_plan_10_1_puts_them() {
        assert_eq!(
            pipeline_href("app", ID),
            "/p/app/pipelines/01900000-0000-7000-8000-000000000001"
        );
        assert_eq!(
            worker_href("app", ID),
            "/workers/01900000-0000-7000-8000-000000000001?project=app"
        );
        assert_eq!(
            device_href("app", ID),
            "/devices/01900000-0000-7000-8000-000000000001?project=app"
        );
        assert_eq!(
            flow_href("app", ID),
            "/flows/01900000-0000-7000-8000-000000000001?project=app"
        );
    }

    #[test]
    fn a_slug_is_escaped_in_paths_and_queries() {
        assert_eq!(
            pipeline_href("a b/c", ID),
            "/p/a%20b%2Fc/pipelines/01900000-0000-7000-8000-000000000001"
        );
        assert!(worker_href("a&b", ID).ends_with("?project=a%26b"));
    }

    #[test]
    fn sign_in_returns_to_the_current_page_with_its_query() {
        assert_eq!(
            login_url("/p/app/pipelines/1?tab=jobs"),
            "/api/v1/auth/login?return_to=%2Fp%2Fapp%2Fpipelines%2F1%3Ftab%3Djobs"
        );
        assert_eq!(login_url("/"), "/api/v1/auth/login?return_to=%2F");
    }

    #[test]
    fn the_live_socket_follows_the_page_scheme() {
        assert_eq!(
            live_url("http:", "127.0.0.1:8080"),
            "ws://127.0.0.1:8080/api/v1/live"
        );
        assert_eq!(
            live_url("https:", "fleet.example"),
            "wss://fleet.example/api/v1/live"
        );
    }

    #[test]
    fn only_github_actions_runs_get_a_link() {
        assert_eq!(
            ci_run_url("github_actions", Some("acme/app"), "acme/app:9001").as_deref(),
            Some("https://github.com/acme/app/actions/runs/9001")
        );
        assert_eq!(
            ci_run_url("generic", Some("loadgen/app"), "loadgen/app:7"),
            None
        );
        assert_eq!(ci_run_url("github_actions", None, "acme/app:9001"), None);
        assert_eq!(
            ci_run_url("github_actions", Some("acme/app"), "no-colon"),
            None
        );
        assert_eq!(
            ci_run_url("github_actions", Some("acme/app"), "acme/app:../x"),
            None,
            "a run id is digits only"
        );
    }
}

// SPDX-License-Identifier: AGPL-3.0-only
//! The REST client. Every body is a `fleet_protocol` type: the dashboard has no DTOs of its own
//! (PLAN §10.1). The session cookie travels with every request and the SPA holds no token.

use fleet_protocol::{
    live::{AttemptView, PlanView, ProjectView},
    rest::{
        ArtifactListView, DeviceDetailView, FlowDetailView, MetricsView, OverviewView, SessionView,
        TimelineView, WorkerRunPage, WorkerView,
        admin::{
            AdminAgentsView, AdminApiKeyView, AdminAuditPage, AdminIngestionView, AdminProjectView,
            AdminRetentionView, AdminRoleMappingsView, AdminSettingView, AdminTrustPolicyView,
        },
        history::{
            CompareView, FailureTriageRequest, FailureTriageView, FlowCatalogPage, FlowStatsView,
            HistoryPage, HistoryTrendView,
        },
    },
};
use gloo_net::http::Request;
use serde::de::DeserializeOwned;
use uuid::Uuid;

use crate::paths::API;

/// Why a request did not give data.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ApiError {
    /// No valid session or viewer token: the person has to sign in.
    #[error("sign-in required")]
    Unauthorized,
    /// The session's role does not allow it.
    #[error("your role does not allow this")]
    Forbidden,
    /// Too many requests for this credential. The server said when to try again; the dashboard
    /// waits that long and keeps the data it has, so this is not shown to the person.
    #[error("too many requests; try again in {retry_after_secs} s")]
    RateLimited {
        /// The server's `Retry-After`, in seconds.
        retry_after_secs: u64,
    },
    /// There is no such thing (or the caller may not know it exists).
    #[error("not found")]
    NotFound,
    /// Any other refusal or server failure.
    #[error("the server answered {status}: {message}")]
    Status {
        /// The HTTP status.
        status: u16,
        /// The server's `error` text, or the status's reason.
        message: String,
    },
    /// The request never got an answer.
    #[error("the server could not be reached: {0}")]
    Network(String),
    /// The answer was not the shape this build expects.
    #[error("unreadable response: {0}")]
    Decode(String),
}

/// The wait a `429` asks for when its `Retry-After` is missing or unreadable. The server always
/// sends one, and never less than a second.
const DEFAULT_RETRY_AFTER_SECS: u64 = 1;

/// The error for an HTTP answer, or `None` for a success. `retry_after` is the `Retry-After` header
/// in whole seconds.
#[must_use]
pub fn classify(status: u16, body: &str, retry_after: Option<u64>) -> Option<ApiError> {
    if (200..300).contains(&status) {
        return None;
    }
    Some(match status {
        401 => ApiError::Unauthorized,
        403 => ApiError::Forbidden,
        404 => ApiError::NotFound,
        429 => ApiError::RateLimited {
            retry_after_secs: retry_after.unwrap_or(DEFAULT_RETRY_AFTER_SECS),
        },
        _ => ApiError::Status {
            status,
            message: error_text(body).unwrap_or_else(|| format!("HTTP {status}")),
        },
    })
}

/// The `Retry-After` of a response in whole seconds, when it has one this build can read.
fn retry_after(headers: &gloo_net::http::Headers) -> Option<u64> {
    headers.get("retry-after")?.trim().parse().ok()
}

/// The `error` field of the server's JSON error body.
fn error_text(body: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Body {
        error: String,
    }
    serde_json::from_str::<Body>(body).ok().map(|b| b.error)
}

async fn get<T: DeserializeOwned>(path: &str) -> Result<T, ApiError> {
    let response = Request::get(path)
        .send()
        .await
        .map_err(|e| ApiError::Network(e.to_string()))?;
    let status = response.status();
    let retry = retry_after(&response.headers());
    let body = response
        .text()
        .await
        .map_err(|e| ApiError::Network(e.to_string()))?;
    if let Some(error) = classify(status, &body, retry) {
        return Err(error);
    }
    serde_json::from_str(&body).map_err(|e| ApiError::Decode(e.to_string()))
}

/// Every project the caller may read.
pub async fn projects() -> Result<Vec<ProjectView>, ApiError> {
    get(&format!("{API}/projects")).await
}

/// Screen A's data for one project.
pub async fn overview(project: &str) -> Result<OverviewView, ApiError> {
    get(&format!("{API}/projects/{}/overview", segment(project))).await
}

/// One attempt's Fleet plans, each shard with its actual time: the plan table's data.
pub async fn plans(project: &str, attempt: Uuid) -> Result<Vec<PlanView>, ApiError> {
    get(&format!(
        "{API}/projects/{}/attempts/{attempt}/plans",
        segment(project)
    ))
    .await
}

/// Screen B's timeline of one attempt.
pub async fn timeline(project: &str, attempt: Uuid) -> Result<TimelineView, ApiError> {
    get(&format!(
        "{API}/projects/{}/attempts/{attempt}/timeline",
        segment(project)
    ))
    .await
}

/// Screen C's worker: the machine, its latest runs and devices.
pub async fn worker(project: &str, worker: Uuid) -> Result<WorkerView, ApiError> {
    get(&format!(
        "{API}/projects/{}/workers/{worker}",
        segment(project)
    ))
    .await
}

/// A page of a worker's runs, newest first; `cursor` is the previous page's `next_cursor`.
pub async fn worker_runs(
    project: &str,
    worker: Uuid,
    cursor: Option<&str>,
) -> Result<WorkerRunPage, ApiError> {
    let mut path = format!("{API}/projects/{}/workers/{worker}/runs", segment(project));
    if let Some(cursor) = cursor {
        path.push_str("?cursor=");
        path.push_str(&crate::paths::escape(cursor));
    }
    get(&path).await
}

/// A worker's metric series; `query` is [`MetricsQuery::to_query_string`](crate::screens::metrics::MetricsQuery::to_query_string).
pub async fn worker_metrics(
    project: &str,
    worker: Uuid,
    query: &str,
) -> Result<MetricsView, ApiError> {
    get(&format!(
        "{API}/projects/{}/workers/{worker}/metrics?{query}",
        segment(project)
    ))
    .await
}

/// Screen C's device session and the flows that ran on it.
pub async fn device(project: &str, device: Uuid) -> Result<DeviceDetailView, ApiError> {
    get(&format!(
        "{API}/projects/{}/devices/{device}",
        segment(project)
    ))
    .await
}

/// A device session's metric series (see [`worker_metrics`] for `query`).
pub async fn device_metrics(
    project: &str,
    device: Uuid,
    query: &str,
) -> Result<MetricsView, ApiError> {
    get(&format!(
        "{API}/projects/{}/devices/{device}/metrics?{query}",
        segment(project)
    ))
    .await
}

/// Screen D's flow execution with its claims, steps and failure.
pub async fn flow(project: &str, execution: Uuid) -> Result<FlowDetailView, ApiError> {
    get(&format!(
        "{API}/projects/{}/flows/{execution}",
        segment(project)
    ))
    .await
}

/// A flow's artifacts, each with a short-lived signed link on the usercontent origin.
pub async fn flow_artifacts(project: &str, execution: Uuid) -> Result<ArtifactListView, ApiError> {
    get(&format!(
        "{API}/projects/{}/flows/{execution}/artifacts",
        segment(project)
    ))
    .await
}

/// A project's latest pipeline attempts, newest first (the compare page's pickers).
pub async fn attempts(project: &str, limit: u32) -> Result<Vec<AttemptView>, ApiError> {
    get(&format!(
        "{API}/projects/{}/attempts?limit={limit}",
        segment(project)
    ))
    .await
}

/// Reads a text artifact (the Maestro log, `commands.json`) from the usercontent origin through
/// its signed link. The request is cross-origin and carries no cookie: the link is the credential,
/// and the dashboard's CSP (`connect-src`) allows exactly that one origin.
pub async fn fetch_text(link: &str) -> Result<String, ApiError> {
    let response = Request::get(link)
        .send()
        .await
        .map_err(|e| ApiError::Network(e.to_string()))?;
    let status = response.status();
    let retry = retry_after(&response.headers());
    let body = response
        .text()
        .await
        .map_err(|e| ApiError::Network(e.to_string()))?;
    match status {
        410 => Err(ApiError::Status {
            status,
            message: "the link has expired".to_owned(),
        }),
        _ => classify(status, &body, retry).map_or(Ok(body), Err),
    }
}

/// Screen E: a page of the project's history; `query` is the history query string without `?`
/// (see [`crate::screens::history::model::HistoryQuery::rest_query`]).
pub async fn history(project: &str, query: &str) -> Result<HistoryPage, ApiError> {
    get(&format!(
        "{API}/projects/{}/history?{query}",
        segment(project)
    ))
    .await
}

/// Screen E's flow catalog: one page of the flows the project knows, with the filters in `query`.
pub async fn catalog(project: &str, query: &str) -> Result<FlowCatalogPage, ApiError> {
    get(&format!(
        "{API}/projects/{}/flows?{query}",
        segment(project)
    ))
    .await
}

/// Screen E's trend over the filtered runs; `query` holds the filters and the window.
pub async fn trend(project: &str, query: &str) -> Result<HistoryTrendView, ApiError> {
    get(&format!(
        "{API}/projects/{}/history/trend?{query}",
        segment(project)
    ))
    .await
}

/// Screen E's panel for the flow `flow` (its identity as the history lists it); `query` holds the
/// optional `from` and `to`.
pub async fn flow_stats(project: &str, flow: &str, query: &str) -> Result<FlowStatsView, ApiError> {
    get(&format!(
        "{API}/projects/{}/flows/{}/stats?{query}",
        segment(project),
        crate::paths::escape(flow)
    ))
    .await
}

/// Screen F: attempt `a` against attempt `b`.
pub async fn compare(project: &str, a: Uuid, b: Uuid) -> Result<CompareView, ApiError> {
    get(&format!(
        "{API}/projects/{}/compare?a={a}&b={b}",
        segment(project)
    ))
    .await
}

/// Who is signed in; [`ApiError::Unauthorized`] when nobody is.
pub async fn session() -> Result<SessionView, ApiError> {
    get(&format!("{API}/auth/session")).await
}

/// Ends the session. `csrf_token` comes from [`session`].
pub async fn logout(csrf_token: &str) -> Result<(), ApiError> {
    let response = Request::post(&format!("{API}/auth/logout"))
        .header("X-CSRF-Token", csrf_token)
        .send()
        .await
        .map_err(|e| ApiError::Network(e.to_string()))?;
    let retry = retry_after(&response.headers());
    let body = response.text().await.unwrap_or_default();
    classify(response.status(), &body, retry).map_or(Ok(()), Err)
}

/// Marks the failure `fingerprint` known or resolved (issue #84). `csrf_token` comes from
/// [`session`]; the server refuses a person whose role is below maintainer.
pub async fn set_triage(
    project: &str,
    fingerprint: &str,
    body: &FailureTriageRequest,
    csrf_token: &str,
) -> Result<FailureTriageView, ApiError> {
    let request = Request::put(&triage_path(project, fingerprint))
        .header("X-CSRF-Token", csrf_token)
        .json(body)
        .map_err(|e| ApiError::Decode(e.to_string()))?;
    answer(request).await
}

/// Clears the failure `fingerprint` back to untriaged (issue #84). The same rules as
/// [`set_triage`] apply.
pub async fn clear_triage(
    project: &str,
    fingerprint: &str,
    csrf_token: &str,
) -> Result<FailureTriageView, ApiError> {
    let request = Request::delete(&triage_path(project, fingerprint))
        .header("X-CSRF-Token", csrf_token)
        .build()
        .map_err(|e| ApiError::Decode(e.to_string()))?;
    answer(request).await
}

/// Sends a mutating request and reads its body the way [`get`] does.
async fn answer<T: DeserializeOwned>(request: Request) -> Result<T, ApiError> {
    let response = request
        .send()
        .await
        .map_err(|e| ApiError::Network(e.to_string()))?;
    let status = response.status();
    let retry = retry_after(&response.headers());
    let body = response
        .text()
        .await
        .map_err(|e| ApiError::Network(e.to_string()))?;
    if let Some(error) = classify(status, &body, retry) {
        return Err(error);
    }
    serde_json::from_str(&body).map_err(|e| ApiError::Decode(e.to_string()))
}

fn triage_path(project: &str, fingerprint: &str) -> String {
    format!(
        "{API}/projects/{}/failures/{}/triage",
        segment(project),
        crate::paths::escape(fingerprint)
    )
}

/// A project slug as one path segment.
fn segment(slug: &str) -> String {
    crate::paths::escape(slug)
}

/// Screen G: the projects, for onboarding. Global administrators only.
pub async fn admin_projects() -> Result<Vec<AdminProjectView>, ApiError> {
    get(&format!("{API}/admin/projects")).await
}

/// Screen G: publisher trust policies, by project.
pub async fn admin_trust_policies() -> Result<Vec<AdminTrustPolicyView>, ApiError> {
    get(&format!("{API}/admin/trust-policies")).await
}

/// Screen G: API keys, metadata only.
pub async fn admin_api_keys() -> Result<Vec<AdminApiKeyView>, ApiError> {
    get(&format!("{API}/admin/api-keys")).await
}

/// Screen G: roles bound to people and the identity-provider groups that grant them.
pub async fn admin_role_mappings() -> Result<AdminRoleMappingsView, ApiError> {
    get(&format!("{API}/admin/role-mappings")).await
}

/// Screen G: retention classes per project, and the server's retention windows.
pub async fn admin_retention() -> Result<AdminRetentionView, ApiError> {
    get(&format!("{API}/admin/retention")).await
}

/// Screen G: the artifact policy, from the running configuration.
pub async fn admin_artifact_policy() -> Result<Vec<AdminSettingView>, ApiError> {
    get(&format!("{API}/admin/artifact-policy")).await
}

/// Screen G: the metric collection policy, from the running configuration.
pub async fn admin_metric_policy() -> Result<Vec<AdminSettingView>, ApiError> {
    get(&format!("{API}/admin/metric-policy")).await
}

/// Screen G: agent versions seen and this server's protocol.
pub async fn admin_agents() -> Result<AdminAgentsView, ApiError> {
    get(&format!("{API}/admin/agents")).await
}

/// Screen G: refusals, losses and open runs.
pub async fn admin_ingestion() -> Result<AdminIngestionView, ApiError> {
    get(&format!("{API}/admin/ingestion")).await
}

/// The audit history, newest first: one page of up to `limit` events after `cursor`.
pub async fn admin_audit(cursor: Option<&str>, limit: u32) -> Result<AdminAuditPage, ApiError> {
    let mut path = format!("{API}/admin/audit?limit={limit}");
    if let Some(cursor) = cursor {
        path.push_str("&cursor=");
        path.push_str(&crate::paths::escape(cursor));
    }
    get(&path).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_statuses_are_not_errors() {
        for status in [200, 201, 204] {
            assert_eq!(classify(status, "", None), None, "{status}");
        }
    }

    #[test]
    fn a_429_carries_the_servers_retry_after() {
        assert_eq!(
            classify(429, r#"{"error":"too many requests"}"#, Some(7)),
            Some(ApiError::RateLimited {
                retry_after_secs: 7
            })
        );
        assert_eq!(
            classify(429, "", None),
            Some(ApiError::RateLimited {
                retry_after_secs: 1
            }),
            "a 429 without a readable Retry-After still waits a second, never zero"
        );
    }

    #[test]
    fn the_statuses_the_dashboard_reacts_to_have_their_own_error() {
        assert_eq!(
            classify(401, r#"{"error":"sign in"}"#, None),
            Some(ApiError::Unauthorized)
        );
        assert_eq!(classify(403, "{}", None), Some(ApiError::Forbidden));
        assert_eq!(
            classify(404, r#"{"error":"no such project"}"#, None),
            Some(ApiError::NotFound)
        );
    }

    #[test]
    fn other_failures_carry_the_servers_message_or_the_status() {
        assert_eq!(
            classify(500, r#"{"error":"internal error"}"#, None),
            Some(ApiError::Status {
                status: 500,
                message: "internal error".into()
            })
        );
        assert_eq!(
            classify(502, "<html>Bad Gateway</html>", None),
            Some(ApiError::Status {
                status: 502,
                message: "HTTP 502".into()
            }),
            "a proxy's HTML page is not shown as a message"
        );
    }
}

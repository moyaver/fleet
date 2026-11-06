// SPDX-License-Identifier: AGPL-3.0-only
//! The REST client. Every body is a `fleet_protocol` type: the dashboard has no DTOs of its own
//! (PLAN §10.1). The session cookie travels with every request and the SPA holds no token.

use fleet_protocol::{
    live::{AttemptView, PlanView, ProjectView},
    rest::{
        ArtifactListView, DeviceDetailView, FlowDetailView, MetricsView, OverviewView, SessionView,
        TimelineView, WorkerRunPage, WorkerView,
        history::{CompareView, FlowStatsView, HistoryPage},
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

/// The error for an HTTP answer, or `None` for a success.
#[must_use]
pub fn classify(status: u16, body: &str) -> Option<ApiError> {
    if (200..300).contains(&status) {
        return None;
    }
    Some(match status {
        401 => ApiError::Unauthorized,
        403 => ApiError::Forbidden,
        404 => ApiError::NotFound,
        _ => ApiError::Status {
            status,
            message: error_text(body).unwrap_or_else(|| format!("HTTP {status}")),
        },
    })
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
    let body = response
        .text()
        .await
        .map_err(|e| ApiError::Network(e.to_string()))?;
    if let Some(error) = classify(status, &body) {
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
    let body = response
        .text()
        .await
        .map_err(|e| ApiError::Network(e.to_string()))?;
    match status {
        410 => Err(ApiError::Status {
            status,
            message: "the link has expired".to_owned(),
        }),
        _ => classify(status, &body).map_or(Ok(body), Err),
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
    let body = response.text().await.unwrap_or_default();
    classify(response.status(), &body).map_or(Ok(()), Err)
}

/// A project slug as one path segment.
fn segment(slug: &str) -> String {
    crate::paths::escape(slug)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_statuses_are_not_errors() {
        for status in [200, 201, 204] {
            assert_eq!(classify(status, ""), None, "{status}");
        }
    }

    #[test]
    fn the_statuses_the_dashboard_reacts_to_have_their_own_error() {
        assert_eq!(
            classify(401, r#"{"error":"sign in"}"#),
            Some(ApiError::Unauthorized)
        );
        assert_eq!(classify(403, "{}"), Some(ApiError::Forbidden));
        assert_eq!(
            classify(404, r#"{"error":"no such project"}"#),
            Some(ApiError::NotFound)
        );
    }

    #[test]
    fn other_failures_carry_the_servers_message_or_the_status() {
        assert_eq!(
            classify(500, r#"{"error":"internal error"}"#),
            Some(ApiError::Status {
                status: 500,
                message: "internal error".into()
            })
        );
        assert_eq!(
            classify(502, "<html>Bad Gateway</html>"),
            Some(ApiError::Status {
                status: 502,
                message: "HTTP 502".into()
            }),
            "a proxy's HTML page is not shown as a message"
        );
    }
}

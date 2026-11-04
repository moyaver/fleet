// SPDX-License-Identifier: AGPL-3.0-only
//! The REST client. Every body is a `fleet_protocol` type: the dashboard has no DTOs of its own
//! (PLAN §10.1). The session cookie travels with every request and the SPA holds no token.

use fleet_protocol::{
    live::ProjectView,
    rest::{OverviewView, SessionView, TimelineView},
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

/// Screen B's timeline of one attempt.
pub async fn timeline(project: &str, attempt: Uuid) -> Result<TimelineView, ApiError> {
    get(&format!(
        "{API}/projects/{}/attempts/{attempt}/timeline",
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

// SPDX-License-Identifier: AGPL-3.0-only
//! The artifact upload protocol (PLAN §6.6, §7.6; research 05 §12.1): what an agent and the
//! server say to each other over HTTPS. Artifact bytes never travel over the WebSocket.
//!
//! ```text
//! POST /api/v1/agent/artifacts                  UploadRequest  → UploadGrant
//! PUT  /api/v1/agent/artifacts/{id}/content     the bytes      → 204
//! POST /api/v1/agent/artifacts/{id}/complete                   → UploadResult
//! ```
//!
//! Every call carries the agent's bearer credential. A grant belongs to one artifact of one agent
//! run, expires, and its bytes are accepted once. The agent reports the result in its own journal
//! (`artifact.uploaded` or `artifact.rejected`, see [`crate::event`]); the server's artifact
//! table is the record the dashboard reads.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

/// The kinds of file the server accepts: an allowlist, never "everything in the output
/// directory" (spec §7). A request for any other class is [`RejectReason::ClassNotAllowed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum ArtifactClass {
    /// A screenshot Maestro took at a failing step (PNG, JPEG or WebP).
    FailureScreenshot,
    /// A flow's `commands.json`, redacted by the agent (text).
    Commands,
    /// The JUnit XML report.
    Junit,
    /// Maestro's session `maestro.log`, redacted by the agent (text).
    MaestroLog,
    /// Maestro's HTML report: one HTML file, or a zip of the report directory.
    HtmlReport,
}

impl ArtifactClass {
    /// Every class the server accepts.
    pub const ALL: [Self; 5] = [
        Self::FailureScreenshot,
        Self::Commands,
        Self::Junit,
        Self::MaestroLog,
        Self::HtmlReport,
    ];

    /// The wire name, for example `failure_screenshot`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FailureScreenshot => "failure_screenshot",
            Self::Commands => "commands",
            Self::Junit => "junit",
            Self::MaestroLog => "maestro_log",
            Self::HtmlReport => "html_report",
        }
    }

    /// The class named by `name`, or `None` for anything off the allowlist.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.as_str() == name)
    }

    /// The `Content-Type`s an uploader may declare for this class.
    #[must_use]
    pub fn content_types(self) -> &'static [&'static str] {
        match self {
            Self::FailureScreenshot => &["image/png", "image/jpeg", "image/webp"],
            Self::Commands => &["application/json", "text/plain"],
            Self::Junit => &["application/xml", "text/xml"],
            Self::MaestroLog => &["text/plain"],
            Self::HtmlReport => &["text/html", "application/zip"],
        }
    }
}

/// Why an artifact was refused, with a stable code (`snake_case`) an agent can journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum RejectReason {
    /// The class isn't on the allowlist.
    ClassNotAllowed,
    /// The declared `Content-Type` isn't one this class accepts.
    ContentTypeNotAllowed,
    /// Bigger than the class's size limit.
    TooLarge,
    /// The run already uploaded as many bytes or files as one run may.
    RunQuotaExceeded,
    /// The bytes sent are not the declared size.
    SizeMismatch,
    /// The bytes' SHA-256 is not the declared one.
    DigestMismatch,
    /// The content (magic bytes) isn't a type this class accepts.
    TypeNotAllowed,
    /// A text artifact that isn't UTF-8 without NUL bytes.
    NotText,
    /// An archive that is a bomb, escapes its directory, or holds links or nested archives.
    UnsafeArchive,
}

impl RejectReason {
    /// The wire code.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ClassNotAllowed => "class_not_allowed",
            Self::ContentTypeNotAllowed => "content_type_not_allowed",
            Self::TooLarge => "too_large",
            Self::RunQuotaExceeded => "run_quota_exceeded",
            Self::SizeMismatch => "size_mismatch",
            Self::DigestMismatch => "digest_mismatch",
            Self::TypeNotAllowed => "type_not_allowed",
            Self::NotText => "not_text",
            Self::UnsafeArchive => "unsafe_archive",
        }
    }

    /// The reason with this wire code, if this build knows it.
    #[must_use]
    pub fn parse(code: &str) -> Option<Self> {
        [
            Self::ClassNotAllowed,
            Self::ContentTypeNotAllowed,
            Self::TooLarge,
            Self::RunQuotaExceeded,
            Self::SizeMismatch,
            Self::DigestMismatch,
            Self::TypeNotAllowed,
            Self::NotText,
            Self::UnsafeArchive,
        ]
        .into_iter()
        .find(|r| r.as_str() == code)
    }
}

/// `POST /api/v1/agent/artifacts`: "I have this file; may I upload it?"
///
/// Idempotent on `(agent_run_id, class, sha256)`: asking again returns the same artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct UploadRequest {
    /// The agent run that produced the file. It must belong to the caller's project.
    pub agent_run_id: Uuid,
    /// The flow execution it is evidence for, if any. It must belong to the same run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flow_execution_id: Option<Uuid>,
    /// The class, one of [`ArtifactClass::as_str`]. A string, so an older server answers an
    /// unknown class with [`RejectReason::ClassNotAllowed`] instead of failing to parse.
    pub class: String,
    /// The path relative to the run's output directory, redacted. A label for people; it never
    /// names where the server stores the bytes.
    pub path: String,
    /// The declared `Content-Type`; see [`ArtifactClass::content_types`]. The server serves what
    /// the bytes are, not what is declared.
    pub content_type: String,
    /// Size in bytes.
    pub size: u64,
    /// SHA-256 of the content: 64 lowercase hex digits.
    pub sha256: String,
}

/// The server's answer to an [`UploadRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(tag = "disposition", rename_all = "snake_case")]
pub enum UploadGrant {
    /// Send the bytes: `PUT` them to `url` (relative to the server's address) with the same
    /// bearer credential, then call [`UploadGrant::Upload::complete_url`]. The grant is
    /// short-lived and accepts the bytes once.
    Upload {
        /// The artifact being uploaded.
        artifact_id: Uuid,
        /// Where to `PUT` the bytes.
        url: String,
        /// Where to `POST` once the bytes are sent.
        complete_url: String,
        /// The grant is void after this.
        #[serde(with = "time::serde::rfc3339")]
        expires_at: OffsetDateTime,
        /// The bytes to send, exactly.
        size: u64,
    },
    /// The project already has these bytes for this class: nothing to send.
    Exists {
        /// The artifact.
        artifact_id: Uuid,
    },
    /// Refused; sending the bytes would not help.
    Rejected {
        /// Why.
        reason: RejectReason,
        /// A sentence for the agent's log.
        message: String,
    },
}

/// The final state of an upload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum UploadResult {
    /// Verified and stored.
    Available {
        /// The artifact.
        artifact_id: Uuid,
    },
    /// Verification refused the bytes; nothing was kept.
    Rejected {
        /// The artifact row that records the refusal.
        artifact_id: Uuid,
        /// Why.
        reason: RejectReason,
        /// A sentence for the agent's log.
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_and_reason_codes_round_trip_and_unknown_ones_are_refused() {
        for c in ArtifactClass::ALL {
            assert_eq!(ArtifactClass::parse(c.as_str()), Some(c));
            assert_eq!(
                serde_json::to_string(&c).unwrap(),
                format!("\"{}\"", c.as_str())
            );
        }
        assert_eq!(ArtifactClass::parse("video"), None);
        assert_eq!(
            RejectReason::parse("unsafe_archive"),
            Some(RejectReason::UnsafeArchive)
        );
        assert_eq!(RejectReason::parse("nope"), None);
    }

    #[test]
    fn a_grant_is_tagged_by_disposition() {
        let g: UploadGrant = serde_json::from_str(
            r#"{"disposition":"rejected","reason":"too_large","message":"too big"}"#,
        )
        .unwrap();
        assert_eq!(
            g,
            UploadGrant::Rejected {
                reason: RejectReason::TooLarge,
                message: "too big".into()
            }
        );
    }
}

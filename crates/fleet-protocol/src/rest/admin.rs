// SPDX-License-Identifier: AGPL-3.0-only
//! Screen G, administration (spec §8 "Screen G — Administration"; PLAN §10.3, M6): the read models
//! of the installation-wide pages. Every value comes from a table or from the running server
//! configuration, and each area says which. No secret is ever in these types: an API key is shown
//! by its ID, name, prefix and dates, never by its secret or its hash (invariant I6).
//!
//! The routes are `GET /api/v1/admin/…`, admin only (`fleet_core::policy::Action::ViewAdministration`,
//! and `ViewAudit` for the audit history).

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

/// A project, for onboarding: its identity and what is attached to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct AdminProjectView {
    /// The project's ID.
    pub id: Uuid,
    /// URL-safe identifier, as the API and the dashboard name it.
    pub slug: String,
    /// Display name.
    pub name: String,
    /// The branch treated as the default.
    pub default_branch: String,
    /// When it was created.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// When it was archived, if it was.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub archived_at: Option<OffsetDateTime>,
    /// API keys that still work (not revoked, not expired).
    pub active_api_keys: i64,
    /// Publisher trust policies that are not revoked.
    pub active_trust_policies: i64,
}

/// A publisher trust policy: who may publish into a project with a CI OIDC token (ADR-0008).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct AdminTrustPolicyView {
    /// The policy's ID.
    pub id: Uuid,
    /// The project it belongs to (slug).
    pub project: String,
    /// Its name, unique within the project.
    pub name: String,
    /// The token issuer it accepts.
    pub issuer: String,
    /// The audience the token must carry.
    pub audience: String,
    /// The immutable numeric repository ID it matches.
    pub repository_id: String,
    /// The immutable numeric owner ID it matches.
    pub repository_owner_id: String,
    /// Refs whose pushes count as trusted (globs). Empty: none.
    pub trusted_refs: Vec<String>,
    /// Events accepted. Empty: the default set.
    pub allowed_events: Vec<String>,
    /// Only tokens minted inside this reusable workflow, if set.
    pub job_workflow_ref: Option<String>,
    /// Only jobs in this environment, if set.
    pub environment: Option<String>,
    /// `restricted`, `full` or `deny`.
    pub pr_policy: String,
    /// When it was created.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// When it was revoked, if it was.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub revoked_at: Option<OffsetDateTime>,
}

/// An API key as an operator sees it: what identifies it, never the secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct AdminApiKeyView {
    /// The key's ID, the handle `fleet api-key revoke|expire` takes.
    pub id: Uuid,
    /// The project it publishes into (slug).
    pub project: String,
    /// The name it was created with.
    pub name: String,
    /// The first characters of the secret, to tell keys apart.
    pub prefix: String,
    /// The trust level its runs get.
    pub trust_level: String,
    /// Who created it, by display name or email; `None` when not recorded.
    pub created_by: Option<String>,
    /// When it was created.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// When it stops working; `None` for a key made before expiry existed.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub expires_at: Option<OffsetDateTime>,
    /// When it was revoked, if it was.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub revoked_at: Option<OffsetDateTime>,
    /// When it last authenticated a request.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub last_used_at: Option<OffsetDateTime>,
}

/// A person's role on one project, or on every project (`project` is `None`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct AdminRoleBindingView {
    /// The binding's ID.
    pub id: Uuid,
    /// The project (slug), or `None` for every project.
    pub project: Option<String>,
    /// The person, by display name, else email, else subject.
    pub user: String,
    /// `viewer`, `maintainer` or `admin`.
    pub role: String,
    /// When it was granted.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// An identity-provider group and the role its members get (configured per project).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct AdminGroupMappingView {
    /// The mapping's ID.
    pub id: Uuid,
    /// The identity provider's issuer.
    pub issuer: String,
    /// The claim value (a group name, an object ID or an app role).
    pub group: String,
    /// The project (slug), or `None` for every project.
    pub project: Option<String>,
    /// `viewer`, `maintainer` or `admin`.
    pub role: String,
}

/// Role mappings: the roles bound to people, and the identity-provider groups that grant them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct AdminRoleMappingsView {
    /// Roles bound to people directly.
    pub bindings: Vec<AdminRoleBindingView>,
    /// Group → role mappings (synced from `login.group_roles` at start-up).
    pub groups: Vec<AdminGroupMappingView>,
}

/// One retention class of one project (`retention_classes`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct AdminRetentionClassView {
    /// The project (slug).
    pub project: String,
    /// The artifact class it applies to (`report`, `failure_evidence`, …).
    pub class: String,
    /// How long an artifact of this class is kept, in seconds; `None` keeps it for ever.
    pub ttl_seconds: Option<i64>,
}

/// One setting read from the running server configuration (or a fixed fact about the agents),
/// shown read-only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct AdminSettingView {
    /// The configuration key (`artifacts.max_run_bytes`) or a name for a fact.
    pub key: String,
    /// The value, as text.
    pub value: String,
    /// Where it comes from: `config` (the running configuration), or `agent` (built into
    /// `fleet-agent`, not a server setting).
    pub source: String,
}

/// The retention page: each project's classes, and the server's retention windows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct AdminRetentionView {
    /// Per project, per artifact class (`retention_classes`).
    pub classes: Vec<AdminRetentionClassView>,
    /// How long telemetry, the journal and the audit log are kept (`retention.*`, config).
    pub windows: Vec<AdminSettingView>,
}

/// One agent version as the server has seen it: the version and protocol it spoke, and how often.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct AdminAgentRow {
    /// The agent's version, as its `hello` named it.
    pub agent_version: String,
    /// The protocol version the run spoke.
    pub protocol_version: i32,
    /// Runs seen with this version and protocol.
    pub runs: i64,
    /// Of those, runs still open (active, detached or finalizing).
    pub open_runs: i64,
    /// The newest run's start.
    #[serde(with = "time::serde::rfc3339")]
    pub last_started_at: OffsetDateTime,
    /// Whether this server speaks the protocol the runs spoke.
    pub compatible: bool,
}

/// Agent compatibility: the server's own versions, and the agent versions seen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct AdminAgentsView {
    /// This server's version.
    pub server_version: String,
    /// The protocol version this server speaks.
    pub protocol_version: i32,
    /// Agent versions seen, newest first.
    pub agents: Vec<AdminAgentRow>,
}

/// A refused event (`ingest_rejections`): the cursor moved past it and it is not in the journal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct AdminRejectionRow {
    /// The project (slug).
    pub project: String,
    /// The agent run it came from.
    pub agent_run_id: Uuid,
    /// The agent's version.
    pub agent_version: String,
    /// The event's sequence number in its run.
    pub seq: i64,
    /// Why it was refused (a short code).
    pub code: String,
    /// The refusal's detail, when there is one.
    pub message: Option<String>,
    /// When it was refused.
    #[serde(with = "time::serde::rfc3339")]
    pub rejected_at: OffsetDateTime,
}

/// A loss the agent reported (`ingest_gaps`): events or telemetry that never arrived.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct AdminGapRow {
    /// The project (slug).
    pub project: String,
    /// The agent run it belongs to.
    pub agent_run_id: Uuid,
    /// `journal` or `telemetry`.
    pub stream: String,
    /// The first missing sequence number.
    pub from_seq: i64,
    /// The last missing sequence number.
    pub to_seq: i64,
    /// Why the agent could not send it.
    pub reason: String,
    /// When the server recorded it.
    #[serde(with = "time::serde::rfc3339")]
    pub recorded_at: OffsetDateTime,
}

/// An agent run that is still open, and how far behind the server is with it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct AdminLagRow {
    /// The project (slug).
    pub project: String,
    /// The agent run.
    pub agent_run_id: Uuid,
    /// The agent's version.
    pub agent_version: String,
    /// `active`, `detached` or `finalizing`.
    pub state: String,
    /// When the run started.
    #[serde(with = "time::serde::rfc3339")]
    pub started_at: OffsetDateTime,
    /// When the server last had a frame from it.
    #[serde(with = "time::serde::rfc3339")]
    pub last_frame_at: OffsetDateTime,
    /// Seconds since that frame, measured by the server.
    pub seconds_since_frame: i64,
    /// The newest committed journal sequence number.
    pub committed_seq: i64,
    /// How long the newest committed event took from the agent's clock to the server's commit,
    /// in milliseconds. `None` when the run has no committed event yet.
    pub newest_event_lag_ms: Option<i64>,
}

/// Event ingestion health: refusals, losses and the lag of open runs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct AdminIngestionView {
    /// The newest refusals, newest first.
    pub rejections: Vec<AdminRejectionRow>,
    /// The newest losses the agents reported, newest first.
    pub gaps: Vec<AdminGapRow>,
    /// The open runs, the ones that have been silent longest first.
    pub open_runs: Vec<AdminLagRow>,
}

/// One entry of the audit history (`audit_events`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct AdminAuditEventView {
    /// The event's ID.
    pub id: Uuid,
    /// When it happened.
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
    /// Who did it: `user`, `publisher` or `system`.
    pub actor_kind: String,
    /// The actor's ID, when it has one.
    pub actor_id: Option<Uuid>,
    /// The actor by name: a person's display name or email, or an API key's name.
    pub actor: Option<String>,
    /// What was done (`failure.triage`, `ingest.foreign_entity`, …).
    pub action: String,
    /// The project it concerns (slug), if any.
    pub project: Option<String>,
    /// The kind of thing acted on, if recorded.
    pub target_kind: Option<String>,
    /// The ID of the thing acted on, if recorded.
    pub target_id: Option<Uuid>,
    /// The value before the change, where the event recorded one (`before`, or the `from` of a
    /// triage change).
    pub before: Option<serde_json::Value>,
    /// The value after the change, where the event recorded one (`after`, or the `to`).
    pub after: Option<serde_json::Value>,
    /// Everything the event recorded, as stored.
    pub details: serde_json::Value,
}

/// A page of the audit history, newest first.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct AdminAuditPage {
    /// The events of this page.
    pub items: Vec<AdminAuditEventView>,
    /// Pass as `cursor` for the next page; `None` when this is the last.
    pub next_cursor: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_setting_has_its_key_value_and_source_as_text() {
        let s = AdminSettingView {
            key: "artifacts.max_run_files".into(),
            value: "2000".into(),
            source: "config".into(),
        };
        let json = serde_json::to_value(&s).unwrap();
        assert_eq!(json["key"], "artifacts.max_run_files");
        assert_eq!(json["source"], "config");
    }

    #[test]
    fn an_api_key_view_has_no_field_for_a_secret_or_its_hash() {
        let key = AdminApiKeyView {
            id: Uuid::nil(),
            project: "app".into(),
            name: "ci".into(),
            prefix: "mf_live_abcd".into(),
            trust_level: "trusted".into(),
            created_by: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
            expires_at: None,
            revoked_at: None,
            last_used_at: None,
        };
        let json = serde_json::to_value(&key).unwrap();
        let keys: Vec<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert!(
            !keys.iter().any(|k| k.contains("hash") || *k == "secret"),
            "{keys:?}"
        );
    }
}

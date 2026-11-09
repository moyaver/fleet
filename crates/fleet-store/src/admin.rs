// SPDX-License-Identifier: AGPL-3.0-only
//! Screen G, administration: the read queries behind the installation-wide pages (spec §8; PLAN
//! §10.3, M6). Each area reads the table that holds it. The areas that come from the running
//! configuration (artifact policy, metric collection, retention windows) have no table, so the
//! server builds those from its configuration; this module reads only what is stored.
//!
//! The writes (add, change, revoke, remove, each audited in its own transaction) are in
//! [`mod@write`].
//!
//! Nothing here returns a secret: API keys come back by ID, name, prefix and dates, never by their
//! hash (invariant I6). Audit history is paged newest first with a keyset cursor, so a page is
//! stable while new events arrive.

pub mod policy;
pub mod write;

use base64::Engine as _;
use fleet_protocol::rest::admin::{
    AdminAgentRow, AdminApiKeyView, AdminAuditEventView, AdminGapRow, AdminGroupMappingView,
    AdminIngestionView, AdminLagRow, AdminPersonView, AdminProjectView, AdminRejectionRow,
    AdminRetentionClassView, AdminRoleBindingView, AdminRoleMappingsView, AdminTrustPolicyView,
};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{Result, Store};

/// How many of each recent refusal, loss and open run the ingestion page shows.
pub const INGESTION_LIMIT: i64 = 50;

/// Where a page of the audit history ends: the last event it showed. Opaque to clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuditCursor {
    at: OffsetDateTime,
    id: Uuid,
}

impl AuditCursor {
    /// The token a client sends back for the next page.
    #[must_use]
    pub fn encode(&self) -> String {
        let micros = self.at.unix_timestamp_nanos() / 1000;
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!("{micros}.{}", self.id))
    }

    /// The cursor a token stands for; `None` for anything this server didn't issue.
    #[must_use]
    pub fn decode(token: &str) -> Option<Self> {
        let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(token)
            .ok()?;
        let raw = String::from_utf8(raw).ok()?;
        let (micros, id) = raw.split_once('.')?;
        Some(Self {
            at: OffsetDateTime::from_unix_timestamp_nanos(
                micros.parse::<i128>().ok()?.checked_mul(1000)?,
            )
            .ok()?,
            id: id.parse().ok()?,
        })
    }
}

/// The `before` and `after` of an audit event, where it recorded them: `before`/`after`, or the
/// `from`/`to` a triage change writes. Anything else is shown only as `details`.
#[must_use]
pub fn before_and_after(
    details: &serde_json::Value,
) -> (Option<serde_json::Value>, Option<serde_json::Value>) {
    let pick = |names: [&str; 2]| {
        names
            .iter()
            .find_map(|n| details.get(*n))
            .filter(|v| !v.is_null())
            .cloned()
    };
    (pick(["before", "from"]), pick(["after", "to"]))
}

impl Store {
    /// Every project, by slug, with what is attached to it.
    pub async fn admin_projects(&self) -> Result<Vec<AdminProjectView>> {
        let rows = sqlx::query!(
            r#"SELECT p.id, p.slug, p.name, p.source, p.default_branch, p.created_at, p.archived_at,
                      (SELECT count(*) FROM api_keys k
                        WHERE k.project_id = p.id AND k.revoked_at IS NULL
                          AND (k.expires_at IS NULL OR k.expires_at > now())) AS "active_api_keys!",
                      (SELECT count(*) FROM publisher_trust_policies t
                        WHERE t.project_id = p.id AND t.revoked_at IS NULL) AS "active_trust_policies!"
                 FROM projects p ORDER BY p.slug"#
        )
        .fetch_all(self.pool())
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| AdminProjectView {
                id: r.id,
                slug: r.slug,
                name: r.name,
                source: r.source,
                default_branch: r.default_branch,
                created_at: r.created_at,
                archived_at: r.archived_at,
                active_api_keys: r.active_api_keys,
                active_trust_policies: r.active_trust_policies,
            })
            .collect())
    }

    /// Every publisher trust policy, by project and name. Revoked ones are included, marked.
    pub async fn admin_trust_policies(&self) -> Result<Vec<AdminTrustPolicyView>> {
        let rows = sqlx::query!(
            r#"SELECT t.id, p.slug AS project, t.name, t.issuer, t.audience, t.repository_id,
                      t.repository_owner_id, t.trusted_refs, t.allowed_events, t.job_workflow_ref,
                      t.environment, t.pr_policy, t.created_at, t.revoked_at, t.source AS "source!"
                 FROM publisher_trust_policies t JOIN projects p ON p.id = t.project_id
                ORDER BY p.slug, t.name"#
        )
        .fetch_all(self.pool())
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| AdminTrustPolicyView {
                id: r.id,
                project: r.project,
                name: r.name,
                issuer: r.issuer,
                audience: r.audience,
                repository_id: r.repository_id,
                repository_owner_id: r.repository_owner_id,
                trusted_refs: r.trusted_refs,
                allowed_events: r.allowed_events,
                job_workflow_ref: r.job_workflow_ref,
                environment: r.environment,
                pr_policy: r.pr_policy,
                created_at: r.created_at,
                revoked_at: r.revoked_at,
                source: r.source,
            })
            .collect())
    }

    /// Every API key, newest first. Metadata only: the secret and its hash are never selected.
    pub async fn admin_api_keys(&self) -> Result<Vec<AdminApiKeyView>> {
        let rows = sqlx::query!(
            r#"SELECT k.id, p.slug AS project, k.name, k.prefix, k.trust_level AS "trust_level!",
                      COALESCE(u.display_name, u.email) AS created_by, k.created_at, k.expires_at,
                      k.revoked_at, k.last_used_at
                 FROM api_keys k
                 JOIN projects p ON p.id = k.project_id
                 LEFT JOIN users u ON u.id = k.created_by
                ORDER BY k.created_at DESC, k.id"#
        )
        .fetch_all(self.pool())
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| AdminApiKeyView {
                id: r.id,
                project: r.project,
                name: r.name,
                prefix: r.prefix,
                trust_level: r.trust_level,
                created_by: r.created_by,
                created_at: r.created_at,
                expires_at: r.expires_at,
                revoked_at: r.revoked_at,
                last_used_at: r.last_used_at,
            })
            .collect())
    }

    /// One API key's metadata, or `None` when there is no such key. Metadata only, as above.
    pub async fn admin_api_key(&self, id: Uuid) -> Result<Option<AdminApiKeyView>> {
        let row = sqlx::query!(
            r#"SELECT k.id, p.slug AS project, k.name, k.prefix, k.trust_level AS "trust_level!",
                      COALESCE(u.display_name, u.email) AS created_by, k.created_at, k.expires_at,
                      k.revoked_at, k.last_used_at
                 FROM api_keys k
                 JOIN projects p ON p.id = k.project_id
                 LEFT JOIN users u ON u.id = k.created_by
                WHERE k.id = $1"#,
            id
        )
        .fetch_optional(self.pool())
        .await?;
        Ok(row.map(|r| AdminApiKeyView {
            id: r.id,
            project: r.project,
            name: r.name,
            prefix: r.prefix,
            trust_level: r.trust_level,
            created_by: r.created_by,
            created_at: r.created_at,
            expires_at: r.expires_at,
            revoked_at: r.revoked_at,
            last_used_at: r.last_used_at,
        }))
    }

    /// Roles bound to people, and the identity-provider groups that grant roles.
    pub async fn admin_role_mappings(&self) -> Result<AdminRoleMappingsView> {
        let bindings = sqlx::query!(
            r#"SELECT b.id, p.slug AS "project?", COALESCE(u.display_name, u.email, u.oidc_subject) AS "user!",
                      b.role AS "role!", b.created_at
                 FROM role_bindings b
                 JOIN users u ON u.id = b.user_id
                 LEFT JOIN projects p ON p.id = b.project_id
                ORDER BY p.slug NULLS FIRST, u.oidc_subject, b.id"#
        )
        .fetch_all(self.pool())
        .await?;
        let groups = sqlx::query!(
            r#"SELECT g.id, g.oidc_issuer AS issuer, g.group_claim AS "group!", p.slug AS "project?",
                      g.role AS "role!", g.source AS "source!"
                 FROM group_mappings g
                 LEFT JOIN projects p ON p.id = g.project_id
                ORDER BY g.oidc_issuer, g.group_claim, p.slug NULLS FIRST"#
        )
        .fetch_all(self.pool())
        .await?;
        let people = sqlx::query!(
            r#"SELECT id, COALESCE(display_name, email, oidc_subject) AS "name!"
                 FROM users ORDER BY COALESCE(display_name, email, oidc_subject), id"#
        )
        .fetch_all(self.pool())
        .await?;
        Ok(AdminRoleMappingsView {
            bindings: bindings
                .into_iter()
                .map(|r| AdminRoleBindingView {
                    id: r.id,
                    project: r.project,
                    user: r.user,
                    role: r.role,
                    created_at: r.created_at,
                })
                .collect(),
            groups: groups
                .into_iter()
                .map(|r| AdminGroupMappingView {
                    id: r.id,
                    issuer: r.issuer,
                    group: r.group,
                    project: r.project,
                    role: r.role,
                    source: r.source,
                })
                .collect(),
            people: people
                .into_iter()
                .map(|r| AdminPersonView {
                    id: r.id,
                    name: r.name,
                })
                .collect(),
        })
    }

    /// Every project's retention class TTLs. `ttl_seconds` is `None` where a class keeps for ever.
    pub async fn admin_retention_classes(&self) -> Result<Vec<AdminRetentionClassView>> {
        let rows = sqlx::query!(
            r#"SELECT p.slug AS project, r.class, EXTRACT(EPOCH FROM r.ttl)::bigint AS ttl_seconds
                 FROM retention_classes r JOIN projects p ON p.id = r.project_id
                ORDER BY p.slug, r.class"#
        )
        .fetch_all(self.pool())
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| AdminRetentionClassView {
                project: r.project,
                class: r.class,
                ttl_seconds: r.ttl_seconds,
            })
            .collect())
    }

    /// The agent versions seen, grouped by version and protocol, newest first.
    pub async fn admin_agents(&self) -> Result<Vec<AdminAgentRow>> {
        let rows = sqlx::query!(
            r#"SELECT agent_version AS "agent_version!", protocol_version::int AS "protocol_version!",
                      count(*) AS "runs!",
                      count(*) FILTER (WHERE state IN ('active', 'detached', 'finalizing')) AS "open_runs!",
                      max(started_at) AS "last_started_at!"
                 FROM agent_runs
                GROUP BY agent_version, protocol_version
                ORDER BY max(started_at) DESC, agent_version, protocol_version"#
        )
        .fetch_all(self.pool())
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| AdminAgentRow {
                agent_version: r.agent_version,
                protocol_version: r.protocol_version,
                runs: r.runs,
                open_runs: r.open_runs,
                last_started_at: r.last_started_at,
                // Set by the caller, which knows what this server speaks.
                compatible: false,
            })
            .collect())
    }

    /// The newest refusals and losses, and the open runs with how far behind they are.
    pub async fn admin_ingestion(&self) -> Result<AdminIngestionView> {
        let rejections = sqlx::query!(
            r#"SELECT p.slug AS "project!", j.agent_run_id, r.agent_version, j.seq, j.code,
                      j.message, j.rejected_at
                 FROM ingest_rejections j
                 JOIN agent_runs r ON r.id = j.agent_run_id
                 JOIN projects p ON p.id = r.project_id
                ORDER BY j.rejected_at DESC, j.agent_run_id, j.seq DESC
                LIMIT $1"#,
            INGESTION_LIMIT
        )
        .fetch_all(self.pool())
        .await?;
        let gaps = sqlx::query!(
            r#"SELECT p.slug AS "project!", g.agent_run_id, g.stream, g.from_seq, g.to_seq,
                      g.reason, g.recorded_at
                 FROM ingest_gaps g
                 JOIN agent_runs r ON r.id = g.agent_run_id
                 JOIN projects p ON p.id = r.project_id
                ORDER BY g.recorded_at DESC, g.agent_run_id, g.from_seq DESC
                LIMIT $1"#,
            INGESTION_LIMIT
        )
        .fetch_all(self.pool())
        .await?;
        // Each open run's newest committed event: the journal's primary key makes it one lookup.
        let open = sqlx::query!(
            r#"SELECT p.slug AS "project!", r.id AS agent_run_id, r.agent_version, r.state,
                      r.started_at, r.last_frame_at,
                      EXTRACT(EPOCH FROM (now() - r.last_frame_at))::bigint AS "seconds_since_frame!",
                      r.committed_seq,
                      (SELECT (EXTRACT(EPOCH FROM (e.received_at - e.occurred_at)) * 1000)::bigint
                         FROM agent_events e
                        WHERE e.run_day = r.run_day AND e.agent_run_id = r.id
                        ORDER BY e.seq DESC LIMIT 1) AS newest_event_lag_ms
                 FROM agent_runs r
                 JOIN projects p ON p.id = r.project_id
                WHERE r.state IN ('active', 'detached', 'finalizing')
                ORDER BY r.last_frame_at ASC, r.id
                LIMIT $1"#,
            INGESTION_LIMIT
        )
        .fetch_all(self.pool())
        .await?;
        Ok(AdminIngestionView {
            rejections: rejections
                .into_iter()
                .map(|r| AdminRejectionRow {
                    project: r.project,
                    agent_run_id: r.agent_run_id,
                    agent_version: r.agent_version,
                    seq: r.seq,
                    code: r.code,
                    message: r.message,
                    rejected_at: r.rejected_at,
                })
                .collect(),
            gaps: gaps
                .into_iter()
                .map(|r| AdminGapRow {
                    project: r.project,
                    agent_run_id: r.agent_run_id,
                    stream: r.stream,
                    from_seq: r.from_seq,
                    to_seq: r.to_seq,
                    reason: r.reason,
                    recorded_at: r.recorded_at,
                })
                .collect(),
            open_runs: open
                .into_iter()
                .map(|r| AdminLagRow {
                    project: r.project,
                    agent_run_id: r.agent_run_id,
                    agent_version: r.agent_version,
                    state: r.state,
                    started_at: r.started_at,
                    last_frame_at: r.last_frame_at,
                    seconds_since_frame: r.seconds_since_frame,
                    committed_seq: r.committed_seq,
                    newest_event_lag_ms: r.newest_event_lag_ms,
                })
                .collect(),
        })
    }

    /// A page of the audit history, newest first, after `after`. The cursor is `Some` when more
    /// events follow.
    pub async fn admin_audit(
        &self,
        after: Option<AuditCursor>,
        limit: i64,
    ) -> Result<(Vec<AdminAuditEventView>, Option<AuditCursor>)> {
        let (at, id) = match after {
            Some(c) => (Some(c.at), Some(c.id)),
            None => (None, None),
        };
        let rows = sqlx::query!(
            r#"SELECT a.id, a.at, a.actor_kind, a.actor_id,
                      COALESCE(u.display_name, u.email, k.name) AS actor,
                      a.action, p.slug AS "project?", a.target_kind, a.target_id,
                      a.details AS "details!: serde_json::Value"
                 FROM audit_events a
                 LEFT JOIN projects p ON p.id = a.project_id
                 LEFT JOIN users u ON a.actor_kind = 'user' AND u.id = a.actor_id
                 LEFT JOIN api_keys k ON a.actor_kind = 'publisher' AND k.id = a.actor_id
                WHERE ($1::timestamptz IS NULL OR (a.at, a.id) < ($1::timestamptz, $2::uuid))
                ORDER BY a.at DESC, a.id DESC
                LIMIT $3"#,
            at,
            id,
            limit + 1
        )
        .fetch_all(self.pool())
        .await?;
        let more = rows.len() > usize::try_from(limit).unwrap_or(0);
        let items: Vec<AdminAuditEventView> = rows
            .into_iter()
            .take(usize::try_from(limit).unwrap_or(0))
            .map(|r| {
                let (before, after) = before_and_after(&r.details);
                AdminAuditEventView {
                    id: r.id,
                    at: r.at,
                    actor_kind: r.actor_kind,
                    actor_id: r.actor_id,
                    actor: r.actor,
                    action: r.action,
                    project: r.project,
                    target_kind: r.target_kind,
                    target_id: r.target_id,
                    before,
                    after,
                    details: r.details,
                }
            })
            .collect();
        let next = if more {
            items.last().map(|last| AuditCursor {
                at: last.at,
                id: last.id,
            })
        } else {
            None
        };
        Ok((items, next))
    }
}

#[cfg(test)]
mod tests {
    use fleet_protocol::PROTOCOL_VERSION;
    use serde_json::json;
    use sqlx::PgPool;
    use time::Duration;

    use super::*;
    use crate::{
        auth::AuditActor,
        ingest::tests::{Agent, publisher},
    };

    fn store(pool: PgPool) -> Store {
        Store {
            pool,
            origin: Uuid::now_v7(),
        }
    }

    #[test]
    fn a_before_and_after_come_from_before_after_or_triage_from_to() {
        let triage = json!({"fingerprint": "f", "from": "unknown", "to": "known"});
        assert_eq!(
            before_and_after(&triage),
            (Some(json!("unknown")), Some(json!("known")))
        );
        let explicit = json!({"before": {"x": 1}, "after": {"x": 2}});
        assert_eq!(
            before_and_after(&explicit),
            (Some(json!({"x": 1})), Some(json!({"x": 2})))
        );
        assert_eq!(before_and_after(&json!({"seq": 3})), (None, None));
        assert_eq!(before_and_after(&json!({"to": null})), (None, None));
    }

    #[test]
    fn an_audit_cursor_round_trips_and_refuses_what_it_did_not_issue() {
        let c = AuditCursor {
            at: OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap(),
            id: Uuid::from_u128(7),
        };
        let back = AuditCursor::decode(&c.encode()).unwrap();
        assert_eq!(back, c);
        assert!(AuditCursor::decode("not a cursor").is_none());
        assert!(AuditCursor::decode("").is_none());
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn projects_list_their_keys_and_policies_that_still_work(pool: PgPool) {
        let store = store(pool);
        let app = store.upsert_project("app", "App").await.unwrap();
        let other = store.upsert_project("other", "Other").await.unwrap();
        let now = OffsetDateTime::now_utc();
        store
            .create_api_key(
                app,
                "live",
                crate::auth::TrustLevel::Trusted,
                now + Duration::days(1),
            )
            .await
            .unwrap();
        let (dead, _) = store
            .create_api_key(
                app,
                "dead",
                crate::auth::TrustLevel::Trusted,
                now + Duration::days(1),
            )
            .await
            .unwrap();
        store.revoke_api_key(dead).await.unwrap();
        sqlx::query!(
            "INSERT INTO publisher_trust_policies (project_id, name, issuer, audience,
                 repository_id, repository_owner_id, trusted_refs)
             VALUES ($1, 'main', 'https://token.actions.githubusercontent.com', 'fleet', '1', '2',
                     '{refs/heads/main}')",
            app
        )
        .execute(store.pool())
        .await
        .unwrap();
        let projects = store.admin_projects().await.unwrap();
        let slugs: Vec<&str> = projects.iter().map(|p| p.slug.as_str()).collect();
        assert_eq!(slugs, ["app", "other"]);
        let app_row = projects.iter().find(|p| p.id == app).unwrap();
        assert_eq!(
            (app_row.active_api_keys, app_row.active_trust_policies),
            (1, 1)
        );
        let other_row = projects.iter().find(|p| p.id == other).unwrap();
        assert_eq!(
            (other_row.active_api_keys, other_row.active_trust_policies),
            (0, 0)
        );
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn trust_policies_show_their_matches_and_revocation(pool: PgPool) {
        let store = store(pool);
        let app = store.upsert_project("app", "App").await.unwrap();
        sqlx::query!(
            "INSERT INTO publisher_trust_policies (project_id, name, issuer, audience,
                 repository_id, repository_owner_id, trusted_refs, allowed_events, environment,
                 pr_policy, revoked_at)
             VALUES ($1, 'release', 'https://issuer.test', 'fleet', '11', '22',
                     '{refs/heads/main,refs/tags/*}', '{push}', 'prod', 'deny', now())",
            app
        )
        .execute(store.pool())
        .await
        .unwrap();
        let policies = store.admin_trust_policies().await.unwrap();
        assert_eq!(policies.len(), 1);
        let p = &policies[0];
        assert_eq!((p.project.as_str(), p.name.as_str()), ("app", "release"));
        assert_eq!(p.trusted_refs, ["refs/heads/main", "refs/tags/*"]);
        assert_eq!(p.allowed_events, ["push"]);
        assert_eq!(p.environment.as_deref(), Some("prod"));
        assert_eq!(p.pr_policy, "deny");
        assert!(
            p.revoked_at.is_some(),
            "a revoked policy is shown as revoked"
        );
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn api_keys_show_metadata_and_never_the_secret_or_its_hash(pool: PgPool) {
        let store = store(pool);
        let app = store.upsert_project("app", "App").await.unwrap();
        let (id, secret) = store
            .create_api_key(
                app,
                "ci-main",
                crate::auth::TrustLevel::Pr,
                OffsetDateTime::now_utc() + Duration::days(9),
            )
            .await
            .unwrap();
        let keys = store.admin_api_keys().await.unwrap();
        assert_eq!(keys.len(), 1);
        let k = &keys[0];
        assert_eq!(
            (k.id, k.project.as_str(), k.name.as_str()),
            (id, "app", "ci-main")
        );
        assert_eq!(k.trust_level, "pr");
        assert_eq!(k.prefix, &secret.expose()[..secret.expose().len().min(12)]);
        assert!(k.expires_at.is_some() && k.revoked_at.is_none() && k.created_by.is_none());
        let text = serde_json::to_string(k).unwrap();
        assert!(
            !text.contains(secret.expose()),
            "the secret is never returned"
        );
        let hash: String = crate::sha256(secret.expose())
            .iter()
            .fold(String::new(), |hex, b| hex + &format!("{b:02x}"));
        assert!(!text.contains(&hash), "the hash is never returned");
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn role_mappings_list_bindings_and_group_mappings(pool: PgPool) {
        let store = store(pool);
        let app = store.upsert_project("app", "App").await.unwrap();
        let ada = store
            .upsert_user(
                "https://idp.test",
                "sub-ada",
                Some("ada@example.com"),
                Some("Ada"),
            )
            .await
            .unwrap();
        sqlx::query!(
            "INSERT INTO role_bindings (project_id, user_id, role) VALUES ($1, $2, 'maintainer'),
                                                                   (NULL, $2, 'admin')",
            app,
            ada
        )
        .execute(store.pool())
        .await
        .unwrap();
        store
            .sync_group_mappings(
                "https://idp.test",
                &[crate::sessions::GroupMapping {
                    project: Some(app),
                    group: "mobile-qa".into(),
                    role: fleet_core::policy::Role::Viewer,
                }],
            )
            .await
            .unwrap();
        let roles = store.admin_role_mappings().await.unwrap();
        assert_eq!(roles.bindings.len(), 2);
        assert!(roles.bindings.iter().all(|b| b.user == "Ada"));
        assert!(
            roles
                .bindings
                .iter()
                .any(|b| b.project.is_none() && b.role == "admin")
        );
        assert_eq!(roles.groups.len(), 1);
        assert_eq!(roles.groups[0].group, "mobile-qa");
        assert_eq!(roles.groups[0].project.as_deref(), Some("app"));
        assert_eq!(roles.groups[0].role, "viewer");
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn retention_classes_show_each_ttl_and_none_for_ever(pool: PgPool) {
        let store = store(pool);
        let app = store.upsert_project("app", "App").await.unwrap();
        sqlx::query!(
            "UPDATE retention_classes SET ttl = NULL WHERE project_id = $1 AND class = 'report'",
            app
        )
        .execute(store.pool())
        .await
        .unwrap();
        let classes = store.admin_retention_classes().await.unwrap();
        assert_eq!(classes.len(), 7, "the seven seeded classes");
        let get = |class: &str| {
            classes
                .iter()
                .find(|c| c.class == class)
                .unwrap()
                .ttl_seconds
        };
        assert_eq!(get("report"), None);
        assert_eq!(get("failure_evidence"), Some(90 * 86_400));
        assert_eq!(get("success_evidence"), Some(7 * 86_400));
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn agents_group_by_version_and_protocol_with_their_open_runs(pool: PgPool) {
        let store = store(pool);
        let p = publisher(&store).await;
        let a = Agent::new("301", "a");
        let b = Agent::new("302", "b");
        store
            .open_run(&p, &a.hello(None), crate::ingest::Transport::WebSocket)
            .await
            .unwrap();
        store
            .open_run(&p, &b.hello(None), crate::ingest::Transport::WebSocket)
            .await
            .unwrap();
        // One run ended: it still counts as a run, not as open.
        sqlx::query!(
            "UPDATE agent_runs SET state = 'ended', ended_at = now() WHERE id = $1",
            b.id
        )
        .execute(store.pool())
        .await
        .unwrap();
        let rows = store.admin_agents().await.unwrap();
        assert_eq!(rows.len(), 1, "both runs share one version and protocol");
        assert_eq!(rows[0].agent_version, "0.0.0");
        assert_eq!(rows[0].protocol_version, i32::from(PROTOCOL_VERSION));
        assert_eq!((rows[0].runs, rows[0].open_runs), (2, 1));
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn ingestion_shows_refusals_losses_and_open_runs(pool: PgPool) {
        let store = store(pool);
        let p = publisher(&store).await;
        let a = Agent::new("401", "a");
        store
            .open_run(&p, &a.hello(None), crate::ingest::Transport::WebSocket)
            .await
            .unwrap();
        sqlx::query!(
            "INSERT INTO ingest_rejections (agent_run_id, seq, code, message) VALUES ($1, 9, 'invalid', 'nul in flow name')",
            a.id
        )
        .execute(store.pool())
        .await
        .unwrap();
        sqlx::query!(
            "INSERT INTO ingest_gaps (agent_run_id, stream, from_seq, to_seq, reason)
             VALUES ($1, 'telemetry', 4, 7, 'spool full')",
            a.id
        )
        .execute(store.pool())
        .await
        .unwrap();
        let view = store.admin_ingestion().await.unwrap();
        assert_eq!(view.rejections.len(), 1);
        assert_eq!(view.rejections[0].code, "invalid");
        assert_eq!(
            view.rejections[0].message.as_deref(),
            Some("nul in flow name")
        );
        assert_eq!(view.rejections[0].agent_version, "0.0.0");
        assert_eq!(view.gaps.len(), 1);
        assert_eq!((view.gaps[0].from_seq, view.gaps[0].to_seq), (4, 7));
        assert_eq!(view.gaps[0].reason, "spool full");
        assert_eq!(view.open_runs.len(), 1);
        let lag = &view.open_runs[0];
        assert_eq!(lag.agent_run_id, a.id);
        assert_eq!(lag.state, "active");
        assert!(lag.seconds_since_frame >= 0);
        assert_eq!(lag.newest_event_lag_ms, None, "no committed event yet");
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn the_audit_history_is_newest_first_and_pages_without_repeats(pool: PgPool) {
        let store = store(pool);
        let app = store.upsert_project("app", "App").await.unwrap();
        let ada = store
            .upsert_user("https://idp.test", "sub-ada", None, Some("Ada"))
            .await
            .unwrap();
        for n in 0..5 {
            store
                .audit(
                    AuditActor::User(ada),
                    Some(app),
                    "failure.triage",
                    json!({"fingerprint": format!("f{n}"), "from": "unknown", "to": "known"}),
                )
                .await
                .unwrap();
        }
        let (first, next) = store.admin_audit(None, 2).await.unwrap();
        assert_eq!(first.len(), 2);
        assert!(first[0].at >= first[1].at);
        assert_eq!(first[0].actor.as_deref(), Some("Ada"));
        assert_eq!(first[0].before, Some(json!("unknown")));
        assert_eq!(first[0].after, Some(json!("known")));
        let (second, next2) = store.admin_audit(next, 2).await.unwrap();
        let (third, done) = store.admin_audit(next2, 2).await.unwrap();
        assert_eq!((second.len(), third.len()), (2, 1));
        assert!(done.is_none(), "the last page has no cursor");
        let mut seen: Vec<Uuid> = first
            .iter()
            .chain(&second)
            .chain(&third)
            .map(|e| e.id)
            .collect();
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), 5, "every event once across the pages");
    }
}

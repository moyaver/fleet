// SPDX-License-Identifier: AGPL-3.0-only
//! Projects and publisher credentials (PLAN §7.5, ADR-0008, research 03 §8.2).
//!
//! Agents present one bearer secret: an API key (`mf_live_…`, long-lived, one project) or an
//! agent token (`mf_agent_…`, minted by the OIDC exchange, short-lived). Both are 256-bit random
//! values stored only as SHA-256 hashes; neither can read anything (write-only).

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{Error, Result, Store, sha256};

/// Prefix of API keys.
pub const API_KEY_PREFIX: &str = "mf_live_";
/// Prefix of agent tokens minted by the OIDC exchange.
pub const AGENT_TOKEN_PREFIX: &str = "mf_agent_";

/// A bearer secret. `Debug` never prints it (invariant I6).
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// Wraps a secret value.
    #[must_use]
    pub fn new(value: String) -> Self {
        Self(value)
    }

    /// The secret itself. Call only to hand it to its owner or put it on the wire.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// How much a publisher is trusted (research 06 §2.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustLevel {
    /// A push or schedule on a trusted ref.
    Trusted,
    /// A pull request from the same repository.
    Pr,
    /// Published by a relay for code Fleet can't vouch for.
    Untrusted,
}

impl TrustLevel {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Trusted => "trusted",
            Self::Pr => "pr",
            Self::Untrusted => "untrusted",
        }
    }

    pub(crate) fn parse(s: &str) -> Result<Self> {
        match s {
            "trusted" => Ok(Self::Trusted),
            "pr" => Ok(Self::Pr),
            "untrusted" => Ok(Self::Untrusted),
            other => Err(Error::Corrupt(format!("trust level {other:?}"))),
        }
    }
}

/// CI identity from a verified OIDC token. When present it overrides what the agent reports
/// (research 06 §2.5 step 8); agent-reported values are display-only.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiClaims {
    /// `owner/name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    /// Immutable numeric repository ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository_id: Option<String>,
    /// Workflow run ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// Re-run attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_attempt: Option<u32>,
    /// The job's check run: the stable job key on GitHub.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check_run_id: Option<String>,
    /// Commit SHA.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha: Option<String>,
    /// Git ref.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    /// Triggering event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_name: Option<String>,
}

/// Which credential authenticated a publisher.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Credential {
    /// An API key.
    ApiKey(Uuid),
    /// An agent token from the OIDC exchange.
    AgentToken(Uuid),
}

/// An authenticated agent: who may publish where, and nothing else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Publisher {
    /// The one project it may write to.
    pub project_id: Uuid,
    /// The credential.
    pub credential: Credential,
    /// Its trust level.
    pub trust: TrustLevel,
    /// Verified CI claims, for OIDC-derived tokens.
    pub claims: Option<CiClaims>,
}

/// A 256-bit random secret with a prefix.
pub(crate) fn random_secret(prefix: &str) -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|e| Error::Corrupt(format!("the OS random source failed: {e}")))?;
    Ok(format!(
        "{prefix}{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    ))
}

impl Store {
    /// Creates a project, or renames it if the slug exists. Returns its ID.
    pub async fn upsert_project(&self, slug: &str, name: &str) -> Result<Uuid> {
        let id = sqlx::query_scalar!(
            "INSERT INTO projects (slug, name) VALUES ($1, $2)
             ON CONFLICT (slug) DO UPDATE SET name = EXCLUDED.name
             RETURNING id",
            slug,
            name
        )
        .fetch_one(self.pool())
        .await?;
        Ok(id)
    }

    /// A project's ID by slug.
    pub async fn project_id(&self, slug: &str) -> Result<Option<Uuid>> {
        Ok(
            sqlx::query_scalar!("SELECT id FROM projects WHERE slug = $1", slug)
                .fetch_optional(self.pool())
                .await?,
        )
    }

    /// Creates an API key. The returned secret is shown once; only its hash is kept.
    pub async fn create_api_key(
        &self,
        project_id: Uuid,
        name: &str,
        trust: TrustLevel,
    ) -> Result<(Uuid, Secret)> {
        let secret = random_secret(API_KEY_PREFIX)?;
        let prefix: String = secret.chars().take(API_KEY_PREFIX.len() + 4).collect();
        let id = sqlx::query_scalar!(
            "INSERT INTO api_keys (project_id, name, prefix, token_hash, trust_level)
             VALUES ($1, $2, $3, $4, $5) RETURNING id",
            project_id,
            name,
            prefix,
            sha256(&secret),
            trust.as_str()
        )
        .fetch_one(self.pool())
        .await?;
        Ok((id, Secret(secret)))
    }

    /// Revokes an API key.
    pub async fn revoke_api_key(&self, id: Uuid) -> Result<()> {
        sqlx::query!(
            "UPDATE api_keys SET revoked_at = now() WHERE id = $1 AND revoked_at IS NULL",
            id
        )
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Mints a short-lived agent token for a verified OIDC identity or an API key.
    pub async fn mint_agent_token(
        &self,
        project_id: Uuid,
        source: TokenSource,
        trust: TrustLevel,
        claims: &CiClaims,
        expires_at: OffsetDateTime,
    ) -> Result<(Uuid, Secret)> {
        let secret = random_secret(AGENT_TOKEN_PREFIX)?;
        let (policy, key) = match source {
            TokenSource::TrustPolicy(id) => (Some(id), None),
            TokenSource::ApiKey(id) => (None, Some(id)),
        };
        let claims = serde_json::to_value(claims).map_err(|e| Error::Corrupt(e.to_string()))?;
        let id = sqlx::query_scalar!(
            "INSERT INTO agent_tokens (project_id, token_hash, policy_id, api_key_id, trust_level,
                                       claims, expires_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING id",
            project_id,
            sha256(&secret),
            policy,
            key,
            trust.as_str(),
            claims,
            expires_at
        )
        .fetch_one(self.pool())
        .await?;
        Ok((id, Secret(secret)))
    }

    /// Resolves a bearer secret to a publisher. `None` for anything unknown, revoked or expired.
    pub async fn authenticate(&self, bearer: &str) -> Result<Option<Publisher>> {
        if bearer.starts_with(API_KEY_PREFIX) {
            let row = sqlx::query!(
                "UPDATE api_keys SET last_used_at = now()
                  WHERE token_hash = $1 AND revoked_at IS NULL
                    AND (expires_at IS NULL OR expires_at > now())
                  RETURNING id, project_id, trust_level AS \"trust_level!\"",
                sha256(bearer)
            )
            .fetch_optional(self.pool())
            .await?;
            return row
                .map(|r| {
                    Ok(Publisher {
                        project_id: r.project_id,
                        credential: Credential::ApiKey(r.id),
                        trust: TrustLevel::parse(&r.trust_level)?,
                        claims: None,
                    })
                })
                .transpose();
        }
        if bearer.starts_with(AGENT_TOKEN_PREFIX) {
            let row = sqlx::query!(
                "UPDATE agent_tokens SET last_used_at = now()
                  WHERE token_hash = $1 AND expires_at > now()
                  RETURNING id, project_id, trust_level AS \"trust_level!\", claims,
                            policy_id IS NOT NULL AS \"oidc!\"",
                sha256(bearer)
            )
            .fetch_optional(self.pool())
            .await?;
            return row
                .map(|r| {
                    let claims: CiClaims = serde_json::from_value(r.claims)
                        .map_err(|e| Error::Corrupt(format!("token claims: {e}")))?;
                    Ok(Publisher {
                        project_id: r.project_id,
                        credential: Credential::AgentToken(r.id),
                        trust: TrustLevel::parse(&r.trust_level)?,
                        claims: r.oidc.then_some(claims),
                    })
                })
                .transpose();
        }
        Ok(None)
    }

    /// Records an audit event.
    pub async fn audit(
        &self,
        actor: AuditActor,
        project_id: Option<Uuid>,
        action: &str,
        details: serde_json::Value,
    ) -> Result<()> {
        let (kind, id) = actor.parts();
        sqlx::query!(
            "INSERT INTO audit_events (actor_kind, actor_id, project_id, action, details)
             VALUES ($1, $2, $3, $4, $5)",
            kind,
            id,
            project_id,
            action,
            details
        )
        .execute(self.pool())
        .await?;
        Ok(())
    }
}

/// What an agent token was minted from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenSource {
    /// A publisher trust policy matched a verified OIDC token.
    TrustPolicy(Uuid),
    /// An API key was exchanged.
    ApiKey(Uuid),
}

/// Who did something worth auditing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditActor {
    /// The server itself.
    System,
    /// A signed-in user.
    User(Uuid),
    /// An agent, by credential.
    Publisher(Credential),
}

impl AuditActor {
    pub(crate) fn parts(self) -> (&'static str, Option<Uuid>) {
        match self {
            Self::System => ("system", None),
            Self::User(id) => ("user", Some(id)),
            Self::Publisher(Credential::ApiKey(id) | Credential::AgentToken(id)) => {
                ("publisher", Some(id))
            }
        }
    }
}

/// A trust policy as configured (ADR-0008).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyRow {
    /// Name, unique per project.
    pub name: String,
    /// OIDC issuer.
    pub issuer: String,
    /// Expected audience.
    pub audience: String,
    /// The rule.
    pub policy: fleet_core::trust::Policy,
}

/// A trust policy that may match a token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// Policy ID.
    pub policy_id: Uuid,
    /// The project it grants.
    pub project_id: Uuid,
    /// That project's slug.
    pub project_slug: String,
    /// The rule.
    pub policy: fleet_core::trust::Policy,
}

fn pr_policy_name(p: fleet_core::trust::PrPolicy) -> &'static str {
    match p {
        fleet_core::trust::PrPolicy::Restricted => "restricted",
        fleet_core::trust::PrPolicy::Full => "full",
        fleet_core::trust::PrPolicy::Deny => "deny",
    }
}

impl Store {
    /// Makes a project's stored trust policies equal the configured ones: upserts by name,
    /// revokes the rest.
    pub async fn sync_trust_policies(&self, project_id: Uuid, rows: &[PolicyRow]) -> Result<()> {
        let mut tx = self.pool().begin().await?;
        for r in rows {
            let p = &r.policy;
            sqlx::query!(
                "INSERT INTO publisher_trust_policies
                   (project_id, name, issuer, audience, repository_id, repository_owner_id,
                    trusted_refs, allowed_events, job_workflow_ref, environment, pr_policy)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
                 ON CONFLICT (project_id, name) DO UPDATE SET
                   issuer = EXCLUDED.issuer, audience = EXCLUDED.audience,
                   repository_id = EXCLUDED.repository_id,
                   repository_owner_id = EXCLUDED.repository_owner_id,
                   trusted_refs = EXCLUDED.trusted_refs, allowed_events = EXCLUDED.allowed_events,
                   job_workflow_ref = EXCLUDED.job_workflow_ref, environment = EXCLUDED.environment,
                   pr_policy = EXCLUDED.pr_policy, revoked_at = NULL",
                project_id,
                r.name,
                r.issuer,
                r.audience,
                p.repository_id,
                p.repository_owner_id,
                &p.trusted_refs,
                &p.allowed_events,
                p.job_workflow_ref,
                p.environment,
                pr_policy_name(p.pr_policy)
            )
            .execute(&mut *tx)
            .await?;
        }
        let names: Vec<String> = rows.iter().map(|r| r.name.clone()).collect();
        sqlx::query!(
            "UPDATE publisher_trust_policies SET revoked_at = now()
              WHERE project_id = $1 AND revoked_at IS NULL AND NOT (name = ANY($2))",
            project_id,
            &names
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Live trust policies for a token's issuer, audience and repository.
    pub async fn trust_candidates(
        &self,
        issuer: &str,
        audience: &str,
        repository_id: &str,
        repository_owner_id: &str,
    ) -> Result<Vec<Candidate>> {
        let rows = sqlx::query!(
            "SELECT t.id, t.project_id, p.slug, t.repository_id, t.repository_owner_id,
                    t.trusted_refs, t.allowed_events, t.pr_policy, t.job_workflow_ref, t.environment
               FROM publisher_trust_policies t JOIN projects p ON p.id = t.project_id
              WHERE t.revoked_at IS NULL AND p.archived_at IS NULL
                AND t.issuer = $1 AND t.audience = $2
                AND t.repository_id = $3 AND t.repository_owner_id = $4
              ORDER BY p.slug, t.name",
            issuer,
            audience,
            repository_id,
            repository_owner_id
        )
        .fetch_all(self.pool())
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| Candidate {
                policy_id: r.id,
                project_id: r.project_id,
                project_slug: r.slug,
                policy: fleet_core::trust::Policy {
                    repository_id: r.repository_id,
                    repository_owner_id: r.repository_owner_id,
                    trusted_refs: r.trusted_refs,
                    allowed_events: r.allowed_events,
                    pr_policy: match r.pr_policy.as_str() {
                        "full" => fleet_core::trust::PrPolicy::Full,
                        "deny" => fleet_core::trust::PrPolicy::Deny,
                        _ => fleet_core::trust::PrPolicy::Restricted,
                    },
                    job_workflow_ref: r.job_workflow_ref,
                    environment: r.environment,
                },
            })
            .collect())
    }

    /// Records a token's `jti`. False when it was already used (a replay).
    pub async fn claim_jti(
        &self,
        issuer: &str,
        jti: &str,
        expires_at: OffsetDateTime,
    ) -> Result<bool> {
        let fresh = sqlx::query_scalar!(
            "INSERT INTO oidc_jti_seen (issuer, jti, expires_at) VALUES ($1, $2, $3)
             ON CONFLICT DO NOTHING RETURNING jti",
            issuer,
            jti,
            expires_at
        )
        .fetch_optional(self.pool())
        .await?;
        Ok(fresh.is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_never_print() {
        let s = Secret::new("mf_live_abc".into());
        assert_eq!(format!("{s:?}"), "Secret(<redacted>)");
        assert_eq!(s.expose(), "mf_live_abc");
        let r = random_secret(API_KEY_PREFIX).unwrap();
        assert!(r.starts_with(API_KEY_PREFIX) && r.len() == API_KEY_PREFIX.len() + 43);
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn keys_authenticate_until_revoked(pool: sqlx::PgPool) {
        let store = Store { pool };
        let project = store.upsert_project("app", "App").await.unwrap();
        let (id, key) = store
            .create_api_key(project, "ci", TrustLevel::Trusted)
            .await
            .unwrap();
        let p = store.authenticate(key.expose()).await.unwrap().unwrap();
        assert_eq!(
            (p.project_id, p.credential),
            (project, Credential::ApiKey(id))
        );
        assert!(store.authenticate("mf_live_nope").await.unwrap().is_none());
        store.revoke_api_key(id).await.unwrap();
        assert!(store.authenticate(key.expose()).await.unwrap().is_none());

        let claims = CiClaims {
            run_id: Some("9".into()),
            ..CiClaims::default()
        };
        let (_, token) = store
            .mint_agent_token(
                project,
                TokenSource::ApiKey(id),
                TrustLevel::Pr,
                &claims,
                OffsetDateTime::now_utc() + time::Duration::hours(2),
            )
            .await
            .unwrap();
        let p = store.authenticate(token.expose()).await.unwrap().unwrap();
        assert_eq!(p.trust, TrustLevel::Pr);
        assert_eq!(p.claims, None, "only OIDC tokens carry verified claims");
    }
}

// SPDX-License-Identifier: AGPL-3.0-only
//! Browser users and sessions (PLAN §7.5, research 03 §8.1).
//!
//! A session is a 256-bit random cookie value. Only its SHA-256 hash is stored (`sessions.id`),
//! so a leaked database dump can't be replayed as cookies. The same table holds the short-lived
//! sign-in transaction (state, nonce, PKCE verifier) before a user exists; the callback consumes
//! it with [`Store::take_session`], so a callback can't be replayed.

use base64::Engine as _;
use fleet_core::policy::{Grant, Role};
use sha2::Digest as _;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{
    Error, Result, Store,
    auth::{Secret, random_secret},
};

/// A stored session, if it hasn't expired.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionRecord {
    /// The signed-in user; `None` for a sign-in transaction that hasn't finished.
    pub user_id: Option<Uuid>,
    /// What the server keeps in the session (opaque here).
    pub data: serde_json::Value,
}

/// One identity-provider group's role, on one project or on all of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupMapping {
    /// The project, or `None` for every project.
    pub project: Option<Uuid>,
    /// The group claim value.
    pub group: String,
    /// The role it grants.
    pub role: Role,
}

/// The key a cookie value is stored under.
fn key(cookie: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(cookie.as_bytes()))
}

fn role_name(role: Role) -> &'static str {
    match role {
        Role::Viewer => "viewer",
        Role::Maintainer => "maintainer",
        Role::Admin => "admin",
    }
}

fn parse_role(name: &str) -> Result<Role> {
    match name {
        "viewer" => Ok(Role::Viewer),
        "maintainer" => Ok(Role::Maintainer),
        "admin" => Ok(Role::Admin),
        other => Err(Error::Corrupt(format!("role {other:?}"))),
    }
}

impl Store {
    /// Stores a session valid until `expires_at` and returns the cookie value (shown once).
    pub async fn create_session(
        &self,
        user_id: Option<Uuid>,
        data: &serde_json::Value,
        expires_at: OffsetDateTime,
    ) -> Result<Secret> {
        let cookie = random_secret("")?;
        sqlx::query!(
            "INSERT INTO sessions (id, user_id, data, expires_at) VALUES ($1, $2, $3, $4)",
            key(&cookie),
            user_id,
            data,
            expires_at
        )
        .execute(self.pool())
        .await?;
        Ok(Secret::new(cookie))
    }

    /// The live session for a cookie value; `None` if unknown or expired.
    pub async fn session(&self, cookie: &str) -> Result<Option<SessionRecord>> {
        let row = sqlx::query!(
            "SELECT user_id, data FROM sessions WHERE id = $1 AND expires_at > now()",
            key(cookie)
        )
        .fetch_optional(self.pool())
        .await?;
        Ok(row.map(|r| SessionRecord {
            user_id: r.user_id,
            data: r.data,
        }))
    }

    /// Deletes and returns a live sign-in transaction in one statement: it can be used once.
    pub async fn take_session(&self, cookie: &str) -> Result<Option<SessionRecord>> {
        let row = sqlx::query!(
            "DELETE FROM sessions WHERE id = $1 AND user_id IS NULL RETURNING user_id, data, expires_at > now() AS \"live!\"",
            key(cookie)
        )
        .fetch_optional(self.pool())
        .await?;
        Ok(row.filter(|r| r.live).map(|r| SessionRecord {
            user_id: r.user_id,
            data: r.data,
        }))
    }

    /// Ends a session (logout). Unknown cookies are ignored.
    pub async fn delete_session(&self, cookie: &str) -> Result<()> {
        sqlx::query!("DELETE FROM sessions WHERE id = $1", key(cookie))
            .execute(self.pool())
            .await?;
        Ok(())
    }

    /// Creates or refreshes the user for an identity-provider subject; returns its ID.
    pub async fn upsert_user(
        &self,
        issuer: &str,
        subject: &str,
        email: Option<&str>,
        display_name: Option<&str>,
    ) -> Result<Uuid> {
        Ok(sqlx::query_scalar!(
            "INSERT INTO users (oidc_issuer, oidc_subject, email, display_name, last_login_at)
             VALUES ($1, $2, $3, $4, now())
             ON CONFLICT (oidc_issuer, oidc_subject) DO UPDATE SET
               email = EXCLUDED.email, display_name = EXCLUDED.display_name, last_login_at = now()
             RETURNING id",
            issuer,
            subject,
            email,
            display_name
        )
        .fetch_one(self.pool())
        .await?)
    }

    /// Makes the stored group mappings of an issuer equal the configured ones.
    pub async fn sync_group_mappings(&self, issuer: &str, rows: &[GroupMapping]) -> Result<()> {
        let mut tx = self.pool().begin().await?;
        sqlx::query!("DELETE FROM group_mappings WHERE oidc_issuer = $1", issuer)
            .execute(&mut *tx)
            .await?;
        for r in rows {
            sqlx::query!(
                "INSERT INTO group_mappings (project_id, oidc_issuer, group_claim, role)
                 VALUES ($1, $2, $3, $4)
                 ON CONFLICT (project_id, oidc_issuer, group_claim) DO UPDATE SET role = EXCLUDED.role",
                r.project,
                issuer,
                r.group,
                role_name(r.role)
            )
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Everything a user may do: the roles their groups map to, plus roles bound to them
    /// directly. The effective role on a project is the highest of these (`fleet_core::policy`).
    pub async fn grants_for(
        &self,
        user_id: Uuid,
        issuer: &str,
        groups: &[String],
    ) -> Result<Vec<Grant>> {
        let mapped = sqlx::query!(
            "SELECT project_id, role AS \"role!\" FROM group_mappings
              WHERE oidc_issuer = $1 AND group_claim = ANY($2)",
            issuer,
            groups
        )
        .fetch_all(self.pool())
        .await?;
        let bound = sqlx::query!(
            "SELECT project_id, role AS \"role!\" FROM role_bindings WHERE user_id = $1",
            user_id
        )
        .fetch_all(self.pool())
        .await?;
        mapped
            .into_iter()
            .map(|r| (r.project_id, r.role))
            .chain(bound.into_iter().map(|r| (r.project_id, r.role)))
            .map(|(project, role)| {
                Ok(Grant {
                    project,
                    role: parse_role(&role)?,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[sqlx::test(migrations = "../../migrations")]
    async fn the_sweep_removes_expired_sessions_and_keeps_live_ones(pool: sqlx::PgPool) {
        let store = Store { pool };
        let now = OffsetDateTime::now_utc();
        let data = serde_json::json!({});
        let old = store
            .create_session(None, &data, now - time::Duration::hours(1))
            .await
            .unwrap();
        let live = store
            .create_session(None, &data, now + time::Duration::hours(1))
            .await
            .unwrap();

        let removed = store.cleanup_credentials().await.unwrap();

        assert_eq!(removed, 1);
        assert!(store.session(live.expose()).await.unwrap().is_some());
        assert!(store.take_session(old.expose()).await.unwrap().is_none());
    }

    #[test]
    fn cookies_are_stored_only_as_hashes() {
        // SHA-256 of "abc", base64url without padding (the FIPS 180-2 test vector).
        assert_eq!(key("abc"), "ungWv48Bz-pBQUDeXa4iI7ADYaOWF3qctBD_YfIAFa0");
    }
}

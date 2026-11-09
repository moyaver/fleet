// SPDX-License-Identifier: AGPL-3.0-only
//! Screen G's writes (issue #139): the changes an administrator makes on the dashboard, and the
//! mechanics they all share.
//!
//! Every change runs in one transaction with its audit event: the row changes, the event is
//! written (`actor`, `action`, the target, and `before` and `after` in `details`), and the commit
//! makes both true together. A change that is refused or fails rolls back and writes nothing.
//!
//! A row is either `config` (it came from the running configuration: the start-up sync replaces
//! it, and the dashboard refuses to change it) or `dashboard` (it was added here: the sync leaves it
//! alone, and the dashboard may edit or remove it). A dashboard row that clashes with a configured
//! row is refused with a message that says which to change.
//!
//! The store checks what the database must hold (existence, uniqueness, the last global
//! administrator, the source of a row). The server checks the input's shape and the configuration
//! (the issuers it accepts, the sign-in issuer) before it calls in.

use fleet_core::policy::Role;
use fleet_core::trust::{Policy, PrPolicy};
use serde_json::{Value, json};
use sqlx::{Postgres, Transaction};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{
    Error, Result, Store,
    auth::{AuditActor, PolicyRow, Secret, TrustLevel, insert_api_key, set_api_key_expiry_in},
    sessions::{parse_role, role_name},
};

/// Longest project name, key name, policy name and group claim a person may enter.
pub const MAX_NAME: usize = 200;

/// Why a change was not made. Nothing was written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// There is no such row (the message says what was looked for).
    NotFound(&'static str),
    /// The row comes from the running configuration: change the configuration, then restart.
    FromConfig(&'static str),
    /// The change clashes with, or repeats, what is already there.
    Conflict(String),
    /// The input is not something the rules accept.
    Invalid(String),
    /// The change would leave no global administrator.
    LastGlobalAdmin,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(what) => write!(f, "no such {what}"),
            Self::FromConfig(what) => write!(
                f,
                "this {what} comes from the configuration: change it there and restart Fleet"
            ),
            Self::Conflict(message) | Self::Invalid(message) => f.write_str(message),
            Self::LastGlobalAdmin => {
                f.write_str("that would leave no global administrator: make another one first")
            }
        }
    }
}

/// A change that did not happen: refused, or the database failed.
#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    /// The change was refused; nothing was written.
    #[error("{0}")]
    Refused(Refusal),
    /// The database failed; nothing was written.
    #[error(transparent)]
    Store(#[from] Error),
}

impl From<sqlx::Error> for WriteError {
    fn from(e: sqlx::Error) -> Self {
        Self::Store(e.into())
    }
}

/// The result of one administration change.
pub type WriteResult<T> = std::result::Result<T, WriteError>;

pub(super) fn refused(r: Refusal) -> WriteError {
    WriteError::Refused(r)
}

/// Whether `slug` is a project slug: 2 to 63 lowercase letters, digits and hyphens, not starting
/// with a hyphen. The same rule the configuration applies, and the database's `CHECK`.
#[must_use]
pub fn valid_slug(slug: &str) -> bool {
    (2..=63).contains(&slug.len())
        && slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !slug.starts_with('-')
}

/// A name a person typed: trimmed, at most `max` characters, not empty, and without control
/// characters (an escape sequence in a name would be shown as text, but it is never legitimate).
pub fn clean_name(value: &str, what: &str, max: usize) -> std::result::Result<String, Refusal> {
    let v = value.trim();
    if v.is_empty() {
        return Err(Refusal::Invalid(format!("{what} is required")));
    }
    if v.chars().count() > max {
        return Err(Refusal::Invalid(format!(
            "{what} is longer than {max} characters"
        )));
    }
    if v.chars().any(char::is_control) {
        return Err(Refusal::Invalid(format!(
            "{what} has a control character in it"
        )));
    }
    Ok(v.to_owned())
}

/// What an audit event records about the change: who, what, and on which row.
pub(super) struct Change {
    pub(super) actor: AuditActor,
    pub(super) project: Option<Uuid>,
    pub(super) action: &'static str,
    pub(super) target_kind: &'static str,
    pub(super) target_id: Option<Uuid>,
}

/// Writes one audit event inside the change's transaction. `before` and `after` are the row's
/// state either side of the change: `null` where there was none (a creation has no `before`).
pub(super) async fn record(
    tx: &mut Transaction<'_, Postgres>,
    change: Change,
    before: Option<Value>,
    after: Option<Value>,
) -> Result<()> {
    let (kind, id) = change.actor.parts();
    let details = json!({ "before": before, "after": after });
    sqlx::query!(
        "INSERT INTO audit_events (actor_kind, actor_id, project_id, action, target_kind, target_id, details)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
        kind,
        id,
        change.project,
        change.action,
        change.target_kind,
        change.target_id,
        details
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Ends the browser sessions of one person. Their grants are resolved at sign-in, so a changed
/// role takes effect the next time they sign in, which this makes immediate.
async fn end_sessions(tx: &mut Transaction<'_, Postgres>, user: Uuid) -> Result<()> {
    sqlx::query!("DELETE FROM sessions WHERE user_id = $1", user)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// A time as the audit trail records it: RFC 3339, or `null` for none.
fn when(t: Option<OffsetDateTime>) -> Value {
    t.and_then(|t| {
        t.format(&time::format_description::well_known::Rfc3339)
            .ok()
    })
    .map_or(Value::Null, Value::from)
}

/// A trust policy as the audit trail and the dashboard show it.
fn policy_json(row: &PolicyRow, revoked: Option<bool>) -> Value {
    let p = &row.policy;
    let mut v = json!({
        "name": row.name,
        "issuer": row.issuer,
        "audience": row.audience,
        "repository_id": p.repository_id,
        "repository_owner_id": p.repository_owner_id,
        "trusted_refs": p.trusted_refs,
        "allowed_events": p.allowed_events,
        "job_workflow_ref": p.job_workflow_ref,
        "environment": p.environment,
        "pr_policy": pr_policy_name(p.pr_policy),
    });
    if let Some(revoked) = revoked {
        v["revoked"] = json!(revoked);
    }
    v
}

fn pr_policy_name(p: PrPolicy) -> &'static str {
    match p {
        PrPolicy::Restricted => "restricted",
        PrPolicy::Full => "full",
        PrPolicy::Deny => "deny",
    }
}

/// The text that names a row, for the messages and the audit trail.
fn scope_json(slug: Option<&str>) -> Value {
    slug.map_or(Value::Null, |s| json!(s))
}

/// Checks a trust policy the database must still hold: a usable name, and the row a configured
/// policy would clash with. Shared by add and edit.
fn check_policy_row(row: &PolicyRow) -> std::result::Result<(), Refusal> {
    clean_name(&row.name, "the policy's name", MAX_NAME)?;
    for (what, value) in [
        ("the repository ID", &row.policy.repository_id),
        ("the owner ID", &row.policy.repository_owner_id),
        ("the issuer", &row.issuer),
        ("the audience", &row.audience),
    ] {
        if value.trim().is_empty() {
            return Err(Refusal::Invalid(format!("{what} is required")));
        }
    }
    Ok(())
}

impl Store {
    /// Creates a project. Refuses a slug that exists: a create never renames a project.
    pub async fn create_project(
        &self,
        actor: AuditActor,
        slug: &str,
        name: &str,
    ) -> WriteResult<Uuid> {
        if !valid_slug(slug) {
            return Err(refused(Refusal::Invalid(
                "a project's slug is 2 to 63 lowercase letters, digits and hyphens, and does not start with a hyphen"
                    .into(),
            )));
        }
        let name = clean_name(name, "the project's name", MAX_NAME).map_err(refused)?;
        let mut tx = self.pool().begin().await?;
        let id = sqlx::query_scalar!(
            "INSERT INTO projects (slug, name) VALUES ($1, $2)
             ON CONFLICT (slug) DO NOTHING RETURNING id",
            slug,
            name
        )
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| {
            refused(Refusal::Conflict(format!(
                "a project with slug {slug} already exists"
            )))
        })?;
        record(
            &mut tx,
            Change {
                actor,
                project: Some(id),
                action: "project.create",
                target_kind: "project",
                target_id: Some(id),
            },
            None,
            Some(json!({ "slug": slug, "name": name })),
        )
        .await?;
        tx.commit().await?;
        Ok(id)
    }

    /// Renames a project. The slug stays: it is what the API and the agents name it by.
    pub async fn rename_project(
        &self,
        actor: AuditActor,
        project: Uuid,
        name: &str,
    ) -> WriteResult<()> {
        let name = clean_name(name, "the project's name", MAX_NAME).map_err(refused)?;
        let mut tx = self.pool().begin().await?;
        let old = sqlx::query!(
            "SELECT slug, name, source FROM projects WHERE id = $1 FOR UPDATE",
            project
        )
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| refused(Refusal::NotFound("project")))?;
        if old.source == "config" {
            return Err(refused(Refusal::FromConfig("project")));
        }
        if old.name == name {
            return Err(refused(Refusal::Conflict(
                "the project already has that name".into(),
            )));
        }
        sqlx::query!("UPDATE projects SET name = $2 WHERE id = $1", project, name)
            .execute(&mut *tx)
            .await?;
        record(
            &mut tx,
            Change {
                actor,
                project: Some(project),
                action: "project.rename",
                target_kind: "project",
                target_id: Some(project),
            },
            Some(json!({ "slug": old.slug, "name": old.name })),
            Some(json!({ "slug": old.slug, "name": name })),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Creates an API key that stops working at `expires_at`, made by `created_by`. Returns its ID
    /// and the secret, which the caller shows once and nothing stores in clear.
    pub async fn create_api_key_as(
        &self,
        actor: AuditActor,
        project: Uuid,
        name: &str,
        trust: TrustLevel,
        expires_at: OffsetDateTime,
    ) -> WriteResult<(Uuid, Secret)> {
        let name = clean_name(name, "the key's name", MAX_NAME).map_err(refused)?;
        if expires_at <= OffsetDateTime::now_utc() {
            return Err(refused(Refusal::Invalid(
                "a key must expire in the future: give it at least a day".into(),
            )));
        }
        let created_by = match actor {
            AuditActor::User(id) => Some(id),
            _ => None,
        };
        let mut tx = self.pool().begin().await?;
        let slug = sqlx::query_scalar!("SELECT slug FROM projects WHERE id = $1", project)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| refused(Refusal::NotFound("project")))?;
        let (id, secret) =
            insert_api_key(&mut *tx, project, &name, trust, expires_at, created_by).await?;
        let prefix: String = secret
            .expose()
            .chars()
            .take(crate::auth::API_KEY_PREFIX.len() + 4)
            .collect();
        record(
            &mut tx,
            Change {
                actor,
                project: Some(project),
                action: "api_key.create",
                target_kind: "api_key",
                target_id: Some(id),
            },
            None,
            Some(json!({
                "project": slug,
                "name": name,
                "prefix": prefix,
                "trust_level": trust.as_str(),
                "expires_at": when(Some(expires_at)),
            })),
        )
        .await?;
        tx.commit().await?;
        Ok((id, secret))
    }

    /// Revokes an API key, and every agent token minted from it stops with it. A key already
    /// revoked is refused: it has nothing left to change.
    pub async fn revoke_api_key_as(&self, actor: AuditActor, id: Uuid) -> WriteResult<()> {
        let mut tx = self.pool().begin().await?;
        let key = key_row_locked(&mut tx, id).await?;
        if key.revoked_at.is_some() {
            return Err(refused(Refusal::Conflict(
                "that key is already revoked".into(),
            )));
        }
        set_revoked(&mut tx, id).await?;
        record(
            &mut tx,
            Change {
                actor,
                project: Some(key.project),
                action: "api_key.revoke",
                target_kind: "api_key",
                target_id: Some(id),
            },
            Some(key_state(&key.name, key.expires_at, false)),
            Some(key_state(&key.name, key.expires_at, true)),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Sets when an API key stops working, `in_days` from now (`0` ends it now). A revoked key
    /// cannot be given a new expiry.
    pub async fn expire_api_key_as(
        &self,
        actor: AuditActor,
        id: Uuid,
        expires_at: OffsetDateTime,
    ) -> WriteResult<()> {
        let mut tx = self.pool().begin().await?;
        let key = key_row_locked(&mut tx, id).await?;
        if key.revoked_at.is_some() {
            return Err(refused(Refusal::Conflict(
                "a revoked key cannot be given a new expiry".into(),
            )));
        }
        if !set_api_key_expiry_in(&mut *tx, id, expires_at).await? {
            return Err(refused(Refusal::NotFound("API key")));
        }
        record(
            &mut tx,
            Change {
                actor,
                project: Some(key.project),
                action: "api_key.expire",
                target_kind: "api_key",
                target_id: Some(id),
            },
            Some(json!({ "name": key.name, "expires_at": when(key.expires_at) })),
            Some(json!({ "name": key.name, "expires_at": when(Some(expires_at)) })),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Gives a person a role on a project, or on every project (`None`). A person has one role
    /// per project: changing it means removing it and adding it again. Their open sessions end, so
    /// the role applies at their next request.
    pub async fn add_role_binding(
        &self,
        actor: AuditActor,
        project: Option<Uuid>,
        user: Uuid,
        role: Role,
    ) -> WriteResult<Uuid> {
        let mut tx = self.pool().begin().await?;
        let person = person_name(&mut tx, user).await?;
        let slug = project_slug(&mut tx, project).await?;
        let creator = match actor {
            AuditActor::User(id) => Some(id),
            _ => None,
        };
        let id = sqlx::query_scalar!(
            "INSERT INTO role_bindings (project_id, user_id, role, created_by)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT DO NOTHING RETURNING id",
            project,
            user,
            role_name(role),
            creator
        )
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| {
            refused(Refusal::Conflict(
                "that person already has a role here: remove it first, then add the new one".into(),
            ))
        })?;
        let after = json!({ "project": scope_json(slug.as_deref()), "person": person, "role": role_name(role) });
        record(
            &mut tx,
            Change {
                actor,
                project,
                action: "role_binding.add",
                target_kind: "role_binding",
                target_id: Some(id),
            },
            None,
            Some(after),
        )
        .await?;
        end_sessions(&mut tx, user).await?;
        tx.commit().await?;
        Ok(id)
    }

    /// Removes a role bound to a person. The last global administrator cannot be removed.
    pub async fn remove_role_binding(&self, actor: AuditActor, id: Uuid) -> WriteResult<()> {
        let mut tx = self.pool().begin().await?;
        let b = sqlx::query!(
            r#"SELECT b.user_id, b.project_id, b.role AS "role!", p.slug AS "slug?",
                      COALESCE(u.display_name, u.email, u.oidc_subject) AS "person!"
                 FROM role_bindings b
                 JOIN users u ON u.id = b.user_id
                 LEFT JOIN projects p ON p.id = b.project_id
                WHERE b.id = $1 FOR UPDATE OF b"#,
            id
        )
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| refused(Refusal::NotFound("role")))?;
        if b.project_id.is_none() && b.role == role_name(Role::Admin) {
            // Lock every global administrator binding, so two removals can't both see a second one.
            let admins = sqlx::query_scalar!(
                "SELECT id FROM role_bindings
                  WHERE project_id IS NULL AND role = 'admin' FOR UPDATE"
            )
            .fetch_all(&mut *tx)
            .await?;
            if admins.len() <= 1 {
                return Err(refused(Refusal::LastGlobalAdmin));
            }
        }
        sqlx::query!("DELETE FROM role_bindings WHERE id = $1", id)
            .execute(&mut *tx)
            .await?;
        record(
            &mut tx,
            Change {
                actor,
                project: b.project_id,
                action: "role_binding.remove",
                target_kind: "role_binding",
                target_id: Some(id),
            },
            Some(json!({ "project": scope_json(b.slug.as_deref()), "person": b.person, "role": b.role })),
            None,
        )
        .await?;
        end_sessions(&mut tx, b.user_id).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Maps an identity-provider group to a role, on a project or on every project. Takes effect
    /// at each member's next sign-in. A configured mapping for the same group and scope refuses it.
    pub async fn add_group_mapping(
        &self,
        actor: AuditActor,
        issuer: &str,
        project: Option<Uuid>,
        group: &str,
        role: Role,
    ) -> WriteResult<Uuid> {
        let group = clean_name(group, "the group", MAX_NAME).map_err(refused)?;
        let mut tx = self.pool().begin().await?;
        let slug = project_slug(&mut tx, project).await?;
        let existing = sqlx::query_scalar!(
            "SELECT source FROM group_mappings
              WHERE oidc_issuer = $1 AND group_claim = $2 AND project_id IS NOT DISTINCT FROM $3::uuid",
            issuer,
            group,
            project
        )
        .fetch_optional(&mut *tx)
        .await?;
        match existing.as_deref() {
            Some("config") => {
                return Err(refused(Refusal::Conflict(format!(
                    "the configuration already maps group {group:?} here: change it there and restart Fleet"
                ))));
            }
            Some(_) => {
                return Err(refused(Refusal::Conflict(format!(
                    "group {group:?} is already mapped here: edit that mapping instead"
                ))));
            }
            None => {}
        }
        let id = sqlx::query_scalar!(
            "INSERT INTO group_mappings (project_id, oidc_issuer, group_claim, role, source)
             VALUES ($1, $2, $3, $4, 'dashboard')
             ON CONFLICT DO NOTHING RETURNING id",
            project,
            issuer,
            group,
            role_name(role)
        )
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| {
            refused(Refusal::Conflict(format!(
                "group {group:?} is already mapped here"
            )))
        })?;
        record(
            &mut tx,
            Change {
                actor,
                project,
                action: "group_mapping.add",
                target_kind: "group_mapping",
                target_id: Some(id),
            },
            None,
            Some(json!({ "issuer": issuer, "group": group, "project": scope_json(slug.as_deref()), "role": role_name(role) })),
        )
        .await?;
        tx.commit().await?;
        Ok(id)
    }

    /// Changes the role a dashboard group mapping grants. A configured mapping is locked.
    pub async fn set_group_mapping_role(
        &self,
        actor: AuditActor,
        id: Uuid,
        role: Role,
    ) -> WriteResult<()> {
        let mut tx = self.pool().begin().await?;
        let g = group_row_locked(&mut tx, id).await?;
        if g.source == "config" {
            return Err(refused(Refusal::FromConfig("group mapping")));
        }
        if parse_role(&g.role).map_err(WriteError::Store)? == role {
            return Err(refused(Refusal::Conflict(
                "the mapping already grants that role".into(),
            )));
        }
        sqlx::query!(
            "UPDATE group_mappings SET role = $2 WHERE id = $1",
            id,
            role_name(role)
        )
        .execute(&mut *tx)
        .await?;
        record(
            &mut tx,
            Change {
                actor,
                project: g.project,
                action: "group_mapping.edit",
                target_kind: "group_mapping",
                target_id: Some(id),
            },
            Some(group_json(&g)),
            Some(group_json_with_role(&g, role_name(role))),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Removes a dashboard group mapping. A configured mapping is locked.
    pub async fn remove_group_mapping(&self, actor: AuditActor, id: Uuid) -> WriteResult<()> {
        let mut tx = self.pool().begin().await?;
        let g = group_row_locked(&mut tx, id).await?;
        if g.source == "config" {
            return Err(refused(Refusal::FromConfig("group mapping")));
        }
        sqlx::query!("DELETE FROM group_mappings WHERE id = $1", id)
            .execute(&mut *tx)
            .await?;
        record(
            &mut tx,
            Change {
                actor,
                project: g.project,
                action: "group_mapping.remove",
                target_kind: "group_mapping",
                target_id: Some(id),
            },
            Some(group_json(&g)),
            None,
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Adds a publisher trust policy to a project. A configured policy with the same name refuses it.
    pub async fn add_trust_policy(
        &self,
        actor: AuditActor,
        project: Uuid,
        row: &PolicyRow,
    ) -> WriteResult<Uuid> {
        check_policy_row(row).map_err(refused)?;
        let mut tx = self.pool().begin().await?;
        let slug = project_slug(&mut tx, Some(project)).await?;
        let existing = sqlx::query_scalar!(
            "SELECT source FROM publisher_trust_policies WHERE project_id = $1 AND name = $2",
            project,
            row.name
        )
        .fetch_optional(&mut *tx)
        .await?;
        match existing.as_deref() {
            Some("config") => {
                return Err(refused(Refusal::Conflict(format!(
                    "the configuration already has a trust policy named {:?} on this project: pick another name, or change it there",
                    row.name
                ))));
            }
            Some(_) => {
                return Err(refused(Refusal::Conflict(format!(
                    "a trust policy named {:?} already exists on this project: edit it instead",
                    row.name
                ))));
            }
            None => {}
        }
        let id = insert_policy(&mut tx, project, row, "dashboard").await?;
        record(
            &mut tx,
            Change {
                actor,
                project: Some(project),
                action: "trust_policy.add",
                target_kind: "trust_policy",
                target_id: Some(id),
            },
            None,
            Some(with_project(policy_json(row, Some(false)), slug.as_deref())),
        )
        .await?;
        tx.commit().await?;
        Ok(id)
    }

    /// Replaces the fields of a dashboard trust policy (its name may change, if no other policy
    /// takes it). A revoked policy stays revoked. A configured policy is locked.
    pub async fn edit_trust_policy(
        &self,
        actor: AuditActor,
        id: Uuid,
        row: &PolicyRow,
    ) -> WriteResult<()> {
        check_policy_row(row).map_err(refused)?;
        let mut tx = self.pool().begin().await?;
        let old = policy_row_locked(&mut tx, id).await?;
        if old.source == "config" {
            return Err(refused(Refusal::FromConfig("trust policy")));
        }
        if row.name != old.name {
            let taken = sqlx::query_scalar!(
                "SELECT EXISTS (SELECT 1 FROM publisher_trust_policies WHERE project_id = $1 AND name = $2) AS \"taken!\"",
                old.project,
                row.name
            )
            .fetch_one(&mut *tx)
            .await?;
            if taken {
                return Err(refused(Refusal::Conflict(format!(
                    "a trust policy named {:?} already exists on this project",
                    row.name
                ))));
            }
        }
        let p = &row.policy;
        sqlx::query!(
            "UPDATE publisher_trust_policies SET name = $2, issuer = $3, audience = $4,
                    repository_id = $5, repository_owner_id = $6, trusted_refs = $7,
                    allowed_events = $8, job_workflow_ref = $9, environment = $10, pr_policy = $11
              WHERE id = $1",
            id,
            row.name,
            row.issuer,
            row.audience,
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
        record(
            &mut tx,
            Change {
                actor,
                project: Some(old.project),
                action: "trust_policy.edit",
                target_kind: "trust_policy",
                target_id: Some(id),
            },
            Some(old.state.clone()),
            Some(policy_json(row, Some(old.revoked_at.is_some()))),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Revokes a dashboard trust policy: no new token may come from it, and the tokens it minted
    /// end now. A configured policy is locked; one already revoked is refused.
    pub async fn revoke_trust_policy(&self, actor: AuditActor, id: Uuid) -> WriteResult<()> {
        let mut tx = self.pool().begin().await?;
        let old = policy_row_locked(&mut tx, id).await?;
        if old.source == "config" {
            return Err(refused(Refusal::FromConfig("trust policy")));
        }
        if old.revoked_at.is_some() {
            return Err(refused(Refusal::Conflict(
                "that trust policy is already revoked".into(),
            )));
        }
        sqlx::query!(
            "UPDATE publisher_trust_policies SET revoked_at = now() WHERE id = $1",
            id
        )
        .execute(&mut *tx)
        .await?;
        end_policy_tokens(&mut tx, id).await?;
        record(
            &mut tx,
            Change {
                actor,
                project: Some(old.project),
                action: "trust_policy.revoke",
                target_kind: "trust_policy",
                target_id: Some(id),
            },
            Some(old.state.clone()),
            Some(json!({ "name": old.name, "revoked": true })),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Removes a dashboard trust policy. The tokens minted from it end first, so none outlives it.
    /// A configured policy is locked.
    pub async fn remove_trust_policy(&self, actor: AuditActor, id: Uuid) -> WriteResult<()> {
        let mut tx = self.pool().begin().await?;
        let old = policy_row_locked(&mut tx, id).await?;
        if old.source == "config" {
            return Err(refused(Refusal::FromConfig("trust policy")));
        }
        end_policy_tokens(&mut tx, id).await?;
        sqlx::query!("DELETE FROM publisher_trust_policies WHERE id = $1", id)
            .execute(&mut *tx)
            .await?;
        record(
            &mut tx,
            Change {
                actor,
                project: Some(old.project),
                action: "trust_policy.remove",
                target_kind: "trust_policy",
                target_id: Some(id),
            },
            Some(old.state.clone()),
            None,
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }
}

/// The API key's name, project, expiry and revocation, locked for a change.
struct KeyRow {
    name: String,
    project: Uuid,
    expires_at: Option<OffsetDateTime>,
    revoked_at: Option<OffsetDateTime>,
}

async fn key_row_locked(tx: &mut Transaction<'_, Postgres>, id: Uuid) -> WriteResult<KeyRow> {
    let row = sqlx::query!(
        "SELECT name, project_id, expires_at, revoked_at FROM api_keys WHERE id = $1 FOR UPDATE",
        id
    )
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| refused(Refusal::NotFound("API key")))?;
    Ok(KeyRow {
        name: row.name,
        project: row.project_id,
        expires_at: row.expires_at,
        revoked_at: row.revoked_at,
    })
}

async fn set_revoked(tx: &mut Transaction<'_, Postgres>, id: Uuid) -> Result<()> {
    sqlx::query!(
        "UPDATE api_keys SET revoked_at = COALESCE(revoked_at, now()) WHERE id = $1",
        id
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

fn key_state(name: &str, expires_at: Option<OffsetDateTime>, revoked: bool) -> Value {
    json!({ "name": name, "expires_at": when(expires_at), "revoked": revoked })
}

async fn person_name(tx: &mut Transaction<'_, Postgres>, user: Uuid) -> WriteResult<String> {
    let name = sqlx::query_scalar!(
        r#"SELECT COALESCE(display_name, email, oidc_subject) AS "name!" FROM users WHERE id = $1"#,
        user
    )
    .fetch_optional(&mut **tx)
    .await?;
    name.ok_or_else(|| refused(Refusal::NotFound("person")))
}

async fn project_slug(
    tx: &mut Transaction<'_, Postgres>,
    project: Option<Uuid>,
) -> WriteResult<Option<String>> {
    match project {
        None => Ok(None),
        Some(id) => {
            let slug = sqlx::query_scalar!("SELECT slug FROM projects WHERE id = $1", id)
                .fetch_optional(&mut **tx)
                .await?;
            slug.map(Some)
                .ok_or_else(|| refused(Refusal::NotFound("project")))
        }
    }
}

/// A group mapping, locked for a change.
struct GroupRowLocked {
    issuer: String,
    group: String,
    project: Option<Uuid>,
    slug: Option<String>,
    role: String,
    source: String,
}

async fn group_row_locked(
    tx: &mut Transaction<'_, Postgres>,
    id: Uuid,
) -> WriteResult<GroupRowLocked> {
    let row = sqlx::query!(
        r#"SELECT g.oidc_issuer, g.group_claim, g.project_id, p.slug AS "slug?", g.role AS "role!",
                  g.source AS "source!"
             FROM group_mappings g LEFT JOIN projects p ON p.id = g.project_id
            WHERE g.id = $1 FOR UPDATE OF g"#,
        id
    )
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| refused(Refusal::NotFound("group mapping")))?;
    Ok(GroupRowLocked {
        issuer: row.oidc_issuer,
        group: row.group_claim,
        project: row.project_id,
        slug: row.slug,
        role: row.role,
        source: row.source,
    })
}

fn group_json(g: &GroupRowLocked) -> Value {
    group_json_with_role(g, &g.role)
}

fn group_json_with_role(g: &GroupRowLocked, role: &str) -> Value {
    json!({ "issuer": g.issuer, "group": g.group, "project": scope_json(g.slug.as_deref()), "role": role })
}

/// A trust policy, locked for a change.
struct PolicyRowLocked {
    project: Uuid,
    name: String,
    source: String,
    revoked_at: Option<OffsetDateTime>,
    /// The policy as the audit trail shows it, revoked or not.
    state: Value,
}

async fn policy_row_locked(
    tx: &mut Transaction<'_, Postgres>,
    id: Uuid,
) -> WriteResult<PolicyRowLocked> {
    let r = sqlx::query!(
        r#"SELECT project_id, name, issuer, audience, repository_id, repository_owner_id,
                  trusted_refs, allowed_events, job_workflow_ref, environment, pr_policy,
                  source AS "source!", revoked_at
             FROM publisher_trust_policies WHERE id = $1 FOR UPDATE"#,
        id
    )
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| refused(Refusal::NotFound("trust policy")))?;
    let row = PolicyRow {
        name: r.name.clone(),
        issuer: r.issuer,
        audience: r.audience,
        policy: Policy {
            repository_id: r.repository_id,
            repository_owner_id: r.repository_owner_id,
            trusted_refs: r.trusted_refs,
            allowed_events: r.allowed_events,
            pr_policy: match r.pr_policy.as_str() {
                "full" => PrPolicy::Full,
                "deny" => PrPolicy::Deny,
                _ => PrPolicy::Restricted,
            },
            job_workflow_ref: r.job_workflow_ref,
            environment: r.environment,
        },
    };
    Ok(PolicyRowLocked {
        project: r.project_id,
        name: r.name,
        source: r.source,
        revoked_at: r.revoked_at,
        state: policy_json(&row, Some(r.revoked_at.is_some())),
    })
}

fn with_project(mut v: Value, slug: Option<&str>) -> Value {
    v["project"] = scope_json(slug);
    v
}

async fn insert_policy(
    tx: &mut Transaction<'_, Postgres>,
    project: Uuid,
    row: &PolicyRow,
    source: &str,
) -> WriteResult<Uuid> {
    let p = &row.policy;
    let id = sqlx::query_scalar!(
        "INSERT INTO publisher_trust_policies
           (project_id, name, issuer, audience, repository_id, repository_owner_id,
            trusted_refs, allowed_events, job_workflow_ref, environment, pr_policy, source)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
         ON CONFLICT (project_id, name) DO NOTHING RETURNING id",
        project,
        row.name,
        row.issuer,
        row.audience,
        p.repository_id,
        p.repository_owner_id,
        &p.trusted_refs,
        &p.allowed_events,
        p.job_workflow_ref,
        p.environment,
        pr_policy_name(p.pr_policy),
        source
    )
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| {
        refused(Refusal::Conflict(format!(
            "a trust policy named {:?} already exists on this project",
            row.name
        )))
    })?;
    Ok(id)
}

async fn end_policy_tokens(tx: &mut Transaction<'_, Postgres>, policy: Uuid) -> Result<()> {
    sqlx::query!(
        "UPDATE agent_tokens SET expires_at = now() WHERE policy_id = $1 AND expires_at > now()",
        policy
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use fleet_core::policy::Grant;
    use serde_json::json;
    use sqlx::PgPool;
    use time::Duration;

    use super::*;
    use crate::{
        auth::{CiClaims, TokenSource},
        sessions::GroupMapping,
    };

    const ISSUER: &str = "https://token.actions.githubusercontent.com";

    fn store(pool: PgPool) -> Store {
        Store {
            pool,
            origin: Uuid::now_v7(),
        }
    }

    /// An administrator, signed in: the actor of every change in these tests.
    async fn ada(store: &Store) -> AuditActor {
        AuditActor::User(
            store
                .upsert_user(
                    "https://idp.test",
                    "ada",
                    Some("ada@example.test"),
                    Some("Ada"),
                )
                .await
                .unwrap(),
        )
    }

    #[test]
    fn slugs_follow_the_configuration_and_the_database() {
        for ok in ["ab", "app", "mobile-qa-2", "a1"] {
            assert!(valid_slug(ok), "{ok}");
        }
        for bad in [
            "a",
            "",
            "-app",
            "App",
            "my app",
            "app_x",
            "ünïcode",
            &"a".repeat(64),
        ] {
            assert!(!valid_slug(bad), "{bad:?}");
        }
    }

    #[test]
    fn names_are_trimmed_bounded_and_free_of_control_characters() {
        assert_eq!(clean_name("  Mobile  ", "x", 10).unwrap(), "Mobile");
        assert!(
            matches!(clean_name("  ", "x", 10), Err(Refusal::Invalid(m)) if m == "x is required")
        );
        assert!(clean_name(&"n".repeat(11), "x", 10).is_err());
        assert!(clean_name("a\u{1b}[31m", "x", 10).is_err());
        assert!(clean_name("a\nb", "x", 10).is_err());
    }

    fn policy(name: &str) -> PolicyRow {
        PolicyRow {
            name: name.into(),
            issuer: ISSUER.into(),
            audience: "fleet".into(),
            policy: fleet_core::trust::Policy {
                repository_id: "1001".into(),
                repository_owner_id: "2002".into(),
                trusted_refs: vec!["refs/heads/main".into()],
                allowed_events: Vec::new(),
                pr_policy: fleet_core::trust::PrPolicy::Restricted,
                job_workflow_ref: None,
                environment: Some("prod".into()),
            },
        }
    }

    /// Every audit event, oldest first: `(action, before, after)`.
    async fn audit_of(store: &Store) -> Vec<(String, Value, Value)> {
        sqlx::query!(
            r#"SELECT action, details AS "details!: Value" FROM audit_events ORDER BY at, id"#
        )
        .fetch_all(store.pool())
        .await
        .unwrap()
        .into_iter()
        .map(|r| {
            let d = r.details;
            (
                r.action,
                d.get("before").cloned().unwrap_or(Value::Null),
                d.get("after").cloned().unwrap_or(Value::Null),
            )
        })
        .collect()
    }

    async fn audit_count(store: &Store) -> i64 {
        sqlx::query_scalar!(r#"SELECT count(*) AS "n!" FROM audit_events"#)
            .fetch_one(store.pool())
            .await
            .unwrap()
    }

    fn is_refused<T: std::fmt::Debug>(res: WriteResult<T>) -> Refusal {
        match res {
            Err(WriteError::Refused(r)) => r,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn a_project_is_created_once_and_its_slug_is_validated(pool: PgPool) {
        let store = store(pool);
        let actor = ada(&store).await;
        let id = store
            .create_project(actor, "mobile", "Mobile")
            .await
            .unwrap();
        assert_eq!(store.project_id("mobile").await.unwrap(), Some(id));
        let trail = audit_of(&store).await;
        assert_eq!(trail.len(), 1, "one event per change");
        assert_eq!(trail[0].0, "project.create");
        assert_eq!(trail[0].1, Value::Null, "a creation has no before");
        assert_eq!(trail[0].2, json!({"slug": "mobile", "name": "Mobile"}));

        // A slug that exists is refused: a create never renames. Nothing is written.
        let r = is_refused(store.create_project(actor, "mobile", "Other").await);
        assert!(matches!(r, Refusal::Conflict(_)), "{r:?}");
        assert_eq!(store.project_id("mobile").await.unwrap(), Some(id));
        assert_eq!(audit_count(&store).await, 1);

        for bad in ["M", "Bad Slug", "-x"] {
            let r = is_refused(store.create_project(actor, bad, "x").await);
            assert!(matches!(r, Refusal::Invalid(_)), "{bad}: {r:?}");
        }
        let r = is_refused(store.create_project(actor, "fine", "bad\u{7}name").await);
        assert!(matches!(r, Refusal::Invalid(_)), "{r:?}");
        assert_eq!(audit_count(&store).await, 1, "a refusal writes nothing");
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn a_project_is_renamed_with_its_old_and_new_name(pool: PgPool) {
        let store = store(pool);
        let actor = ada(&store).await;
        // A dashboard project: a configured one is not renamed here (the test below).
        let id = store.create_project(actor, "app", "App").await.unwrap();
        store
            .rename_project(actor, id, "Application")
            .await
            .unwrap();
        let row = store.admin_projects().await.unwrap();
        assert_eq!(row[0].name, "Application");
        let trail = audit_of(&store).await;
        assert_eq!(trail.len(), 2, "the creation, then the rename");
        assert_eq!(trail[1].0, "project.rename");
        assert_eq!(trail[1].1, json!({"slug": "app", "name": "App"}));
        assert_eq!(trail[1].2, json!({"slug": "app", "name": "Application"}));
        let r = is_refused(store.rename_project(actor, id, "Application").await);
        assert!(matches!(r, Refusal::Conflict(_)), "{r:?}");
        let r = is_refused(store.rename_project(actor, Uuid::now_v7(), "x").await);
        assert_eq!(r, Refusal::NotFound("project"));
        assert_eq!(audit_count(&store).await, 2);
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn a_configured_project_is_not_renamed_here_and_a_dashboard_one_is(pool: PgPool) {
        let store = store(pool);
        let actor = ada(&store).await;
        let configured = store.upsert_project("app", "App").await.unwrap();
        let r = is_refused(store.rename_project(actor, configured, "Application").await);
        assert_eq!(r, Refusal::FromConfig("project"));
        assert_eq!(
            audit_count(&store).await,
            0,
            "a refused rename writes no event"
        );
        let app = store
            .admin_projects()
            .await
            .unwrap()
            .into_iter()
            .find(|p| p.slug == "app")
            .unwrap();
        assert_eq!(app.name, "App", "the name is unchanged");

        let mobile = store
            .create_project(actor, "mobile", "Mobile")
            .await
            .unwrap();
        store
            .rename_project(actor, mobile, "Mobile QA")
            .await
            .unwrap();
        let trail = audit_of(&store).await;
        assert_eq!(trail.last().map(|t| t.0.as_str()), Some("project.rename"));
        assert_eq!(
            trail.last().map(|t| t.2.clone()),
            Some(json!({"slug": "mobile", "name": "Mobile QA"}))
        );
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn a_configured_project_the_configuration_drops_goes_to_the_dashboard(pool: PgPool) {
        let store = store(pool);
        let actor = ada(&store).await;
        store.upsert_project("kept", "Kept").await.unwrap();
        store.upsert_project("dropped", "Dropped").await.unwrap();
        store
            .release_unconfigured_projects(&["kept".to_owned()])
            .await
            .unwrap();
        let source = |slug: &'static str| {
            let pool = store.pool().clone();
            async move {
                sqlx::query_scalar::<_, String>("SELECT source FROM projects WHERE slug = $1")
                    .bind(slug)
                    .fetch_one(&pool)
                    .await
                    .unwrap()
            }
        };
        assert_eq!(
            source("kept").await,
            "config",
            "still listed: still configured"
        );
        assert_eq!(source("dropped").await, "dashboard");

        // Released, its name stays, and the dashboard may rename it now.
        let dropped = store.project_id("dropped").await.unwrap().unwrap();
        store
            .rename_project(actor, dropped, "Dropped Later")
            .await
            .unwrap();
        let row = store
            .admin_projects()
            .await
            .unwrap()
            .into_iter()
            .find(|p| p.slug == "dropped")
            .unwrap();
        assert_eq!(row.name, "Dropped Later");
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn an_api_key_is_made_once_with_its_secret_and_no_secret_in_the_trail(pool: PgPool) {
        let store = store(pool);
        let actor = ada(&store).await;
        let app = store.upsert_project("app", "App").await.unwrap();
        let expires = OffsetDateTime::now_utc() + Duration::days(30);
        let (id, secret) = store
            .create_api_key_as(actor, app, "ci-main", TrustLevel::Trusted, expires)
            .await
            .unwrap();
        assert!(store.authenticate(secret.expose()).await.unwrap().is_some());
        let trail = audit_of(&store).await;
        let create = trail.iter().find(|e| e.0 == "api_key.create").unwrap();
        assert_eq!(create.2["name"], "ci-main");
        assert_eq!(create.2["prefix"], &secret.expose()[..12]);
        let raw = sqlx::query_scalar!(
            r#"SELECT details::text AS "d!" FROM audit_events WHERE action = 'api_key.create'"#
        )
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert!(
            !raw.contains(secret.expose()),
            "the secret never reaches the trail"
        );
        let by = sqlx::query_scalar!("SELECT created_by FROM api_keys WHERE id = $1", id)
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert!(by.is_some(), "who made the key is recorded");

        let past = OffsetDateTime::now_utc() - Duration::minutes(1);
        let r = is_refused(
            store
                .create_api_key_as(actor, app, "late", TrustLevel::Trusted, past)
                .await,
        );
        assert!(matches!(r, Refusal::Invalid(_)), "{r:?}");
        assert_eq!(audit_count(&store).await, 1);
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn revoking_a_key_stops_it_and_its_tokens_once(pool: PgPool) {
        let store = store(pool);
        let actor = ada(&store).await;
        let app = store.upsert_project("app", "App").await.unwrap();
        let expires = OffsetDateTime::now_utc() + Duration::days(30);
        let (id, key) = store
            .create_api_key_as(actor, app, "ci", TrustLevel::Trusted, expires)
            .await
            .unwrap();
        let (_, token) = store
            .mint_agent_token(
                app,
                TokenSource::ApiKey(id),
                TrustLevel::Trusted,
                &CiClaims::default(),
                OffsetDateTime::now_utc() + Duration::hours(2),
            )
            .await
            .unwrap();
        assert!(store.authenticate(token.expose()).await.unwrap().is_some());

        store.revoke_api_key_as(actor, id).await.unwrap();
        assert!(store.authenticate(key.expose()).await.unwrap().is_none());
        assert!(
            store.authenticate(token.expose()).await.unwrap().is_none(),
            "the token stops with its key"
        );
        let trail = audit_of(&store).await;
        let revoke = trail.iter().find(|e| e.0 == "api_key.revoke").unwrap();
        assert_eq!(revoke.1["revoked"], false);
        assert_eq!(revoke.2["revoked"], true);
        assert_eq!(revoke.1["name"], "ci");

        let r = is_refused(store.revoke_api_key_as(actor, id).await);
        assert!(matches!(r, Refusal::Conflict(_)), "{r:?}");
        let r = is_refused(store.expire_api_key_as(actor, id, expires).await);
        assert!(
            matches!(r, Refusal::Conflict(_)),
            "a revoked key takes no expiry: {r:?}"
        );
        let r = is_refused(store.revoke_api_key_as(actor, Uuid::now_v7()).await);
        assert_eq!(r, Refusal::NotFound("API key"));
        assert_eq!(audit_count(&store).await, 2, "one create and one revoke");
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn a_key_expires_at_the_new_time_and_the_change_is_audited(pool: PgPool) {
        let store = store(pool);
        let actor = ada(&store).await;
        let app = store.upsert_project("app", "App").await.unwrap();
        let (id, key) = store
            .create_api_key_as(
                actor,
                app,
                "ci",
                TrustLevel::Trusted,
                OffsetDateTime::now_utc() + Duration::days(90),
            )
            .await
            .unwrap();
        // Authentication judges `expires_at` against the database's clock (`expires_at > now()`), so
        // the expiry is taken from that clock too. A host instant can sit ahead of the database's
        // (a Docker VM clock drifts), and then the key would still be valid when it "expires now".
        let db_now: OffsetDateTime = sqlx::query_scalar("SELECT now()")
            .fetch_one(store.pool())
            .await
            .unwrap();
        store.expire_api_key_as(actor, id, db_now).await.unwrap();
        assert!(
            store.authenticate(key.expose()).await.unwrap().is_none(),
            "expired now, by the database's clock"
        );
        let trail = audit_of(&store).await;
        let e = trail.iter().find(|e| e.0 == "api_key.expire").unwrap();
        assert!(e.1["expires_at"].is_string() && e.2["expires_at"].is_string());
        assert_ne!(e.1["expires_at"], e.2["expires_at"]);
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn a_role_binding_is_added_takes_effect_and_is_removed(pool: PgPool) {
        let store = store(pool);
        let actor = ada(&store).await;
        let app = store.upsert_project("app", "App").await.unwrap();
        let grace = store
            .upsert_user("https://idp.test", "grace", None, Some("Grace"))
            .await
            .unwrap();
        // Grace has a live session: a new role must reach her now, so it ends.
        let stale = store
            .create_session(
                Some(grace),
                &json!({"grants": []}),
                OffsetDateTime::now_utc() + Duration::hours(1),
            )
            .await
            .unwrap();
        let id = store
            .add_role_binding(actor, Some(app), grace, Role::Maintainer)
            .await
            .unwrap();
        assert!(
            store
                .session(stale.expose(), Duration::hours(1))
                .await
                .unwrap()
                .is_none()
        );
        let grants = store
            .grants_for(grace, "https://idp.test", &[])
            .await
            .unwrap();
        assert_eq!(
            grants,
            vec![Grant {
                project: Some(app),
                role: Role::Maintainer
            }]
        );
        let trail = audit_of(&store).await;
        assert_eq!(trail[0].0, "role_binding.add");
        assert_eq!(
            trail[0].2,
            json!({"project": "app", "person": "Grace", "role": "maintainer"})
        );

        let r = is_refused(
            store
                .add_role_binding(actor, Some(app), grace, Role::Admin)
                .await,
        );
        assert!(
            matches!(r, Refusal::Conflict(_)),
            "one role per project: {r:?}"
        );

        store.remove_role_binding(actor, id).await.unwrap();
        assert_eq!(
            store
                .grants_for(grace, "https://idp.test", &[])
                .await
                .unwrap()
                .len(),
            0
        );
        let trail = audit_of(&store).await;
        assert_eq!(trail[1].0, "role_binding.remove");
        assert_eq!(
            trail[1].1,
            json!({"project": "app", "person": "Grace", "role": "maintainer"})
        );
        assert_eq!(trail[1].2, Value::Null);
        assert_eq!(audit_count(&store).await, 2);
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn the_last_global_administrator_cannot_be_removed(pool: PgPool) {
        let store = store(pool);
        let actor = ada(&store).await;
        let AuditActor::User(ada_id) = actor else {
            unreachable!("the test's actor is a person")
        };
        let only = store
            .add_role_binding(actor, None, ada_id, Role::Admin)
            .await
            .unwrap();
        let r = is_refused(store.remove_role_binding(actor, only).await);
        assert_eq!(r, Refusal::LastGlobalAdmin);
        assert_eq!(
            store
                .grants_for(ada_id, "https://idp.test", &[])
                .await
                .unwrap()
                .len(),
            1,
            "still an administrator"
        );
        assert_eq!(
            audit_count(&store).await,
            1,
            "the refused removal writes nothing"
        );

        // A second global administrator makes the first one removable.
        let grace = store
            .upsert_user("https://idp.test", "grace", None, None)
            .await
            .unwrap();
        let second = store
            .add_role_binding(actor, None, grace, Role::Admin)
            .await
            .unwrap();
        store.remove_role_binding(actor, only).await.unwrap();
        // ...and now the second is the last one.
        let r = is_refused(store.remove_role_binding(actor, second).await);
        assert_eq!(r, Refusal::LastGlobalAdmin);
        // A project admin is not a global administrator: it doesn't count towards the last one.
        let app = store.upsert_project("app", "App").await.unwrap();
        let p = store
            .add_role_binding(actor, Some(app), ada_id, Role::Admin)
            .await
            .unwrap();
        let r = is_refused(store.remove_role_binding(actor, second).await);
        assert_eq!(r, Refusal::LastGlobalAdmin);
        store.remove_role_binding(actor, p).await.unwrap();
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn dashboard_group_mappings_are_edited_and_removed_and_config_ones_are_locked(
        pool: PgPool,
    ) {
        let store = store(pool);
        let actor = ada(&store).await;
        let app = store.upsert_project("app", "App").await.unwrap();
        // A configured mapping, and a dashboard one for another group.
        store
            .sync_group_mappings(
                ISSUER,
                &[GroupMapping {
                    project: Some(app),
                    group: "qa".into(),
                    role: Role::Viewer,
                }],
            )
            .await
            .unwrap();
        let id = store
            .add_group_mapping(actor, ISSUER, Some(app), "mobile-leads", Role::Viewer)
            .await
            .unwrap();
        store
            .set_group_mapping_role(actor, id, Role::Maintainer)
            .await
            .unwrap();
        let trail = audit_of(&store).await;
        let edit = trail.iter().find(|e| e.0 == "group_mapping.edit").unwrap();
        assert_eq!(edit.1["role"], "viewer");
        assert_eq!(edit.2["role"], "maintainer");

        // A dashboard group that the configuration already maps is refused, and says what to change.
        let r = is_refused(
            store
                .add_group_mapping(actor, ISSUER, Some(app), "qa", Role::Admin)
                .await,
        );
        assert!(
            matches!(&r, Refusal::Conflict(m) if m.contains("configuration")),
            "{r:?}"
        );

        // The configured row is locked: no role change, no removal.
        let config_id =
            sqlx::query_scalar!("SELECT id FROM group_mappings WHERE group_claim = 'qa'")
                .fetch_one(store.pool())
                .await
                .unwrap();
        let r = is_refused(
            store
                .set_group_mapping_role(actor, config_id, Role::Admin)
                .await,
        );
        assert_eq!(r, Refusal::FromConfig("group mapping"));
        let r = is_refused(store.remove_group_mapping(actor, config_id).await);
        assert_eq!(r, Refusal::FromConfig("group mapping"));

        store.remove_group_mapping(actor, id).await.unwrap();
        let trail = audit_of(&store).await;
        assert_eq!(trail.last().unwrap().0, "group_mapping.remove");
        assert_eq!(trail.last().unwrap().1["group"], "mobile-leads");
        assert_eq!(
            audit_count(&store).await,
            i64::try_from(trail.len()).unwrap()
        );
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn the_start_up_sync_replaces_only_configured_rows(pool: PgPool) {
        let store = store(pool);
        let actor = ada(&store).await;
        let app = store.upsert_project("app", "App").await.unwrap();
        store
            .sync_group_mappings(
                ISSUER,
                &[GroupMapping {
                    project: Some(app),
                    group: "old-config".into(),
                    role: Role::Viewer,
                }],
            )
            .await
            .unwrap();
        store
            .add_group_mapping(actor, ISSUER, None, "dashboard-group", Role::Maintainer)
            .await
            .unwrap();
        // The configuration changes: the old configured group goes, a new one comes.
        store
            .sync_group_mappings(
                ISSUER,
                &[GroupMapping {
                    project: Some(app),
                    group: "new-config".into(),
                    role: Role::Admin,
                }],
            )
            .await
            .unwrap();
        let groups: Vec<(String, String)> =
            sqlx::query!("SELECT group_claim, source FROM group_mappings ORDER BY group_claim")
                .fetch_all(store.pool())
                .await
                .unwrap()
                .into_iter()
                .map(|r| (r.group_claim, r.source))
                .collect();
        assert_eq!(
            groups,
            [
                ("dashboard-group".into(), "dashboard".into()),
                ("new-config".into(), "config".into()),
            ]
        );

        // A configured mapping that a dashboard row already holds: the sync refuses, and stores nothing.
        let clash = store
            .sync_group_mappings(
                ISSUER,
                &[GroupMapping {
                    project: None,
                    group: "dashboard-group".into(),
                    role: Role::Viewer,
                }],
            )
            .await;
        assert!(matches!(clash, Err(Error::Conflict(m)) if m.contains("dashboard-group")));
        let still: i64 = sqlx::query_scalar!(r#"SELECT count(*) AS "n!" FROM group_mappings"#)
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(still, 2, "the refused sync left the rows as they were");
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn a_trust_policy_is_added_edited_revoked_and_removed(pool: PgPool) {
        let store = store(pool);
        let actor = ada(&store).await;
        let app = store.upsert_project("app", "App").await.unwrap();
        let id = store
            .add_trust_policy(actor, app, &policy("release"))
            .await
            .unwrap();
        let listed = store.admin_trust_policies().await.unwrap();
        assert_eq!(listed[0].source, "dashboard");
        assert_eq!(listed[0].environment.as_deref(), Some("prod"));

        // A second add with the same name is refused.
        let r = is_refused(store.add_trust_policy(actor, app, &policy("release")).await);
        assert!(matches!(r, Refusal::Conflict(_)), "{r:?}");

        let mut changed = policy("release-2");
        changed.policy.environment = None;
        store.edit_trust_policy(actor, id, &changed).await.unwrap();
        let listed = store.admin_trust_policies().await.unwrap();
        assert_eq!(listed[0].name, "release-2");
        assert_eq!(listed[0].environment, None);

        store.revoke_trust_policy(actor, id).await.unwrap();
        assert!(
            store.admin_trust_policies().await.unwrap()[0]
                .revoked_at
                .is_some()
        );
        let r = is_refused(store.revoke_trust_policy(actor, id).await);
        assert!(matches!(r, Refusal::Conflict(_)), "{r:?}");

        store.remove_trust_policy(actor, id).await.unwrap();
        assert_eq!(store.admin_trust_policies().await.unwrap().len(), 0);

        let trail = audit_of(&store).await;
        let names: Vec<&str> = trail.iter().map(|e| e.0.as_str()).collect();
        assert_eq!(
            names,
            [
                "trust_policy.add",
                "trust_policy.edit",
                "trust_policy.revoke",
                "trust_policy.remove"
            ]
        );
        assert_eq!(trail[0].2["name"], "release");
        assert_eq!(trail[1].1["name"], "release");
        assert_eq!(trail[1].2["name"], "release-2");
        assert_eq!(trail[2].2["revoked"], true);
        assert_eq!(trail[3].1["name"], "release-2");
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn a_revoked_trust_policy_ends_the_tokens_it_minted(pool: PgPool) {
        let store = store(pool);
        let actor = ada(&store).await;
        let app = store.upsert_project("app", "App").await.unwrap();
        let id = store
            .add_trust_policy(actor, app, &policy("release"))
            .await
            .unwrap();
        let claims = CiClaims {
            run_id: Some("9".into()),
            ..CiClaims::default()
        };
        let (_, token) = store
            .mint_agent_token(
                app,
                TokenSource::TrustPolicy(id),
                TrustLevel::Trusted,
                &claims,
                OffsetDateTime::now_utc() + Duration::hours(2),
            )
            .await
            .unwrap();
        assert!(store.authenticate(token.expose()).await.unwrap().is_some());
        store.revoke_trust_policy(actor, id).await.unwrap();
        assert!(store.authenticate(token.expose()).await.unwrap().is_none());

        // Removal keeps the token rows: the policy link is cleared, and the token stays dead.
        let (_, token2) = store
            .mint_agent_token(
                app,
                TokenSource::TrustPolicy(id),
                TrustLevel::Trusted,
                &claims,
                OffsetDateTime::now_utc() + Duration::hours(2),
            )
            .await
            .unwrap();
        store.remove_trust_policy(actor, id).await.unwrap();
        assert!(store.authenticate(token2.expose()).await.unwrap().is_none());
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn the_configuration_locks_its_trust_policies_and_the_sync_leaves_dashboard_ones(
        pool: PgPool,
    ) {
        let store = store(pool);
        let actor = ada(&store).await;
        let app = store.upsert_project("app", "App").await.unwrap();
        store
            .sync_trust_policies(app, &[policy("from-config")])
            .await
            .unwrap();
        let dash = store
            .add_trust_policy(actor, app, &policy("from-dash"))
            .await
            .unwrap();

        // A dashboard row can't take a configured name, and a configured row can't be changed here.
        let r = is_refused(
            store
                .add_trust_policy(actor, app, &policy("from-config"))
                .await,
        );
        assert!(
            matches!(&r, Refusal::Conflict(m) if m.contains("configuration")),
            "{r:?}"
        );
        let config_id = sqlx::query_scalar!(
            "SELECT id FROM publisher_trust_policies WHERE name = 'from-config'"
        )
        .fetch_one(store.pool())
        .await
        .unwrap();
        let r = is_refused(
            store
                .edit_trust_policy(actor, config_id, &policy("x"))
                .await,
        );
        assert_eq!(r, Refusal::FromConfig("trust policy"));
        let r = is_refused(store.revoke_trust_policy(actor, config_id).await);
        assert_eq!(r, Refusal::FromConfig("trust policy"));
        let r = is_refused(store.remove_trust_policy(actor, config_id).await);
        assert_eq!(r, Refusal::FromConfig("trust policy"));

        // A sync that drops the configured policy leaves the dashboard one alone.
        store.sync_trust_policies(app, &[]).await.unwrap();
        let listed = store.admin_trust_policies().await.unwrap();
        let names: Vec<(&str, bool)> = listed
            .iter()
            .map(|t| (t.name.as_str(), t.revoked_at.is_some()))
            .collect();
        assert_eq!(names, [("from-config", true), ("from-dash", false)]);

        // A configured name that a dashboard row holds: the sync refuses and stores nothing.
        let clash = store.sync_trust_policies(app, &[policy("from-dash")]).await;
        assert!(matches!(clash, Err(Error::Conflict(m)) if m.contains("from-dash")));
        let still_live = store.admin_trust_policies().await.unwrap();
        assert!(
            still_live
                .iter()
                .any(|t| t.id == dash && t.revoked_at.is_none())
        );
    }
}

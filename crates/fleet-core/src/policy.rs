// SPDX-License-Identifier: AGPL-3.0-only
//! Authorization policy (PLAN §7.5, research 05 §13): a hand-rolled
//! `authorize(principal, action, resource) -> Decision`, deny by default.
//!
//! Three human roles ([`Role`]: viewer, maintainer, admin) are granted per project or for every
//! project ([`Grant`]); the effective role on a project is the highest applicable grant. A
//! publisher (a CI credential) is not a human role: it may only [`Action::Ingest`] and
//! [`Action::UploadArtifact`] into its own project and can never read anything (spec §10).
//! Every action belongs to exactly one kind of [`Resource`]; a mismatch is a denial, so a global
//! action can't be satisfied by a project grant.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A human role, ordered by privilege.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Reads history, the live view and artifacts.
    Viewer,
    /// A viewer who also manages project settings, credentials and retention.
    Maintainer,
    /// Everything, including roles; with a global grant, installation-wide settings and audit.
    Admin,
}

/// A role on one project, or on every project when `project` is `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Grant {
    /// The project, or `None` for all of them.
    pub project: Option<Uuid>,
    /// The role held there.
    pub role: Role,
}

/// Who is asking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Principal {
    /// A signed-in person (or a configured viewer token) and what they were granted.
    User {
        /// Every grant; the highest applicable one wins.
        grants: Vec<Grant>,
    },
    /// A CI credential, bound to exactly one project and write-only.
    Publisher {
        /// The only project it may write to.
        project: Uuid,
    },
}

/// What is being done.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    /// List the projects one may see (a [`Resource::Global`] action; the list is then filtered).
    ListProjects,
    /// Read runs, flows, devices and telemetry.
    ReadRuns,
    /// Read and download artifacts.
    ReadArtifacts,
    /// Change project settings and flow tags.
    ManageProject,
    /// Mark a failure fingerprint known (with its issue) or resolved, or clear that.
    TriageFailures,
    /// Create and revoke a project's publisher credentials.
    ManageCredentials,
    /// Change retention within limits.
    ManageRetention,
    /// Change who holds which role on the project.
    ManageRoles,
    /// Read the audit log (a [`Resource::Global`] action).
    ViewAudit,
    /// Read installation-wide administration: projects, publisher trust, API keys, role mappings,
    /// retention, artifact and metric policy, agent compatibility and ingestion health (Screen G;
    /// a [`Resource::Global`] action).
    ViewAdministration,
    /// Change installation-wide administration: create projects, rename them, issue and revoke API
    /// keys, bind roles and group mappings, and manage publisher trust (Screen G's writes; a
    /// [`Resource::Global`] action).
    ManageAdministration,
    /// Change installation-wide settings (a [`Resource::Global`] action).
    ManageGlobalSettings,
    /// Publish events and telemetry (publishers only).
    Ingest,
    /// Upload artifacts of the caller's own run (publishers only).
    UploadArtifact,
}

/// What is acted on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Resource {
    /// One project.
    Project(Uuid),
    /// The installation as a whole.
    Global,
}

/// Why a request was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Denied {
    /// The caller has no grant covering this resource (or isn't the kind of caller the action
    /// is for).
    NoRole,
    /// The caller has a role here, but a lower one than the action needs.
    NeedsRole(Role),
    /// The action doesn't apply to this kind of resource.
    WrongResource,
}

/// The outcome of [`authorize`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use = "a decision that is not checked authorizes nothing"]
pub enum Decision {
    /// Go ahead.
    Allow,
    /// Refused, and why.
    Deny(Denied),
}

/// What an action requires.
enum Needs {
    /// A human role on a project.
    Project(Role),
    /// A global grant of a human role.
    Global(Role),
    /// Any grant at all.
    AnyGrant,
    /// A publisher credential for the project.
    Publisher,
}

impl Action {
    fn needs(self) -> Needs {
        match self {
            Self::ListProjects => Needs::AnyGrant,
            Self::ReadRuns | Self::ReadArtifacts => Needs::Project(Role::Viewer),
            Self::ManageProject
            | Self::TriageFailures
            | Self::ManageCredentials
            | Self::ManageRetention => Needs::Project(Role::Maintainer),
            Self::ManageRoles => Needs::Project(Role::Admin),
            Self::ViewAudit
            | Self::ViewAdministration
            | Self::ManageAdministration
            | Self::ManageGlobalSettings => Needs::Global(Role::Admin),
            Self::Ingest | Self::UploadArtifact => Needs::Publisher,
        }
    }
}

/// Decides whether `principal` may do `action` to `resource`. Deny by default.
pub fn authorize(principal: &Principal, action: Action, resource: Resource) -> Decision {
    let needs = action.needs();
    let right_kind = matches!(
        (&needs, resource),
        (Needs::Project(_) | Needs::Publisher, Resource::Project(_))
            | (Needs::Global(_) | Needs::AnyGrant, Resource::Global)
    );
    if !right_kind {
        return Decision::Deny(Denied::WrongResource);
    }
    match (principal, needs, resource) {
        (Principal::User { grants }, Needs::AnyGrant, _) => {
            if grants.is_empty() {
                Decision::Deny(Denied::NoRole)
            } else {
                Decision::Allow
            }
        }
        (Principal::User { grants }, Needs::Project(min), Resource::Project(id)) => {
            let best = grants
                .iter()
                .filter(|g| g.project.is_none() || g.project == Some(id))
                .map(|g| g.role)
                .max();
            check(best, min)
        }
        (Principal::User { grants }, Needs::Global(min), _) => check(
            grants
                .iter()
                .filter(|g| g.project.is_none())
                .map(|g| g.role)
                .max(),
            min,
        ),
        (Principal::Publisher { project }, Needs::Publisher, Resource::Project(id))
            if *project == id =>
        {
            Decision::Allow
        }
        _ => Decision::Deny(Denied::NoRole),
    }
}

fn check(best: Option<Role>, min: Role) -> Decision {
    match best {
        Some(role) if role >= min => Decision::Allow,
        Some(_) => Decision::Deny(Denied::NeedsRole(min)),
        None => Decision::Deny(Denied::NoRole),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: Uuid = Uuid::from_u128(1);
    const B: Uuid = Uuid::from_u128(2);

    fn user(grants: &[(Option<Uuid>, Role)]) -> Principal {
        Principal::User {
            grants: grants
                .iter()
                .map(|&(project, role)| Grant { project, role })
                .collect(),
        }
    }

    fn allowed(p: &Principal, a: Action, r: Resource) -> bool {
        authorize(p, a, r) == Decision::Allow
    }

    /// The role × action table of research 05 §13, written out literally.
    #[test]
    fn every_role_and_action_on_a_project() {
        use Action::{
            ManageCredentials, ManageProject, ManageRetention, ManageRoles, ReadArtifacts,
            ReadRuns, TriageFailures,
        };
        let project_actions = [
            ReadRuns,
            ReadArtifacts,
            ManageProject,
            TriageFailures,
            ManageCredentials,
            ManageRetention,
            ManageRoles,
        ];
        let table: [(Role, &[Action]); 3] = [
            (Role::Viewer, &[ReadRuns, ReadArtifacts]),
            (
                Role::Maintainer,
                &[
                    ReadRuns,
                    ReadArtifacts,
                    ManageProject,
                    TriageFailures,
                    ManageCredentials,
                    ManageRetention,
                ],
            ),
            (Role::Admin, &project_actions),
        ];
        for (role, may) in table {
            let p = user(&[(Some(A), role)]);
            for action in project_actions {
                assert_eq!(
                    allowed(&p, action, Resource::Project(A)),
                    may.contains(&action),
                    "{role:?} on {action:?}"
                );
            }
        }
    }

    #[test]
    fn global_actions_need_a_global_admin_grant() {
        for action in [
            Action::ViewAudit,
            Action::ViewAdministration,
            Action::ManageAdministration,
            Action::ManageGlobalSettings,
        ] {
            assert!(allowed(
                &user(&[(None, Role::Admin)]),
                action,
                Resource::Global
            ));
            // Admin of one project is not an admin of the installation.
            assert!(!allowed(
                &user(&[(Some(A), Role::Admin)]),
                action,
                Resource::Global
            ));
            assert!(!allowed(
                &user(&[(None, Role::Maintainer)]),
                action,
                Resource::Global
            ));
        }
    }

    #[test]
    fn listing_projects_needs_any_grant_and_nothing_else() {
        let list = |p: &Principal| allowed(p, Action::ListProjects, Resource::Global);
        assert!(list(&user(&[(Some(A), Role::Viewer)])));
        assert!(list(&user(&[(None, Role::Viewer)])));
        assert!(!list(&user(&[])));
        assert!(!list(&Principal::Publisher { project: A }));
    }

    #[test]
    fn a_project_grant_does_not_reach_another_project() {
        let p = user(&[(Some(A), Role::Admin)]);
        assert_eq!(
            authorize(&p, Action::ReadRuns, Resource::Project(B)),
            Decision::Deny(Denied::NoRole)
        );
    }

    #[test]
    fn a_global_grant_reaches_every_project_and_the_highest_role_wins() {
        let p = user(&[(None, Role::Viewer), (Some(A), Role::Maintainer)]);
        assert!(allowed(&p, Action::ManageProject, Resource::Project(A)));
        assert!(!allowed(&p, Action::ManageProject, Resource::Project(B)));
        assert!(allowed(&p, Action::ReadRuns, Resource::Project(B)));
    }

    #[test]
    fn a_denial_says_which_role_was_missing() {
        let p = user(&[(Some(A), Role::Viewer)]);
        assert_eq!(
            authorize(&p, Action::ManageCredentials, Resource::Project(A)),
            Decision::Deny(Denied::NeedsRole(Role::Maintainer))
        );
    }

    #[test]
    fn a_publisher_writes_to_its_own_project_and_never_reads() {
        let p = Principal::Publisher { project: A };
        for action in [Action::Ingest, Action::UploadArtifact] {
            assert!(allowed(&p, action, Resource::Project(A)));
            assert!(!allowed(&p, action, Resource::Project(B)));
        }
        for action in [
            Action::ReadRuns,
            Action::ReadArtifacts,
            Action::ManageProject,
            Action::TriageFailures,
            Action::ManageCredentials,
            Action::ManageRetention,
            Action::ManageRoles,
        ] {
            assert!(!allowed(&p, action, Resource::Project(A)), "{action:?}");
        }
        assert!(!allowed(&p, Action::ViewAudit, Resource::Global));
        assert!(!allowed(&p, Action::ViewAdministration, Resource::Global));
        assert!(!allowed(&p, Action::ManageAdministration, Resource::Global));
    }

    #[test]
    fn a_user_never_publishes() {
        let p = user(&[(None, Role::Admin)]);
        for action in [Action::Ingest, Action::UploadArtifact] {
            assert!(!allowed(&p, action, Resource::Project(A)));
        }
    }

    #[test]
    fn an_action_on_the_wrong_kind_of_resource_is_denied() {
        let p = user(&[(None, Role::Admin)]);
        assert_eq!(
            authorize(&p, Action::ViewAudit, Resource::Project(A)),
            Decision::Deny(Denied::WrongResource)
        );
        assert_eq!(
            authorize(&p, Action::ReadRuns, Resource::Global),
            Decision::Deny(Denied::WrongResource)
        );
    }

    #[test]
    fn roles_round_trip_as_lowercase_names() {
        let g = Grant {
            project: None,
            role: Role::Maintainer,
        };
        assert_eq!(
            serde_json::to_string(&g).unwrap(),
            r#"{"project":null,"role":"maintainer"}"#
        );
    }
}

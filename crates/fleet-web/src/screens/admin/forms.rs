// SPDX-License-Identifier: AGPL-3.0-only
//! Screen G's forms (issue #139). Each is a plain HTML form that posts to an administration route,
//! so a change needs no script of the dashboard's own: the browser sends the form, and the server
//! answers with a redirect back to its section (or, for a new API key, the one page that shows the
//! secret). Every form carries the session's CSRF token in a hidden `csrf` field; every field is
//! labelled by the element around it; a row's edit is a `<details>` disclosure, which needs no script.
//!
//! A configured row (`source` is `config`) has no form at all: it says where it is set.

use fleet_protocol::rest::admin::{
    AdminApiKeyView, AdminGroupMappingView, AdminPersonView, AdminProjectView,
    AdminRoleBindingView, AdminTrustPolicyView,
};
use leptos::prelude::*;

use super::model::{PR_POLICY_OPTIONS, ROLE_OPTIONS, TRUST_OPTIONS, is_global_admin};
use crate::app::Auth;

/// The class of a text or number field's control.
const CONTROL: &str =
    "min-h-8 rounded border border-line-strong bg-transparent px-2 text-sm text-fg";
/// The class of a primary submit button.
const SUBMIT: &str = "inline-flex min-h-8 items-center rounded bg-primary px-3 py-1.5 text-sm font-medium text-primary-fg hover:opacity-90";
/// The class of a secondary action, which posts a row's change.
const ACTION: &str = "inline-flex min-h-8 items-center rounded border border-line-strong px-3 py-1.5 text-sm font-medium text-fg hover:bg-band";

/// Shows `render` once the session of a global administrator has loaded. A form built before that
/// would post an empty token, so none is shown, and none can be sent, until the token is known; and
/// a person who may not change Screen G is shown no form (the server refuses them anyway).
fn signed_in_only(render: impl Fn() -> AnyView + Send + Sync + 'static) -> AnyView {
    let auth = expect_context::<Auth>();
    let may_change = move || {
        auth.session
            .with(|s| s.as_ref().is_some_and(is_global_admin))
    };
    (move || may_change().then(&render)).into_any()
}

/// A form that posts to `action`, with its heading as its accessible name, its fields (built by
/// `fields`), and a submit button. The session's CSRF token goes in a hidden `csrf` field.
#[must_use]
pub fn post(
    action: String,
    title: &'static str,
    submit: &'static str,
    fields: impl Fn() -> AnyView + Send + Sync + 'static,
) -> AnyView {
    let auth = expect_context::<Auth>();
    signed_in_only(move || {
        let token = move || auth.session.get().map(|s| s.csrf_token).unwrap_or_default();
        view! {
            <form method="post" action=action.clone() aria-label=title class="flex flex-wrap items-end gap-3 pt-3">
                <input type="hidden" name="csrf" prop:value=token />
                {fields()}
                <button type="submit" class=SUBMIT>{submit}</button>
            </form>
        }
        .into_any()
    })
}

/// A form that is only a button, for a row's action (revoke, remove).
#[must_use]
pub fn action(action: String, title: &'static str, label: &'static str) -> AnyView {
    let auth = expect_context::<Auth>();
    signed_in_only(move || {
        let token = move || auth.session.get().map(|s| s.csrf_token).unwrap_or_default();
        view! {
            <form method="post" action=action.clone() aria-label=title class="inline">
                <input type="hidden" name="csrf" prop:value=token />
                <button type="submit" class=ACTION>{label}</button>
            </form>
        }
        .into_any()
    })
}

/// A labelled text field. `required` fields say so to the browser too.
#[must_use]
pub fn text(
    label: &'static str,
    name: &'static str,
    value: String,
    placeholder: &'static str,
    required: bool,
) -> AnyView {
    view! {
        <label class="flex min-w-40 flex-col gap-1 text-sm">
            <span class="text-muted">{label}</span>
            <input type="text" name=name value=value placeholder=placeholder required=required class=CONTROL />
        </label>
    }
    .into_any()
}

/// A labelled whole-number field between `min` and `max`.
#[must_use]
pub fn number(label: &'static str, name: &'static str, value: u32, min: u32, max: u32) -> AnyView {
    view! {
        <label class="flex min-w-32 flex-col gap-1 text-sm">
            <span class="text-muted">{label}</span>
            <input type="number" name=name value=value.to_string() min=min.to_string() max=max.to_string() required=true class=CONTROL />
        </label>
    }
    .into_any()
}

/// A labelled choice. The option whose value is `current` is selected; a `""` value is "every
/// project" for the scope fields, and reads as such.
#[must_use]
pub fn select(
    label: &'static str,
    name: &'static str,
    options: Vec<(String, String)>,
    current: &str,
) -> AnyView {
    let rows = options
        .into_iter()
        .map(|(value, text)| {
            let selected = value == current;
            view! { <option value=value selected=selected>{text}</option> }
        })
        .collect_view();
    view! {
        <label class="flex min-w-40 flex-col gap-1 text-sm">
            <span class="text-muted">{label}</span>
            <select name=name class=CONTROL>{rows}</select>
        </label>
    }
    .into_any()
}

/// A disclosure with its summary: a row's edit, closed until it is opened. No script.
#[must_use]
pub fn details(summary: &'static str, body: AnyView) -> AnyView {
    view! {
        <details class="text-sm">
            <summary class="cursor-pointer text-primary underline">{summary}</summary>
            {body}
        </details>
    }
    .into_any()
}

/// The actions of one row, side by side.
#[must_use]
pub fn row_actions(parts: Vec<AnyView>) -> AnyView {
    view! { <div class="flex flex-wrap items-start gap-3">{parts.into_iter().collect_view()}</div> }
        .into_any()
}

/// The text that says a row is set in the configuration, where the dashboard can't change it.
#[must_use]
pub fn locked(what: &'static str) -> AnyView {
    view! { <span class="note">{format!("Set in the configuration: change the {what} there")}</span> }.into_any()
}

/// The project choices, by slug. `every` adds the "every project" choice (scope fields).
#[must_use]
pub fn project_options(projects: &[AdminProjectView], every: bool) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if every {
        out.push((String::new(), "Every project".to_owned()));
    }
    out.extend(projects.iter().map(|p| (p.slug.clone(), p.slug.clone())));
    out
}

/// The role choices.
#[must_use]
pub fn role_options() -> Vec<(String, String)> {
    ROLE_OPTIONS
        .iter()
        .map(|r| ((*r).to_owned(), (*r).to_owned()))
        .collect()
}

/// Adds a project: its slug and display name.
#[must_use]
pub fn add_project() -> AnyView {
    post(
        "/api/v1/admin/projects".to_owned(),
        "Add a project",
        "Create project",
        || {
            view! {
                {text("Slug", "slug", String::new(), "app-mobile", true)}
                {text("Name", "name", String::new(), "Mobile app", true)}
            }
            .into_any()
        },
    )
}

/// Renames a project (its slug stays).
#[must_use]
pub fn rename_project(p: &AdminProjectView) -> AnyView {
    let current = p.name.clone();
    details(
        "Rename",
        post(
            format!("/api/v1/admin/projects/{}/rename", p.slug),
            "Rename the project",
            "Save name",
            move || text("New name", "name", current.clone(), "", true),
        ),
    )
}

/// The fields of a trust policy: the project to add it to (with `projects`), or the fields of the
/// policy `t`, prefilled, when it is given.
#[must_use]
pub fn policy_fields(projects: &[AdminProjectView], t: Option<&AdminTrustPolicyView>) -> AnyView {
    let v = |f: fn(&AdminTrustPolicyView) -> String| t.map(f).unwrap_or_default();
    let pr = t.map_or("restricted".to_owned(), |t| t.pr_policy.clone());
    let project_field = if t.is_none() {
        select("Project", "project", project_options(projects, false), "")
    } else {
        ().into_any()
    };
    view! {
        {project_field}
        {text("Name", "name", v(|t| t.name.clone()), "release", true)}
        {text("Issuer", "issuer", v(|t| t.issuer.clone()), "https://token.actions.githubusercontent.com", false)}
        {text("Audience", "audience", v(|t| t.audience.clone()), "fleet", false)}
        {text("Repository ID", "repository_id", v(|t| t.repository_id.clone()), "1001", true)}
        {text("Owner ID", "repository_owner_id", v(|t| t.repository_owner_id.clone()), "2002", true)}
        {text("Trusted refs", "trusted_refs", v(|t| t.trusted_refs.join(", ")), "refs/heads/main, refs/tags/*", false)}
        {text("Events", "allowed_events", v(|t| t.allowed_events.join(", ")), "push, schedule", false)}
        {text("Reusable workflow", "job_workflow_ref", v(|t| t.job_workflow_ref.clone().unwrap_or_default()), "", false)}
        {text("Environment", "environment", v(|t| t.environment.clone().unwrap_or_default()), "prod", false)}
        {select("Pull requests", "pr_policy", PR_POLICY_OPTIONS.iter().map(|p| ((*p).to_owned(), (*p).to_owned())).collect(), &pr)}
    }
    .into_any()
}

/// Adds a trust policy to a project.
#[must_use]
pub fn add_policy(projects: &[AdminProjectView]) -> AnyView {
    let projects = projects.to_vec();
    post(
        "/api/v1/admin/trust-policies".to_owned(),
        "Add a trust policy",
        "Add policy",
        move || policy_fields(&projects, None),
    )
}

/// What a trust policy row can do: edit, revoke and remove a dashboard row; a configured row is
/// locked.
#[must_use]
pub fn policy_actions(t: &AdminTrustPolicyView) -> AnyView {
    if t.source == "config" {
        return locked("trust policy");
    }
    let policy = t.clone();
    let edit = post(
        format!("/api/v1/admin/trust-policies/{}/edit", t.id),
        "Edit the trust policy",
        "Save policy",
        move || policy_fields(&[], Some(&policy)),
    );
    let mut parts = vec![details("Edit", edit)];
    if t.revoked_at.is_none() {
        parts.push(action(
            format!("/api/v1/admin/trust-policies/{}/revoke", t.id),
            "Revoke the trust policy",
            "Revoke",
        ));
    }
    parts.push(action(
        format!("/api/v1/admin/trust-policies/{}/remove", t.id),
        "Remove the trust policy",
        "Remove",
    ));
    row_actions(parts)
}

/// Creates an API key. Its secret comes back on the page the server answers with, once.
#[must_use]
pub fn add_key(projects: &[AdminProjectView]) -> AnyView {
    let projects = projects.to_vec();
    post(
        "/api/v1/admin/api-keys".to_owned(),
        "Create an API key",
        "Create key",
        move || {
            view! {
                {select("Project", "project", project_options(&projects, false), "")}
                {text("Name", "name", String::new(), "ci-main", true)}
                {select("Trust", "trust", TRUST_OPTIONS.iter().map(|t| ((*t).to_owned(), (*t).to_owned())).collect(), "trusted")}
                {number("Days until it stops working", "days", 90, 1, 3650)}
            }
            .into_any()
        },
    )
}

/// Expires or revokes an API key. Revoking also stops the tokens minted from it.
#[must_use]
pub fn key_actions(k: &AdminApiKeyView) -> AnyView {
    let expire = post(
        format!("/api/v1/admin/api-keys/{}/expire", k.id),
        "Set when the key stops working",
        "Set expiry",
        || number("Days from now (0 ends it now)", "days", 30, 0, 3650),
    );
    row_actions(vec![
        details("Expire", expire),
        action(
            format!("/api/v1/admin/api-keys/{}/revoke", k.id),
            "Revoke the API key",
            "Revoke",
        ),
    ])
}

/// Binds a role to a person, on a project or on every project.
#[must_use]
pub fn add_binding(projects: &[AdminProjectView], people: &[AdminPersonView]) -> AnyView {
    if people.is_empty() {
        return view! { <p class="note pt-3">"Nobody has signed in yet: a person can be given a role once they have."</p> }.into_any();
    }
    let projects = projects.to_vec();
    let people = people.to_vec();
    post(
        "/api/v1/admin/role-bindings".to_owned(),
        "Give a person a role",
        "Give role",
        move || {
            view! {
                {select("Person", "person", people.iter().map(|p| (p.id.to_string(), p.name.clone())).collect(), "")}
                {select("Scope", "project", project_options(&projects, true), "")}
                {select("Role", "role", role_options(), "viewer")}
            }
            .into_any()
        },
    )
}

/// Removes a role bound to a person.
#[must_use]
pub fn binding_actions(b: &AdminRoleBindingView) -> AnyView {
    action(
        format!("/api/v1/admin/role-bindings/{}/remove", b.id),
        "Remove the role",
        "Remove",
    )
}

/// Maps a group to a role, on a project or on every project.
#[must_use]
pub fn add_group(projects: &[AdminProjectView]) -> AnyView {
    let projects = projects.to_vec();
    post(
        "/api/v1/admin/group-mappings".to_owned(),
        "Map a group to a role",
        "Map group",
        move || {
            view! {
                {text("Group", "group", String::new(), "mobile-qa", true)}
                {select("Scope", "project", project_options(&projects, true), "")}
                {select("Role", "role", role_options(), "viewer")}
            }
            .into_any()
        },
    )
}

/// Changes or removes a dashboard group mapping. A configured mapping is locked.
#[must_use]
pub fn group_actions(g: &AdminGroupMappingView) -> AnyView {
    if g.source == "config" {
        return locked("group mapping");
    }
    let current = g.role.clone();
    let change = post(
        format!("/api/v1/admin/group-mappings/{}/edit", g.id),
        "Change the role",
        "Save role",
        move || select("Role", "role", role_options(), &current),
    );
    row_actions(vec![
        details("Change role", change),
        action(
            format!("/api/v1/admin/group-mappings/{}/remove", g.id),
            "Remove the mapping",
            "Remove",
        ),
    ])
}

// SPDX-License-Identifier: AGPL-3.0-only
//! The administration pages (Screen G): `/admin`, one section per area, and `/admin/audit`, the
//! audit history. Each section fetches its own area, so one refusal or failure never hides the
//! others. The server decides who may read; the page only shows what it is given.

#![expect(
    clippy::must_use_candidate,
    reason = "Leptos's #[component] macro generates the props structs and the components return views"
)]

use fleet_protocol::rest::admin::{AdminAuditEventView, AdminAuditPage};
use fleet_ui::components::{
    controls::{Button, ButtonVariant},
    display::EmptyState,
};
use leptos::{prelude::*, task::spawn_local};

use super::{
    model::{ADMIN_PATH, AUDIT_HEADERS, AUDIT_PAGE, AUDIT_PATH, audit_cells},
    view::{
        agent_rows, binding_rows, class_rows, ctx, grid, grid_or_empty, group_rows, ingestion_view,
        key_rows, projects_rows, section, setting_rows, trust_rows,
    },
};
use crate::remote::remote;
use crate::{api, api::ApiError, app::ErrorNotice, screens::parts::Fetched};

/// The administration page: every area, in the order of the spec's list.
#[component]
#[expect(
    clippy::too_many_lines,
    reason = "one page, one section per area: the view reads top to bottom in the spec's order"
)]
pub fn AdminRoute() -> impl IntoView {
    let projects = remote(1000, api::admin_projects);
    let trust = remote(1000, api::admin_trust_policies);
    let keys = remote(1000, api::admin_api_keys);
    let roles = remote(1000, api::admin_role_mappings);
    let retention = remote(1000, api::admin_retention);
    let artifacts = remote(1000, api::admin_artifact_policy);
    let metrics = remote(1000, api::admin_metric_policy);
    let agents = remote(1000, api::admin_agents);
    let ingestion = remote(1000, api::admin_ingestion);

    view! {
        <div class="page-h">
            <div>
                <h2 class="text-[26px] font-semibold leading-[1.2] tracking-[-0.015em] max-[720px]:text-[21px]">"Administration"</h2>
                <p class="meta">"Installation-wide: every project, and who may do what"</p>
            </div>
        </div>
        <nav aria-label="Administration sections" class="border-b border-line pb-3">
            <ul class="flex flex-wrap gap-x-4 gap-y-1 text-sm">
                <li><a class="text-primary underline" href="#projects">"Projects"</a></li>
                <li><a class="text-primary underline" href="#trust">"Publisher trust"</a></li>
                <li><a class="text-primary underline" href="#keys">"API keys"</a></li>
                <li><a class="text-primary underline" href="#roles">"Role mappings"</a></li>
                <li><a class="text-primary underline" href="#retention">"Retention"</a></li>
                <li><a class="text-primary underline" href="#artifacts">"Artifact policy"</a></li>
                <li><a class="text-primary underline" href="#metrics">"Metric collection"</a></li>
                <li><a class="text-primary underline" href="#agents">"Agent compatibility"</a></li>
                <li><a class="text-primary underline" href="#ingestion">"Ingestion health"</a></li>
                <li><a class="text-primary underline" href="#audit">"Audit history"</a></li>
            </ul>
        </nav>
        {section(
            "projects",
            "Projects",
            "Onboarding: a project, its working keys and its trust policies",
            Fetched::from(&projects),
            |rows| {
                let ctx = ctx();
                grid_or_empty(
                    "Projects",
                    &["Slug", "Name", "Default branch", "Working API keys", "Trust policies", "Status", "Created"],
                    projects_rows(&rows, &ctx),
                    "No project yet",
                )
            },
        )}
        {section(
            "trust",
            "Publisher trust",
            "Who may publish into a project with a CI token. Set in the configuration",
            Fetched::from(&trust),
            |rows| {
                let ctx = ctx();
                grid_or_empty(
                    "Publisher trust policies",
                    &["Project", "Name", "Issuer", "Repository ID", "Owner ID", "Trusted refs", "Events", "Environment", "PR policy", "Status", "Created"],
                    trust_rows(&rows, &ctx),
                    "No publisher trust policy",
                )
            },
        )}
        {section(
            "keys",
            "API keys",
            "Metadata only: the secret is shown once, when a key is made, and never again",
            Fetched::from(&keys),
            |rows| {
                let ctx = ctx();
                grid_or_empty(
                    "API keys",
                    &["Project", "Name", "Prefix", "Trust", "Status", "Expires", "Created by", "Last used", "Created"],
                    key_rows(&rows, &ctx),
                    "No API key yet",
                )
            },
        )}
        {section(
            "roles",
            "Role mappings",
            "Roles bound to people, and the identity-provider groups that grant them",
            Fetched::from(&roles),
            |mappings| {
                let ctx = ctx();
                view! {
                    <div class="space-y-6">
                        <div>
                            <h4 class="pb-2 text-sm font-semibold">"Bound to people"</h4>
                            {grid_or_empty(
                                "Roles bound to people",
                                &["Person", "Scope", "Role", "Granted"],
                                binding_rows(&mappings.bindings, &ctx),
                                "No role is bound to a person",
                            )}
                        </div>
                        <div>
                            <h4 class="pb-2 text-sm font-semibold">"Groups"</h4>
                            {grid_or_empty(
                                "Identity-provider groups and their roles",
                                &["Issuer", "Group", "Scope", "Role"],
                                group_rows(&mappings.groups),
                                "No group grants a role",
                            )}
                        </div>
                    </div>
                }
                .into_any()
            },
        )}
        {section(
            "retention",
            "Retention",
            "Artifact classes per project, and the windows the server keeps data for",
            Fetched::from(&retention),
            |page| {
                view! {
                    <div class="space-y-6">
                        <div>
                            <h4 class="pb-2 text-sm font-semibold">"Artifact classes"</h4>
                            {grid_or_empty(
                                "Retention classes per project",
                                &["Project", "Class", "Kept"],
                                class_rows(&page.classes),
                                "No retention class",
                            )}
                        </div>
                        <div>
                            <h4 class="pb-2 text-sm font-semibold">"Windows"</h4>
                            {grid(
                                "Retention windows from the configuration",
                                &["Setting", "Value", "Source"],
                                setting_rows(&page.windows),
                            )}
                        </div>
                    </div>
                }
                .into_any()
            },
        )}
        {section(
            "artifacts",
            "Artifact policy",
            "Limits on what agents may upload. Set in the configuration",
            Fetched::from(&artifacts),
            |rows| grid(
                "Artifact policy from the configuration",
                &["Setting", "Value", "Source"],
                setting_rows(&rows),
            ),
        )}
        {section(
            "metrics",
            "Metric collection",
            "How often metrics are checkpointed and kept",
            Fetched::from(&metrics),
            |rows| grid(
                "Metric collection policy",
                &["Setting", "Value", "Source"],
                setting_rows(&rows),
            ),
        )}
        {section(
            "agents",
            "Agent compatibility",
            "Agent versions the server has seen, against the protocol it speaks",
            Fetched::from(&agents),
            |view| {
                let ctx = ctx();
                let note = format!(
                    "This server is {} and speaks protocol {}",
                    view.server_version, view.protocol_version
                );
                view! {
                    <p class="pb-3 text-sm text-muted">{note}</p>
                    {grid_or_empty(
                        "Agent versions seen",
                        &["Agent version", "Protocol", "Runs", "Open runs", "Last started", "Compatible"],
                        agent_rows(&view.agents, &ctx),
                        "No agent has reported in",
                    )}
                }
                .into_any()
            },
        )}
        {section(
            "ingestion",
            "Ingestion health",
            "Refused events, reported losses and how far behind the open runs are",
            Fetched::from(&ingestion),
            |view| ingestion_view(&view, &ctx()),
        )}
        <section id="audit" data-testid="admin-audit" aria-labelledby="audit-heading" class="mt-9 border-t border-line pt-4">
            <div class="flex flex-wrap items-baseline gap-x-4 gap-y-1 pb-3">
                <h3 id="audit-heading" class="text-[15px] font-semibold">"Audit history"</h3>
                <p class="note">"Every change an administrator or a publisher made, newest first"</p>
            </div>
            <a class="text-primary underline" href=AUDIT_PATH>"Open the audit history"</a>
        </section>
    }
}

/// The audit history, newest first, a page at a time.
#[component]
pub fn AuditRoute() -> impl IntoView {
    let rows = RwSignal::new(Vec::<AdminAuditEventView>::new());
    let next = RwSignal::new(None::<String>);
    let error = RwSignal::new(None::<ApiError>);
    let started = RwSignal::new(false);
    let fetch = move |cursor: Option<String>| {
        spawn_local(async move {
            match api::admin_audit(cursor.as_deref(), AUDIT_PAGE).await {
                Ok(AdminAuditPage { items, next_cursor }) => {
                    rows.update(|r| r.extend(items));
                    next.set(next_cursor);
                    error.set(None);
                }
                Err(e) => error.set(Some(e)),
            }
            started.set(true);
        });
    };
    fetch(None);
    view! {
        <div class="page-h">
            <div>
                <h2 class="text-[26px] font-semibold leading-[1.2] tracking-[-0.015em] max-[720px]:text-[21px]">"Audit history"</h2>
                <p class="meta">
                    <a class="text-primary underline" href=ADMIN_PATH>"Back to administration"</a>
                </p>
            </div>
        </div>
        <ErrorNotice error=error />
        {move || {
            let shown = rows.get();
            let ctx = ctx();
            if !started.get() && shown.is_empty() {
                view! { <p class="note">"Loading"</p> }.into_any()
            } else if shown.is_empty() {
                view! { <EmptyState title="No audit events yet" description="A change made in Fleet is recorded here." /> }.into_any()
            } else {
                let cells: Vec<Vec<String>> = shown.iter().map(|e| audit_cells(e, &ctx)).collect();
                view! {
                    {grid("Audit history, newest first", &AUDIT_HEADERS, cells)}
                }
                .into_any()
            }
        }}
        {move || {
            next.get().map(|cursor| {
                view! {
                    <div class="pt-4">
                        <Button
                            variant=ButtonVariant::Secondary
                            on_click=Callback::new(move |()| fetch(Some(cursor.clone())))
                        >
                            "Load more"
                        </Button>
                    </div>
                }
            })
        }}
    }
}

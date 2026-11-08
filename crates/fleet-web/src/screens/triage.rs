// SPDX-License-Identifier: AGPL-3.0-only
//! Failure triage (issue #84): what a maintainer recorded about a failure fingerprint, and the
//! control that changes it. Screens D and E show it. The control is offered only to a session that
//! may triage the project; the server checks again on every change.

#![expect(
    clippy::must_use_candidate,
    reason = "Leptos's #[component] macro generates the props structs"
)]

use fleet_protocol::rest::{
    SessionView,
    history::{FailureTriageRequest, FailureTriageView, TriageState},
};
use fleet_ui::components::controls::{Button, ButtonVariant};
use leptos::{prelude::*, task::spawn_local};

use crate::{api, app::Auth};

/// Whether a session may triage `project`: a maintainer or an admin, on that project or on every
/// project.
#[must_use]
pub fn may_triage(session: &SessionView, project: &str) -> bool {
    session.roles.iter().any(|r| {
        matches!(r.role.as_str(), "maintainer" | "admin")
            && r.project.as_deref().is_none_or(|p| p == project)
    })
}

/// The state's value in the select: empty when untriaged.
fn choice_of(state: Option<TriageState>) -> &'static str {
    match state {
        None => "",
        Some(TriageState::Known) => "known",
        Some(TriageState::Resolved) => "resolved",
    }
}

fn label_of(state: Option<TriageState>) -> &'static str {
    match state {
        None => "Untriaged",
        Some(TriageState::Known) => "Known",
        Some(TriageState::Resolved) => "Resolved",
    }
}

/// What the control shows and edits: the recorded state and link, and the draft a maintainer is
/// typing. The recorded values change only when the server accepts a change.
#[derive(Debug, Clone, Copy)]
struct Signals {
    state: RwSignal<Option<TriageState>>,
    link: RwSignal<String>,
    choice: RwSignal<String>,
    draft_link: RwSignal<String>,
    message: RwSignal<Option<String>>,
}

/// Sends the draft to the server and takes its answer as the recorded state.
fn save(signals: Signals, csrf_token: String, project: String, fingerprint: String) {
    let choice = signals.choice.get_untracked();
    let url = signals.draft_link.get_untracked().trim().to_owned();
    spawn_local(async move {
        let result = match choice.as_str() {
            "known" => {
                let body = FailureTriageRequest {
                    state: TriageState::Known,
                    issue_url: Some(url),
                };
                api::set_triage(&project, &fingerprint, &body, &csrf_token).await
            }
            "resolved" => {
                let body = FailureTriageRequest {
                    state: TriageState::Resolved,
                    issue_url: Some(url).filter(|u| !u.is_empty()),
                };
                api::set_triage(&project, &fingerprint, &body, &csrf_token).await
            }
            _ => api::clear_triage(&project, &fingerprint, &csrf_token).await,
        };
        match result {
            Ok(saved) => {
                let url = saved.issue_url.unwrap_or_default();
                signals.state.set(saved.state);
                signals.link.set(url.clone());
                signals.choice.set(choice_of(saved.state).to_owned());
                signals.draft_link.set(url);
                signals.message.set(None);
            }
            Err(e) => signals.message.set(Some(e.to_string())),
        }
    });
}

/// The recorded state of one failure fingerprint, and for a maintainer the controls to change it.
/// `triage` is what the server sent with the failure; `None` reads as untriaged.
#[component]
pub fn TriageControl(
    /// The project slug.
    project: String,
    /// The failure fingerprint, `v1:…`.
    fingerprint: String,
    /// What is recorded now.
    triage: Option<FailureTriageView>,
) -> impl IntoView {
    let auth = expect_context::<Auth>();
    let state = triage.as_ref().and_then(|t| t.state);
    let link = triage.and_then(|t| t.issue_url).unwrap_or_default();
    let signals = Signals {
        state: RwSignal::new(state),
        link: RwSignal::new(link.clone()),
        choice: RwSignal::new(choice_of(state).to_owned()),
        draft_link: RwSignal::new(link),
        message: RwSignal::new(None),
    };
    let project = StoredValue::new(project);
    let fingerprint = StoredValue::new(fingerprint);
    let may_edit = move || {
        auth.session
            .get()
            .is_some_and(|s| may_triage(&s, &project.get_value()))
    };
    view! {
        <div class="mt-2 flex flex-wrap items-center gap-x-3 gap-y-2 text-sm" data-testid="triage">
            <span data-testid="triage-state" data-state=move || choice_of(signals.state.get()).to_owned()>
                {move || label_of(signals.state.get())}
            </span>
            {move || {
                let url = signals.link.get();
                (!url.is_empty())
                    .then(|| {
                        let href = url.clone();
                        view! {
                            <a
                                data-testid="triage-link"
                                class="text-primary underline"
                                href=href
                                target="_blank"
                                rel="noopener noreferrer"
                            >
                                {url}
                            </a>
                        }
                    })
            }}
            {move || {
                may_edit()
                    .then(|| {
                        view! {
                            <TriageEditor signals project fingerprint />
                        }
                    })
            }}
            <p role="status" class="text-sm text-tone-danger-fg" data-testid="triage-message">
                {move || signals.message.get().unwrap_or_default()}
            </p>
        </div>
    }
}

/// The select, the link field and the save button of a maintainer's control.
#[component]
fn TriageEditor(
    /// What the control shows and edits.
    signals: Signals,
    /// The project slug.
    project: StoredValue<String>,
    /// The failure fingerprint.
    fingerprint: StoredValue<String>,
) -> impl IntoView {
    let auth = expect_context::<Auth>();
    let select_id = format!("triage-select-{}", fingerprint.get_value());
    let on_save = move |()| {
        if let Some(session) = auth.session.get_untracked() {
            save(
                signals,
                session.csrf_token,
                project.get_value(),
                fingerprint.get_value(),
            );
        }
    };
    view! {
        <form class="flex flex-wrap items-center gap-2" on:submit=|ev| ev.prevent_default()>
            <label for=select_id.clone() class="sr-only">"Triage state"</label>
            <select
                id=select_id
                data-testid="triage-select"
                on:change=move |ev| signals.choice.set(event_target_value(&ev))
            >
                <option value="" selected=move || signals.choice.get().is_empty()>"Untriaged"</option>
                <option value="known" selected=move || signals.choice.get() == "known">"Known"</option>
                <option value="resolved" selected=move || signals.choice.get() == "resolved">"Resolved"</option>
            </select>
            <input
                type="url"
                data-testid="triage-link-input"
                aria-label="Issue link"
                placeholder="https://github.com/…/issues/…"
                prop:value=move || signals.draft_link.get()
                on:input=move |ev| signals.draft_link.set(event_target_value(&ev))
            />
            <Button variant=ButtonVariant::Secondary on_click=Callback::new(on_save)>
                "Save triage"
            </Button>
        </form>
    }
}

#[cfg(test)]
mod tests {
    use fleet_protocol::rest::{SessionRole, SessionUser};

    use super::*;

    fn session(roles: &[(Option<&str>, &str)]) -> SessionView {
        SessionView {
            user: SessionUser {
                id: uuid::Uuid::nil(),
                email: None,
                name: None,
            },
            roles: roles
                .iter()
                .map(|(project, role)| SessionRole {
                    project: project.map(str::to_owned),
                    role: (*role).to_owned(),
                })
                .collect(),
            csrf_token: "csrf".to_owned(),
        }
    }

    #[test]
    fn only_a_maintainer_or_admin_of_the_project_may_triage_it() {
        assert!(may_triage(
            &session(&[(Some("shop"), "maintainer")]),
            "shop"
        ));
        assert!(may_triage(&session(&[(None, "admin")]), "shop"));
        assert!(!may_triage(&session(&[(Some("shop"), "viewer")]), "shop"));
        assert!(!may_triage(&session(&[(Some("other"), "admin")]), "shop"));
        assert!(!may_triage(&session(&[]), "shop"));
    }
}

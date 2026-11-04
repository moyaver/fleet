// SPDX-License-Identifier: AGPL-3.0-only
//! The shell: header, router and the sign-in gate (PLAN §7.5, §10.1).
//!
//! The SPA holds no token. Whether a login is needed is learned from the data requests
//! themselves: a 401 anywhere shows a "Sign in" link that returns to the current page, while a
//! deployment with anonymous reads or a viewer token never sees one. `GET /api/v1/auth/session`
//! only decides who the header greets and whether "Sign out" is offered.

#![expect(
    clippy::must_use_candidate,
    reason = "Leptos's #[component] macro generates the props structs and the components return views"
)]

use fleet_protocol::rest::SessionView;
use fleet_ui::components::{
    controls::{Button, ButtonVariant},
    display::EmptyState,
    layout::Layout,
};
use leptos::{prelude::*, task::spawn_local};
use leptos_router::{
    components::{A, Route, Router, Routes},
    hooks::use_location,
    path,
};

use crate::{
    api::{self, ApiError},
    paths::login_url,
    screens::{overview::Overview, pipeline::PipelineDetail},
};

/// What the shell knows about the person, shared with every page.
#[derive(Debug, Clone, Copy)]
pub struct Auth {
    /// The signed-in session, if there is one.
    pub session: RwSignal<Option<SessionView>>,
    /// A request was refused for lack of a login.
    pub sign_in_needed: RwSignal<bool>,
}

/// The app: routes inside the shell.
#[component]
pub fn App() -> impl IntoView {
    let auth = Auth {
        session: RwSignal::new(None),
        sign_in_needed: RwSignal::new(false),
    };
    provide_context(auth);
    spawn_local(async move {
        if let Ok(session) = api::session().await {
            auth.session.set(Some(session));
        }
    });
    view! {
        <Router>
            <Layout nav=move || view! { <Header /> }>
                <Routes fallback=|| {
                    view! { <EmptyState title="Page not found" description="Check the address, or go back to the overview." /> }
                }>
                    <Route path=path!("/") view=Overview />
                    <Route path=path!("/p/:project/pipelines/:id") view=PipelineDetail />
                    <Route path=path!("/workers/:id") view=|| view! { <Unavailable what="Worker" /> } />
                    <Route path=path!("/devices/:id") view=|| view! { <Unavailable what="Device" /> } />
                    <Route path=path!("/flows/:execution_id") view=|| view! { <Unavailable what="Flow" /> } />
                </Routes>
            </Layout>
        </Router>
    }
}

/// A page that is routed but not built yet.
#[component]
fn Unavailable(what: &'static str) -> impl IntoView {
    view! {
        <EmptyState
            title=format!("{what} details are not available yet")
            description="This page is coming soon."
        />
    }
}

/// The title (the page's `h1`), the navigation and the person.
#[component]
fn Header() -> impl IntoView {
    let auth = expect_context::<Auth>();
    let location = use_location();
    let sign_in = move || {
        login_url(&format!(
            "{}{}",
            location.pathname.get(),
            location.search.get()
        ))
    };
    let sign_out = move |()| {
        if let Some(session) = auth.session.get_untracked() {
            spawn_local(async move {
                let _ = api::logout(&session.csrf_token).await;
                let _ = window().location().assign("/");
            });
        }
    };
    view! {
        <header class="border-b border-border bg-surface">
            <div class="mx-auto flex max-w-7xl items-center gap-6 px-4 py-3">
                <h1 class="text-lg font-semibold">"Fleet"</h1>
                <nav aria-label="Primary">
                    <ul class="flex gap-4">
                        <li>
                            <A
                                href="/"
                                exact=true
                                attr:class="rounded px-2 py-1 text-muted hover:text-fg aria-[current=page]:font-semibold aria-[current=page]:text-fg"
                            >
                                "Overview"
                            </A>
                        </li>
                    </ul>
                </nav>
                <div class="ml-auto flex items-center gap-3 text-sm">
                    {move || match auth.session.get() {
                        Some(session) => {
                            let who = session
                                .user
                                .name
                                .or(session.user.email)
                                .unwrap_or_else(|| "Signed in".to_owned());
                            view! {
                                <span data-testid="whoami" class="text-muted">{who}</span>
                                <Button variant=ButtonVariant::Secondary on_click=Callback::new(sign_out)>
                                    "Sign out"
                                </Button>
                            }
                                .into_any()
                        }
                        None if auth.sign_in_needed.get() => {
                            view! {
                                <a
                                    data-testid="sign-in"
                                    class="rounded border border-border px-3 py-1.5 font-medium hover:bg-tone-neutral-bg"
                                    href=sign_in()
                                >
                                    "Sign in"
                                </a>
                            }
                                .into_any()
                        }
                        None => ().into_any(),
                    }}
                </div>
            </div>
        </header>
    }
}

/// Explains why a page has no data: sign-in needed (with the link), not found, or a failure.
/// Renders nothing while there is no error.
#[component]
pub fn ErrorNotice(
    /// The error to show.
    #[prop(into)]
    error: Signal<Option<ApiError>>,
) -> impl IntoView {
    let auth = expect_context::<Auth>();
    let location = use_location();
    Effect::new(move |_| {
        if error.get() == Some(ApiError::Unauthorized) {
            auth.sign_in_needed.set(true);
        }
    });
    move || {
        error.get().map(|e| match e {
            ApiError::Unauthorized => {
                let href = login_url(&format!(
                    "{}{}",
                    location.pathname.get(),
                    location.search.get()
                ));
                view! {
                    <div role="alert" data-testid="sign-in-required" class="rounded-lg border border-border bg-surface p-4">
                        <p class="font-semibold">"Sign in to see this"</p>
                        <p class="mt-1 text-muted">
                            "Fleet shows private test data to signed-in people only."
                        </p>
                        <p class="mt-3">
                            <a class="rounded bg-primary px-3 py-1.5 text-sm font-medium text-primary-fg" href=href>
                                "Sign in"
                            </a>
                        </p>
                    </div>
                }
                    .into_any()
            }
            other => {
                view! {
                    <p role="alert" data-testid="error" class="rounded-lg border border-tone-danger-fg/30 bg-tone-danger-bg p-3 text-tone-danger-fg">
                        {other.to_string()}
                    </p>
                }
                    .into_any()
            }
        })
    }
}

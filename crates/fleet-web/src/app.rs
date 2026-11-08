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
    layout::{Layout, TopBar},
    live::{LiveIndicator, LiveState},
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
    screens::{
        admin::{
            model::{ADMIN_PATH, is_global_admin},
            route::{AdminRoute, AuditRoute},
        },
        compare::route::CompareRoute,
        history::route::HistoryRoute,
        overview::Overview,
        pipeline::PipelineDetail,
        routes::{DeviceRoute, FlowRoute, WorkerRoute},
    },
};

/// What the shell knows about the person, shared with every page.
#[derive(Debug, Clone, Copy)]
pub struct Auth {
    /// The signed-in session, if there is one.
    pub session: RwSignal<Option<SessionView>>,
    /// A request was refused for lack of a login.
    pub sign_in_needed: RwSignal<bool>,
}

/// The live indicator of the top bar, lent by the page that follows a live feed. `None` while the
/// page has none (the indicator is then not shown).
#[derive(Debug, Clone, Copy)]
pub struct LiveBar {
    /// Where the feed stands.
    pub state: RwSignal<LiveState>,
    /// What follows the state's word, such as "updated 2 s ago".
    pub detail: RwSignal<String>,
    /// The menu's action, such as "Pause"; `None` means no menu.
    pub action: RwSignal<Option<String>>,
    /// Runs the menu's action.
    pub on_action: Callback<()>,
}

/// The slot the top bar reads its live indicator from; a page sets it while it is shown.
#[derive(Debug, Clone, Copy)]
pub struct LiveSlot(pub RwSignal<Option<LiveBar>>);

/// The app: routes inside the shell.
#[component]
pub fn App() -> impl IntoView {
    let auth = Auth {
        session: RwSignal::new(None),
        sign_in_needed: RwSignal::new(false),
    };
    provide_context(auth);
    provide_context(LiveSlot(RwSignal::new(None)));
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
                    <Route path=path!("/workers/:id") view=WorkerRoute />
                    <Route path=path!("/devices/:id") view=DeviceRoute />
                    <Route path=path!("/flows/:execution_id") view=FlowRoute />
                    <Route path=path!("/history") view=HistoryRoute />
                    <Route path=path!("/compare") view=CompareRoute />
                    <Route path=path!("/admin") view=AdminRoute />
                    <Route path=path!("/admin/audit") view=AuditRoute />
                </Routes>
            </Layout>
        </Router>
    }
}

/// The project a page is about, from its address: `/p/<project>/…` or `?project=`.
fn project_of(pathname: &str, search: &str) -> Option<String> {
    let mut segments = pathname.trim_matches('/').split('/');
    if segments.next() == Some("p") {
        return segments.next().filter(|p| !p.is_empty()).map(str::to_owned);
    }
    search
        .trim_start_matches('?')
        .split('&')
        .find_map(|pair| pair.strip_prefix("project="))
        .filter(|p| !p.is_empty())
        .map(str::to_owned)
}

/// The top bar (the redesign's Foundations: one rule, no panel): the brand (a link to Overview),
/// the project the page is about, the navigation and the account. The live indicator of a page
/// that has one sits at the right. On phones the account and the navigation scroll in their own
/// boxes, and the account moves behind a ⋯ menu.
#[component]
fn Header() -> impl IntoView {
    let location = use_location();
    let project = move || project_of(&location.pathname.get(), &location.search.get());
    let live = expect_context::<LiveSlot>().0;
    // Administration is offered to global administrators only; the server refuses everyone else.
    let auth = expect_context::<Auth>();
    let admin = move || {
        auth.session
            .with(|s| s.as_ref().is_some_and(is_global_admin))
    };
    view! {
        <TopBar>
            // The product name is the page's only h1; the link takes the reader to Overview.
            <h1 class="m-0 inline-flex">
                <A href="/" attr:class="brand">
                    <span class="brand-mark" aria-hidden="true"></span>
                    <span class="brand-word">"Fleet"</span>
                </A>
            </h1>
            {move || project().map(|p| view! {
                <span class="proj" data-testid="project-label">
                    <span class="muted">"Project"</span>
                    <span>{p}</span>
                </span>
            })}
            <nav aria-label="Primary" class="hide-phone-top">
                <ul class="nav">
                    <li>
                        <A href="/" exact=true>"Overview"</A>
                    </li>
                    <li>
                        <A href="/history">"History"</A>
                    </li>
                    <li>
                        <A href="/compare">"Compare"</A>
                    </li>
                    {move || admin().then(|| view! {
                        <li>
                            <A href=ADMIN_PATH attr:data-testid="nav-admin">"Administration"</A>
                        </li>
                    })}
                </ul>
            </nav>
            <div class="top-right">
                {move || live.get().map(|bar| view! {
                    <LiveIndicator
                        state=bar.state
                        detail=bar.detail
                        action=bar.action
                        on_action=bar.on_action
                        menu_id="live-menu"
                    />
                })}
                <div class="hide-phone-top flex items-center gap-3">
                    <Account compact=false />
                </div>
                <div class="only-phone">
                    <button
                        type="button"
                        class="inline-flex min-h-11 items-center px-2 text-lg text-muted"
                        popovertarget="more-menu"
                        aria-label="More"
                    >
                        "⋯"
                    </button>
                    // On a phone the bar holds the brand mark only: the project, the pages and the
                    // account are all here, one tap away.
                    <div id="more-menu" popover="auto" aria-label="More" class="min-w-56 rounded border border-line-strong bg-bg p-3">
                        <div class="flex flex-col gap-3">
                            {move || project().map(|p| view! {
                                <p class="text-sm text-muted">"Project " <span class="text-fg">{p}</span></p>
                            })}
                            <nav aria-label="Pages">
                                <ul class="flex flex-col">
                                    <li>
                                        <A href="/" exact=true attr:class="flex min-h-11 items-center">"Overview"</A>
                                    </li>
                                    <li>
                                        <A href="/history" attr:class="flex min-h-11 items-center">"History"</A>
                                    </li>
                                    <li>
                                        <A href="/compare" attr:class="flex min-h-11 items-center">"Compare"</A>
                                    </li>
                                    {move || admin().then(|| view! {
                                        <li>
                                            <A href=ADMIN_PATH attr:class="flex min-h-11 items-center">"Administration"</A>
                                        </li>
                                    })}
                                </ul>
                            </nav>
                            <div class="flex flex-col gap-2 border-t border-line pt-3">
                                <Account compact=true />
                            </div>
                        </div>
                    </div>
                </div>
            </div>
        </TopBar>
    }
}

/// Who is signed in, with Sign out; or the Sign in link once a request asked for a login. The
/// compact copy sits in the phone's ⋯ menu and carries no test ids, so each id is on one element.
#[component]
fn Account(
    /// Whether this is the copy inside the phone's menu.
    compact: bool,
) -> impl IntoView {
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
    let whoami = (!compact).then_some("whoami");
    let sign_in_id = (!compact).then_some("sign-in");
    move || {
        match auth.session.get() {
        Some(session) => {
            let who = session
                .user
                .name
                .or(session.user.email)
                .unwrap_or_else(|| "Signed in".to_owned());
            view! {
                <span data-testid=whoami class="who">{who}</span>
                <Button variant=ButtonVariant::Secondary on_click=Callback::new(sign_out)>
                    "Sign out"
                </Button>
            }
                .into_any()
        }
        None if auth.sign_in_needed.get() => {
            view! {
                <a
                    data-testid=sign_in_id
                    rel="external"
                    class="inline-flex min-h-8 items-center rounded border border-line-strong px-3 text-sm font-medium text-fg hover:bg-band"
                    href=sign_in()
                >
                    "Sign in"
                </a>
            }
                .into_any()
        }
        None => ().into_any(),
    }
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
                    <div role="alert" data-testid="sign-in-required" class="border-t border-line pt-4">
                        <p class="font-semibold">"Sign in to see this"</p>
                        <p class="mt-1 text-muted">
                            "Fleet shows private test data to signed-in people only."
                        </p>
                        <p class="mt-3">
                            <a class="rounded bg-primary px-3 py-1.5 text-sm font-medium text-primary-fg" href=href rel="external">
                                "Sign in"
                            </a>
                        </p>
                    </div>
                }
                    .into_any()
            }
            other => {
                view! {
                    <p role="alert" data-testid="error" class="border-l-2 border-tone-danger-fg py-1 pl-3 text-tone-danger-fg">
                        {other.to_string()}
                    </p>
                }
                    .into_any()
            }
        })
    }
}

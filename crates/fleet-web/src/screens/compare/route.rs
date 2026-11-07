// SPDX-License-Identifier: AGPL-3.0-only
//! The `/compare` route: two attempt pickers whose choice lives in the URL (`a` and `b`), and the
//! comparison of the chosen pair. A reload or a shared link shows the same comparison.

#![expect(
    clippy::must_use_candidate,
    reason = "Leptos's #[component] macro generates the props structs and the components return views"
)]

use fleet_ui::components::{display::EmptyState, forms::use_url_query};
use leptos::prelude::*;
use uuid::Uuid;

use crate::{
    api,
    app::ErrorNotice,
    clock::{now_ms, utc_offset_minutes},
    paths::with_query_param,
    remote::remote,
    screens::{
        common::Ctx,
        compare::{
            model::{CompareModel, CompareQuery, PARAM_A, PARAM_B, attempt_options},
            view::{AttemptPickers, ComparisonView, Which},
        },
        parts::{Fetched, loaded},
        project::InProject,
    },
};

/// How many recent attempts the pickers offer.
const PICKABLE: u32 = 100;

/// Screen F, at `/compare?project=<slug>&a=<attempt>&b=<attempt>`.
#[component]
pub fn CompareRoute() -> impl IntoView {
    view! {
        <div data-testid="compare-page">
            <InProject
                path="/compare"
                body=Callback::new(|project: String| view! { <CompareBody project /> }.into_any())
            />
        </div>
    }
}

fn ctx(project: &str) -> Ctx {
    Ctx {
        project: project.to_owned(),
        now_ms: now_ms(),
        utc_offset_minutes: utc_offset_minutes(),
    }
}

#[component]
fn CompareBody(project: String) -> impl IntoView {
    let (query, set_query) = use_url_query();
    let chosen = Memo::new(move |_| CompareQuery::parse(&query.get()));
    let header_project = project.clone();
    let attempts = {
        let project = project.clone();
        remote(1000, move || {
            let project = project.clone();
            async move { api::attempts(&project, PICKABLE).await }
        })
    };
    let fetched = Fetched::from(&attempts);
    let on_pick = Callback::new(move |(which, id): (Which, Option<Uuid>)| {
        let key = match which {
            Which::A => PARAM_A,
            Which::B => PARAM_B,
        };
        let value = id.map(|i| i.to_string());
        set_query.run(with_query_param(
            &query.get_untracked(),
            key,
            value.as_deref(),
        ));
    });
    // Swapping rewrites both parameters in one step, so the history gets one entry.
    let on_swap = Callback::new(move |()| {
        let CompareQuery { a, b } = chosen.get_untracked();
        let id = |v: Option<Uuid>| v.map(|i| i.to_string());
        let (a, b) = (id(a), id(b));
        let q = with_query_param(&query.get_untracked(), PARAM_A, b.as_deref());
        set_query.run(with_query_param(&q, PARAM_B, a.as_deref()));
    });
    let a = Signal::derive(move || chosen.get().a);
    let b = Signal::derive(move || chosen.get().b);
    let pickers = {
        let project = project.clone();
        move || {
            loaded(fetched, |list| {
                let options = attempt_options(&list, &ctx(&project));
                view! { <AttemptPickers options a b on_pick on_swap /> }.into_any()
            })
        }
    };
    view! {
        <div class="page-h">
            <div>
                <h2 class="text-[26px] font-semibold leading-[1.2] tracking-[-0.015em] max-[720px]:text-[21px]">"Compare runs"</h2>
                <p class="meta"><span>"Project " <b>{header_project}</b></span></p>
            </div>
        </div>
        <ErrorNotice error=fetched.error />
        {pickers}
        {move || match chosen.get().both() {
            Some((a, b)) => view! { <Comparison project=project.clone() a b /> }.into_any(),
            None => view! {
                <EmptyState
                    title="Choose two runs"
                    description="Pick the baseline as A and the run to check as B."
                />
            }
            .into_any(),
        }}
    }
}

#[component]
fn Comparison(project: String, a: Uuid, b: Uuid) -> impl IntoView {
    let compare = {
        let project = project.clone();
        remote(1000, move || {
            let project = project.clone();
            async move { api::compare(&project, a, b).await }
        })
    };
    let fetched = Fetched::from(&compare);
    view! {
        <ErrorNotice error=fetched.error />
        {move || {
            let project = project.clone();
            loaded(
                fetched,
                move |view| {
                    let model = CompareModel::build(&view, &ctx(&project));
                    view! { <ComparisonView model /> }.into_any()
                },
            )
        }}
    }
}

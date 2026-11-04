// SPDX-License-Identifier: AGPL-3.0-only
//! Read-only display components: badges, pills, cards, stat tiles, empty and loading states.

use leptos::prelude::*;

use crate::{provenance::Provenance, tone::Tone};

fn pill_class(tone: Tone) -> String {
    format!(
        "inline-flex items-center rounded-full border px-2 py-0.5 text-xs font-medium {}",
        tone.class()
    )
}

/// A small label with a colour role.
#[component]
pub fn Badge(
    /// Colour role.
    tone: Tone,
    /// The label text.
    children: Children,
) -> impl IntoView {
    view! { <span class=pill_class(tone)>{children()}</span> }
}

/// A status such as "online" or "failed": the text always says it, the colour only reinforces it.
#[component]
pub fn StatusPill(
    /// Colour role.
    tone: Tone,
    /// The status text.
    #[prop(into)]
    label: String,
) -> impl IntoView {
    view! {
        <span class=pill_class(tone)>
            <span aria-hidden="true" class="mr-1.5 inline-block size-1.5 rounded-full bg-current"></span>
            {label}
        </span>
    }
}

/// The provenance badge of PLAN I4. Hovering shows what the provenance means; the accessible
/// name carries it for assistive technology.
#[component]
pub fn ProvenanceBadge(
    /// What to show.
    #[prop(into)]
    provenance: Signal<Provenance>,
) -> impl IntoView {
    view! {
        <span
            role="img"
            class=move || pill_class(provenance.get().badge().tone)
            aria-label=move || provenance.get().badge().aria_label
            title=move || provenance.get().badge().description
        >
            {move || provenance.get().badge().label}
        </span>
    }
}

/// A bordered surface with an optional heading.
#[component]
pub fn Card(
    /// The heading, if any.
    #[prop(into, optional)]
    title: Option<String>,
    /// The content.
    children: Children,
) -> impl IntoView {
    view! {
        <section class="rounded-lg border border-border bg-surface p-4">
            {title.map(|t| view! { <h2 class="mb-2 text-sm font-semibold text-muted">{t}</h2> })}
            {children()}
        </section>
    }
}

/// A headline number with a label, a unit and its provenance. A missing value reads
/// "Unavailable" rather than a made-up number (spec §16 AC5).
#[component]
pub fn StatTile(
    /// What is measured.
    #[prop(into)]
    label: String,
    /// The value, or `None` when it is unavailable.
    #[prop(into)]
    value: Option<String>,
    /// The unit, shown after the value.
    #[prop(into, optional)]
    unit: Option<String>,
    /// Where the value comes from, if it should be shown.
    #[prop(into, optional)]
    provenance: Option<Provenance>,
) -> impl IntoView {
    let shown = match value {
        Some(v) => view! {
            <span class="text-2xl font-semibold tabular-nums">{v}</span>
            {unit.map(|u| view! { <span class="ml-1 text-sm text-muted">{u}</span> })}
        }
        .into_any(),
        None => view! { <span class="text-lg italic text-muted">"Unavailable"</span> }.into_any(),
    };
    view! {
        <Card>
            <p class="text-sm text-muted">{label}</p>
            <p class="mt-1">{shown}</p>
            {provenance.map(|p| view! { <p class="mt-2"><ProvenanceBadge provenance=p /></p> })}
        </Card>
    }
}

/// What to show when there is nothing to show yet.
#[component]
pub fn EmptyState(
    /// The headline, such as "No runs yet".
    #[prop(into)]
    title: String,
    /// A sentence on what to do next.
    #[prop(into, optional)]
    description: Option<String>,
) -> impl IntoView {
    view! {
        <div class="rounded-lg border border-dashed border-border p-8 text-center">
            <h2 class="text-lg font-semibold">{title}</h2>
            {description.map(|d| view! { <p class="mt-1 text-muted">{d}</p> })}
        </div>
    }
}

/// A loading placeholder of `lines` bars, announced as "Loading".
#[component]
pub fn Skeleton(
    /// How many bars to draw.
    #[prop(default = 3)]
    lines: usize,
) -> impl IntoView {
    view! {
        <div role="status" aria-busy="true" class="space-y-2">
            <span class="sr-only">"Loading"</span>
            {(0..lines)
                .map(|_| {
                    view! {
                        <div
                            aria-hidden="true"
                            class="h-4 rounded bg-tone-muted-bg motion-safe:animate-pulse"
                        ></div>
                    }
                })
                .collect_view()}
        </div>
    }
}

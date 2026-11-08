// SPDX-License-Identifier: AGPL-3.0-only
//! Read-only display components: status words, provenance labels, sections, readouts, empty and
//! loading states. Sections are a heading and a hairline; nothing here draws a card.

use leptos::prelude::*;

use fleet_protocol::frame::RunOrigin;

use crate::{
    mark::Mark,
    provenance::{PUBLISHED, Provenance},
    tone::Tone,
};

fn pill_class(tone: Tone) -> String {
    format!(
        "inline-flex items-center rounded-full border px-2 py-0.5 text-xs font-medium {}",
        tone.class()
    )
}

/// A small label with a colour role, for things that are not a status (activity, flaky verdicts,
/// device chips).
#[component]
pub fn Badge(
    /// Colour role.
    tone: Tone,
    /// The label text.
    children: Children,
) -> impl IntoView {
    view! { <span class=pill_class(tone)>{children()}</span> }
}

/// A status word with its icon and colour (`.st` in `tailwind.css`). The word is the text; the
/// icon is drawn in CSS, so a status never relies on colour alone.
#[component]
pub fn StatusPill(
    /// The icon and colour of the word.
    mark: Mark,
    /// The status word, Title Case.
    #[prop(into)]
    label: String,
) -> impl IntoView {
    view! {
        <span class=format!("st {}", mark.class())>
            <i class="si" aria-hidden="true"></i>
            {label}
        </span>
    }
}

/// The provenance label of PLAN I4: a monochrome pattern glyph and its word, one element with
/// the accessible name "Provenance: <word>". Hovering shows what the provenance means.
#[component]
pub fn ProvenanceBadge(
    /// What to show.
    #[prop(into)]
    provenance: Signal<Provenance>,
) -> impl IntoView {
    view! {
        <span
            role="img"
            class="pv"
            aria-label=move || provenance.get().badge().aria_label
            title=move || provenance.get().badge().description
        >
            <i class=move || format!("pg {}", provenance.get().badge().glyph) aria-hidden="true"></i>
            {move || provenance.get().badge().label}
        </span>
    }
}

/// A row's provenance: its mark only when the value is an exception (see
/// [`Provenance::marks_a_row`]). A settled value renders nothing here; its group says it once.
#[component]
pub fn RowProvenance(
    /// What to show.
    #[prop(into)]
    provenance: Signal<Provenance>,
) -> impl IntoView {
    move || {
        let p = provenance.get();
        p.marks_a_row()
            .then(|| view! { <ProvenanceBadge provenance=Signal::stored(p) /> })
    }
}

/// The origin label of a published run (issue #83). A live run shows nothing: the label marks only
/// the runs that were imported after the fact, so they never read as live.
#[component]
pub fn OriginBadge(
    /// The run's origin, for the value this badge sits beside.
    #[prop(into)]
    origin: Signal<RunOrigin>,
) -> impl IntoView {
    move || {
        (origin.get() == RunOrigin::Published).then(|| {
            view! {
                <span
                    role="img"
                    class="pv"
                    data-testid="origin-badge"
                    aria-label=PUBLISHED.aria_label
                    title=PUBLISHED.description
                >
                    <i class=format!("pg {}", PUBLISHED.glyph) aria-hidden="true"></i>
                    {PUBLISHED.label}
                </span>
            }
        })
    }
}

/// A group's provenance, said once in its heading, caption or section line: the label, then
/// the sentence that says which values it covers. Rows mark only the exceptions.
#[component]
pub fn ProvenanceStatement(
    /// How the group's values are known.
    provenance: Provenance,
    /// What the statement covers, after the label.
    children: Children,
) -> impl IntoView {
    view! {
        <p class="pv-statement">
            <ProvenanceBadge provenance=Signal::stored(provenance) />
            {children()}
        </p>
    }
}

/// A section: its heading on a hairline, then its content. No background, border box or radius
/// (the redesign's "no cards" rule).
#[component]
pub fn Card(
    /// The heading, if any.
    #[prop(into, optional)]
    title: Option<String>,
    /// The content.
    children: Children,
) -> impl IntoView {
    view! {
        <section class="card">
            {title.map(|t| view! { <div class="card-h"><h2>{t}</h2></div> })}
            <div class="card-b">{children()}</div>
        </section>
    }
}

/// A row of readouts divided by hairlines. Each child is one cell (a [`StatTile`], or a
/// [`ReadoutChoice`] that filters).
#[component]
pub fn ReadoutStrip(
    /// The cells, in order.
    children: Children,
) -> impl IntoView {
    view! { <div class="readouts">{children()}</div> }
}

/// A headline number with a label, a unit and its provenance, as one cell of a [`ReadoutStrip`].
/// A missing value reads "Unavailable" rather than a made-up number (spec §16 AC5).
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
            <span class="tile-n">{v}</span>
            {unit.map(|u| view! { <span class="ml-1 text-sm text-muted">{u}</span> })}
        }
        .into_any(),
        None => {
            view! { <span class="tile-n text-lg italic text-muted">"Unavailable"</span> }.into_any()
        }
    };
    view! {
        <div class="tile">
            <p class="tile-l">{label}</p>
            <p>{shown}</p>
            {provenance.map(|p| view! { <p class="mt-2"><ProvenanceBadge provenance=Signal::stored(p) /></p> })}
        </div>
    }
}

/// A readout that filters: a radio in a label, so the whole cell is the target. The checked
/// cell is underlined at its top edge. `test_id` and `note` are optional: left out, the cell is
/// what it was before they existed.
#[component]
pub fn ReadoutChoice(
    /// The radio group's name.
    #[prop(into)]
    name: String,
    /// The value this cell selects.
    #[prop(into)]
    value: String,
    /// The cell's label, such as "Running".
    #[prop(into)]
    label: String,
    /// The count shown in the cell.
    #[prop(into)]
    count: String,
    /// Whether this cell is the selected one.
    #[prop(into)]
    checked: Signal<bool>,
    /// Called when the cell is chosen.
    #[prop(optional)]
    on_select: Option<Callback<()>>,
    /// The cell's `data-testid`, when a test finds it.
    #[prop(into, optional)]
    test_id: Option<String>,
    /// A line under the count, such as "1 flow running now". `None` leaves no line.
    #[prop(into, optional_no_strip)]
    note: Option<String>,
) -> impl IntoView {
    view! {
        <label class="tile" data-testid=test_id>
            <input
                type="radio"
                name=name
                value=value
                prop:checked=move || checked.get()
                on:change=move |_| {
                    if let Some(on_select) = on_select {
                        on_select.run(());
                    }
                }
            />
            <span class="tile-l">{label}</span>
            <span class="tile-n">{count}</span>
            {note.map(|n| view! { <span class="tile-s">{n}</span> })}
        </label>
    }
}

/// A scroll box for a table: the table scrolls inside it, never the page. It is focusable and
/// named, so keyboard users can scroll it.
#[component]
pub fn TableBox(
    /// The accessible name of the scrollable region, such as "Pipelines".
    #[prop(into)]
    label: String,
    /// The table.
    children: Children,
) -> impl IntoView {
    view! {
        <div class="tbox" role="region" tabindex="0" aria-label=label>
            {children()}
        </div>
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
        <div class="py-10 text-center">
            <h2 class="text-[15px] font-semibold">{title}</h2>
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
                            class="h-4 rounded bg-raised motion-safe:animate-pulse"
                        ></div>
                    }
                })
                .collect_view()}
        </div>
    }
}

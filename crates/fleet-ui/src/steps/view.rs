// SPDX-License-Identifier: AGPL-3.0-only
//! Thin Leptos view over the step tree (WAI-ARIA `tree`).

use leptos::prelude::*;

use crate::components::display::{ProvenanceBadge, StatusPill};
use crate::steps::tree::{Key, Row, StepTree, TreeState, provenance_of, status_label, status_mark};

fn key_of(name: &str) -> Option<Key> {
    Some(match name {
        "ArrowUp" => Key::Up,
        "ArrowDown" => Key::Down,
        "ArrowLeft" => Key::Left,
        "ArrowRight" => Key::Right,
        "Home" => Key::Home,
        "End" => Key::End,
        _ => return None,
    })
}

/// The step tree. Each row shows its number, its status word, its duration and its own provenance
/// badge: a step settled from `commands.json` reads verified, one still read from the log reads
/// provisional (I4, issue #148). `on_select` receives the id of a step the user focuses or clicks.
#[component]
pub fn StepTreeView(
    /// The steps.
    tree: StepTree,
    /// Called with the step id when a step is activated (jump to its log lines).
    #[prop(into, optional)]
    on_select: Option<Callback<String>>,
) -> impl IntoView {
    let tree = StoredValue::new(tree);
    let state = RwSignal::new(tree.with_value(TreeState::open_to_attention));
    let rows = move || tree.with_value(|t| state.with(|s| s.rows(t)));
    let notify = move || {
        if let (Some(cb), Some(id)) = (on_select, state.with(|s| s.focused().map(str::to_owned))) {
            cb.run(id);
        }
    };
    view! {
        <ul
            class="divide-y divide-line text-fg"
            role="tree"
            aria-label="Steps"
            on:keydown=move |ev: web_sys::KeyboardEvent| {
                if let Some(key) = key_of(&ev.key()) {
                    ev.prevent_default();
                    tree.with_value(|t| state.update(|s| s.press(t, key)));
                    notify();
                }
            }
        >
            <For each=rows key=|r: &Row| (r.id.clone(), r.expanded, r.focused) let(row)>
                {
                    let id = row.id.clone();
                    // The tree's level, position and size are not typed attributes in the view
                    // macro (and `attr:` would name the attribute `attr:aria-level`), so they are
                    // set by name.
                    let (level, position, size) = (
                        row.level.to_string(),
                        row.pos_in_set.to_string(),
                        row.set_size.to_string(),
                    );
                    let number = row.number.clone().unwrap_or_default();
                    let unnumbered = row.number.is_none();
                    let provenance = provenance_of(row.source);
                    let item = view! {
                        <li
                            class="step-row flex min-w-0 flex-wrap items-baseline gap-x-3 gap-y-1 px-2 py-2 focus:outline-2 focus:outline-focus"
                            role="treeitem"
                            aria-expanded=row.expanded.map(|e| e.to_string())
                            aria-selected=if row.focused { "true" } else { "false" }
                            tabindex=if row.focused { "0" } else { "-1" }
                            data-status=status_label(row.status)
                            on:click=move |_| {
                                tree.with_value(|t| {
                                    state
                                        .update(|s| {
                                            s.focus(t, &id);
                                            s.toggle(t, &id);
                                        });
                                });
                                notify();
                            }
                        >
                            <span class="w-10 shrink-0 text-right font-mono text-xs tabular-nums text-muted">{number}</span>
                            <span
                                class="min-w-0 flex-1 break-words"
                                class:text-muted=unnumbered
                                style:padding-left=format!("{}rem", row.level.saturating_sub(1))
                            >
                                {row.label}
                            </span>
                            <StatusPill mark=status_mark(row.status) label=status_label(row.status) />
                            <ProvenanceBadge provenance=Signal::stored(provenance) />
                            {row.attempt.map(|a| view! { <span class="text-xs text-muted">{a}</span> })}
                            {row.duration.map(|d| view! { <span class="font-mono text-xs tabular-nums text-muted">{d}</span> })}
                        </li>
                    };
                    item.attr("aria-level", level)
                        .attr("aria-posinset", position)
                        .attr("aria-setsize", size)
                }
            </For>
        </ul>
    }
}

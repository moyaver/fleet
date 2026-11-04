// SPDX-License-Identifier: AGPL-3.0-only
//! Thin Leptos view over the step tree (WAI-ARIA `tree`).

use leptos::prelude::*;
use leptos::web_sys;

use crate::components::display::{ProvenanceBadge, StatusPill};
use crate::steps::tree::{Key, Row, StepTree, TreeState, provenance_of, status_label, status_tone};

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

/// The step tree. `on_select` receives the id of a step the user focuses or clicks.
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
            class="divide-y divide-border rounded border border-border bg-surface text-fg"
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
                    view! {
                        <li
                            class="flex items-center gap-2 px-2 py-1 focus:outline-2 focus:outline-focus"
                            role="treeitem"
                            attr:aria-level=row.level.to_string()
                            attr:aria-posinset=row.pos_in_set.to_string()
                            attr:aria-setsize=row.set_size.to_string()
                            attr:aria-expanded=row.expanded.map(|e| e.to_string())
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
                            <span class="flex-1" style:padding-left=format!("{}rem", row.level.saturating_sub(1))>{row.label}</span>
                            <StatusPill tone=status_tone(row.status) label=status_label(row.status) />
                            {row.attempt.map(|a| view! { <span class="text-xs text-muted">{a}</span> })}
                            {row.duration.map(|d| view! { <span class="text-xs text-muted">{d}</span> })}
                            <ProvenanceBadge provenance=provenance_of(row.source) />
                        </li>
                    }
                }
            </For>
        </ul>
    }
}

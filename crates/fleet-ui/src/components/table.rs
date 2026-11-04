// SPDX-License-Identifier: AGPL-3.0-only
//! A data table with a filter box, sortable headers and a column picker, over
//! [`TableState`]. Cells are text nodes.

use leptos::prelude::*;

use crate::table::{Direction, TableState};

/// A table of text cells. Headers are native buttons announcing `aria-sort`; the column picker
/// is a native `<details>` of checkboxes; the filter is a `type=search` input.
#[component]
pub fn Table(
    /// A short description of the table, used as its caption.
    #[prop(into)]
    caption: String,
    /// The column titles.
    columns: Vec<String>,
    /// The rows; each is a list of cell text in column order.
    #[prop(into)]
    rows: Signal<Vec<Vec<String>>>,
) -> impl IntoView {
    let state = RwSignal::new(TableState::new(columns.len()));
    let shown = Memo::new(move |_| rows.with(|r| state.with(|s| s.derive(r))));
    let columns = StoredValue::new(columns);
    let aria_sort = move |column: usize| {
        state.with(|s| {
            s.sort()
                .filter(|sort| sort.column == column)
                .map(|sort| match sort.direction {
                    Direction::Ascending => "ascending",
                    Direction::Descending => "descending",
                })
        })
    };
    view! {
        <div class="space-y-2">
            <div class="flex items-center gap-3">
                <input
                    type="search"
                    aria-label=format!("Filter {caption}")
                    placeholder="Filter…"
                    class="rounded border border-border bg-surface px-2 py-1"
                    on:input=move |ev| state.update(|s| s.set_filter(&event_target_value(&ev)))
                />
                <ColumnPicker columns=columns state=state />
            </div>
            <table class="w-full border-collapse text-left text-sm">
                <caption class="sr-only">{caption.clone()}</caption>
                <thead class="border-b border-border">
                    <tr>
                        {move || {
                            state
                                .with(TableState::visible_columns)
                                .into_iter()
                                .map(|c| {
                                    let title = columns.with_value(|t| t.get(c).cloned().unwrap_or_default());
                                    view! {
                                        <th scope="col" aria-sort=move || aria_sort(c).or(Some("none")) class="px-2 py-1">
                                            <button
                                                type="button"
                                                class="font-semibold hover:underline"
                                                on:click=move |_| state.update(|s| s.toggle_sort(c))
                                            >
                                                {title}
                                                {move || match aria_sort(c) {
                                                    Some("ascending") => " ▲",
                                                    Some(_) => " ▼",
                                                    None => "",
                                                }}
                                            </button>
                                        </th>
                                    }
                                })
                                .collect_view()
                        }}
                    </tr>
                </thead>
                <tbody>
                    <For
                        each=move || shown.get()
                        key=|i| *i
                        children=move |i| {
                            view! {
                                <tr class="border-b border-border">
                                    {move || {
                                        state
                                            .with(TableState::visible_columns)
                                            .into_iter()
                                            .map(|c| {
                                                let text = rows.with(|r| {
                                                    r.get(i).and_then(|row| row.get(c)).cloned().unwrap_or_default()
                                                });
                                                view! { <td class="px-2 py-1">{text}</td> }
                                            })
                                            .collect_view()
                                    }}
                                </tr>
                            }
                        }
                    />
                </tbody>
            </table>
        </div>
    }
}

/// The native `<details>` of checkboxes that shows and hides columns.
#[component]
fn ColumnPicker(columns: StoredValue<Vec<String>>, state: RwSignal<TableState>) -> impl IntoView {
    view! {
                <details class="relative">
                    <summary class="cursor-pointer rounded border border-border bg-surface px-2 py-1 text-sm">
                        "Columns"
                    </summary>
                    <ul class="absolute z-10 mt-1 rounded border border-border bg-surface p-2 shadow">
                        {columns
                            .get_value()
                            .into_iter()
                            .enumerate()
                            .map(|(i, title)| {
                                view! {
                                    <li>
                                        <label class="flex items-center gap-2">
                                            <input
                                                type="checkbox"
                                                prop:checked=move || state.with(|s| s.is_visible(i))
                                                on:change=move |_| state.update(|s| s.toggle_column(i))
                                            />
                                            {title}
                                        </label>
                                    </li>
                                }
                            })
                            .collect_view()}
                    </ul>
                </details>
    }
}

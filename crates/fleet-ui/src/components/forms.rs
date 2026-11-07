// SPDX-License-Identifier: AGPL-3.0-only
//! Form controls: combobox, date-time range and the URL-synced filter bar.

use leptos::{ev, prelude::*};
use time::UtcOffset;

use crate::{
    combobox::{ComboOption, Combobox as ComboModel},
    datetime_range::DateTimeRange as RangeModel,
    filter_bar::{FilterField, FilterKind, FilterSpec, FilterState},
    keyboard::Key,
};

const INPUT_CLASS: &str = "rounded border border-line-strong bg-bg px-2 py-1 text-sm text-fg";
/// A filter field is underlined, not boxed: the redesign's filter band (Foundations, History).
const FILTER_TEXT_CLASS: &str = "min-h-8 w-[200px] rounded-none border-0 border-b border-line-strong bg-transparent px-0.5 py-1 text-sm text-fg max-[720px]:w-full";
/// A select in the filter band.
const FILTER_SELECT_CLASS: &str = "min-h-8 w-[150px] rounded-none border-0 border-b border-line-strong bg-transparent px-0.5 py-1 text-sm text-fg max-[720px]:w-full";
/// A date-time field in the filter band: its numbers are monospace, like every number.
const FILTER_INSTANT_CLASS: &str = "min-h-8 w-[210px] rounded-none border-0 border-b border-line-strong bg-transparent px-0.5 py-1 font-mono text-[13px] text-fg max-[720px]:w-full";

/// An APG editable combobox with a listbox popup. `on_select` gets the chosen option's value, or
/// `None` when the selection is lost by editing.
#[component]
pub fn Combobox(
    /// Unique id prefix.
    #[prop(into)]
    id: String,
    /// The visible label.
    #[prop(into)]
    label: String,
    /// The choices.
    options: Vec<ComboOption>,
    /// Called when the selected value changes.
    on_select: Callback<Option<String>>,
) -> impl IntoView {
    let model = RwSignal::new(ComboModel::new(id.clone(), options));
    let report = move || on_select.run(model.with(|m| m.selected_value().map(str::to_owned)));
    view! {
        <div class="relative inline-block">
            <label for=id.clone() class="mb-1 block text-sm font-medium">{label}</label>
            <input
                id=id
                type="text"
                role="combobox"
                autocomplete="off"
                aria-autocomplete="list"
                aria-expanded=move || model.with(ComboModel::expanded).to_string()
                aria-controls=move || model.with(ComboModel::listbox_id)
                aria-activedescendant=move || model.with(ComboModel::active_descendant)
                class=INPUT_CLASS
                prop:value=move || model.with(|m| m.input().to_owned())
                on:input=move |ev| {
                    model.update(|m| m.set_input(&event_target_value(&ev)));
                    report();
                }
                on:keydown=move |ev: web_sys::KeyboardEvent| {
                    let mut handled = crate::combobox::ComboEvent::Ignored;
                    model.update(|m| handled = m.on_key(Key::from_dom(&ev.key())));
                    if handled == crate::combobox::ComboEvent::Handled {
                        ev.prevent_default();
                        report();
                    }
                }
                on:blur=move |_| {
                    model.update(ComboModel::blur);
                    report();
                }
            />
            <ul
                id=move || model.with(ComboModel::listbox_id)
                role="listbox"
                hidden=move || !model.with(ComboModel::expanded)
                class="absolute z-10 mt-1 max-h-60 w-full overflow-auto rounded border border-border bg-surface shadow-lg"
            >
                {move || {
                    let active = model.with(ComboModel::active);
                    model
                        .with(|m| m.filtered().into_iter().map(|o| o.label.clone()).collect::<Vec<_>>())
                        .into_iter()
                        .enumerate()
                        .map(|(i, text)| {
                            view! {
                                <li
                                    role="option"
                                    id=model.with(|m| m.option_id(i))
                                    aria-selected=(active == Some(i)).to_string()
                                    class="cursor-pointer px-2 py-1 text-sm aria-selected:bg-tone-info-bg hover:bg-tone-neutral-bg"
                                    on:mousedown=move |ev| ev.prevent_default()
                                    on:click=move |_| {
                                        model.update(|m| m.choose(i));
                                        report();
                                    }
                                >
                                    {text}
                                </li>
                            }
                        })
                        .collect_view()
                }}
            </ul>
        </div>
    }
}

/// Two native `datetime-local` inputs. The fields show wall-clock time in `offset` (labelled);
/// `on_change` gets the start and end as RFC 3339 UTC whenever the range is valid.
#[component]
pub fn DateTimeRange(
    /// Unique id prefix.
    #[prop(into)]
    id: String,
    /// The offset the fields are shown in; UTC by default.
    #[prop(default = UtcOffset::UTC)]
    offset: UtcOffset,
    /// Called with `(start, end)` in RFC 3339 UTC, either of which may be open.
    on_change: Callback<(Option<String>, Option<String>)>,
) -> impl IntoView {
    let model = RwSignal::new(RangeModel::new(offset));
    let announce = move || {
        model.with(|m| {
            if m.errors().is_empty() {
                on_change.run((m.start_rfc3339(), m.end_rfc3339()));
            }
        });
    };
    let (start_id, end_id) = (format!("{id}-start"), format!("{id}-end"));
    view! {
        <fieldset class="flex flex-wrap items-end gap-3">
            <legend class="mb-1 text-sm font-medium">
                "Time range (" {model.with_untracked(RangeModel::offset_label)} ")"
            </legend>
            <div>
                <label for=start_id.clone() class="block text-sm">"From"</label>
                <input
                    id=start_id
                    type="datetime-local"
                    class=INPUT_CLASS
                    on:change=move |ev| {
                        model.update(|m| m.set_start_text(&event_target_value(&ev)));
                        announce();
                    }
                />
            </div>
            <div>
                <label for=end_id.clone() class="block text-sm">"To"</label>
                <input
                    id=end_id
                    type="datetime-local"
                    class=INPUT_CLASS
                    on:change=move |ev| {
                        model.update(|m| m.set_end_text(&event_target_value(&ev)));
                        announce();
                    }
                />
            </div>
            <ul role="alert" class="text-sm text-tone-danger-fg">
                {move || {
                    model
                        .with(RangeModel::errors)
                        .into_iter()
                        .map(|e| view! { <li>{e.to_string()}</li> })
                        .collect_view()
                }}
            </ul>
        </fieldset>
    }
}

/// The current URL query (`location.search`) and a setter that replaces it without a reload.
/// Back and forward navigation update the signal.
pub fn use_url_query() -> (Signal<String>, Callback<String>) {
    let read = || window().location().search().unwrap_or_default();
    let query = RwSignal::new(read());
    let handle = window_event_listener(ev::popstate, move |_| query.set(read()));
    on_cleanup(move || handle.remove());
    let set = Callback::new(move |new: String| {
        let w = window();
        let path = w.location().pathname().unwrap_or_default();
        let url = if new.is_empty() {
            path
        } else {
            format!("{path}?{new}")
        };
        if let Ok(history) = w.history() {
            let _ = history.replace_state_with_url(
                &web_sys::wasm_bindgen::JsValue::NULL,
                "",
                Some(&url),
            );
        }
        query.set(read());
    });
    (query.into(), set)
}

/// A filter bar over a [`FilterSpec`]. `query` is the URL query string (see [`use_url_query`]);
/// every edit calls `on_change` with the new one, other parameters kept.
#[component]
pub fn FilterBar(
    /// The filters offered.
    spec: FilterSpec,
    /// The current query string.
    #[prop(into)]
    query: Signal<String>,
    /// Called with the new query string (no leading `?`).
    on_change: Callback<String>,
) -> impl IntoView {
    let spec = StoredValue::new(spec);
    let state = Memo::new(move |_| spec.with_value(|s| s.decode(&query.get())));
    let edit = Callback::new(move |(key, value): (String, String)| {
        let mut st: FilterState = state.get_untracked();
        spec.with_value(|s| {
            if st.set(s, &key, &value) {
                on_change.run(s.encode(&st));
            }
        });
    });
    let clear = move |_| {
        let mut st = state.get_untracked();
        st.clear();
        spec.with_value(|s| on_change.run(s.encode(&st)));
    };
    let fields = spec.with_value(|s| s.fields().to_vec());
    view! {
        <form role="search" aria-label="Filters" class="flex flex-wrap items-end gap-x-7 gap-y-5" on:submit=|ev| ev.prevent_default()>
            {fields
                .into_iter()
                .map(|f| {
                    let label = label_of(&f.key);
                    let for_id = format!("filter-{}", f.key);
                    view! {
                        <div class="flex min-w-0 flex-col gap-1">
                            <label for=for_id class="text-xs text-muted">{label}</label>
                            {filter_control(f, spec, state, edit)}
                        </div>
                    }
                })
                .collect_view()}
            <button
                type="button"
                class="inline-flex min-h-8 items-center rounded px-3 py-1.5 text-sm text-fg hover:bg-band max-[720px]:min-h-11"
                disabled=move || state.with(|s| s.active() == 0)
                on:click=clear
            >
                "Clear filters"
            </button>
        </form>
    }
}

/// The input for one filter: a search box, a select or a date-time field.
fn filter_control(
    field: FilterField,
    spec: StoredValue<FilterSpec>,
    state: Memo<FilterState>,
    edit: Callback<(String, String)>,
) -> AnyView {
    let key = field.key;
    let id = format!("filter-{key}");
    let value = {
        let key = key.clone();
        move || spec.with_value(|s| state.with(|st| st.get(s, &key).unwrap_or("").to_owned()))
    };
    match field.kind {
        FilterKind::Text => view! {
            <input
                id=id
                type="search"
                class=FILTER_TEXT_CLASS
                prop:value=value
                on:change=move |ev| edit.run((key.clone(), event_target_value(&ev)))
            />
        }
        .into_any(),
        FilterKind::Choice(options) => {
            let current = value.clone();
            view! {
                <select
                    id=id
                    class=FILTER_SELECT_CLASS
                    on:change=move |ev| edit.run((key.clone(), event_target_value(&ev)))
                >
                    <option value="" selected=value().is_empty()>"Any"</option>
                    {options
                        .into_iter()
                        .map(|o| {
                            let current = current.clone();
                            let (chosen, text) = (o.clone(), o.clone());
                            view! {
                                <option value=o selected=move || current() == chosen>
                                    {text}
                                </option>
                            }
                        })
                        .collect_view()}
                </select>
            }
            .into_any()
        }
        FilterKind::Instant => view! {
            <input
                id=id
                type="datetime-local"
                class=FILTER_INSTANT_CLASS
                prop:value=move || {
                    let mut r = RangeModel::new(UtcOffset::UTC);
                    r.set_start_rfc3339(&value());
                    r.start_text()
                }
                on:change=move |ev| {
                    let mut r = RangeModel::new(UtcOffset::UTC);
                    r.set_start_text(&event_target_value(&ev));
                    edit.run((key.clone(), r.start_rfc3339().unwrap_or_default()));
                }
            />
        }
        .into_any(),
    }
}

/// A label for a query key: the key with its first letter capitalised, except the outcome, which
/// the redesign calls its result.
fn label_of(key: &str) -> String {
    if key == "outcome" {
        return "Result".to_owned();
    }
    let mut c = key.chars();
    c.next()
        .map(|f| f.to_uppercase().chain(c).collect())
        .unwrap_or_default()
}

// SPDX-License-Identifier: AGPL-3.0-only
//! Rerunning failed flows (issue #82) in the dashboard: a flow's copyable Maestro command and the
//! download of an attempt's failed flows as a plan manifest for `fleet-agent run --plan`.
//!
//! The command is selectable text in a `<code>`, so it can be copied without the button; the button
//! puts the same text on the clipboard, as the other copy buttons do. Fleet never runs the command.

#![expect(
    clippy::must_use_candidate,
    reason = "Leptos's #[component] macro generates the props structs and the components return views"
)]

use fleet_protocol::rest::ReproCommandView;
use leptos::prelude::*;
use uuid::Uuid;

use crate::{paths::escape, screens::history::view::copy_text};

/// A small control, as on the history rows.
const SMALL_BUTTON: &str = "inline-flex min-h-8 items-center rounded border border-line-strong px-2.5 text-sm text-fg hover:bg-band max-[720px]:min-h-11";

/// The manifest of an attempt's failed flows: the API's download, read with the session's cookie.
#[must_use]
pub fn manifest_href(project: &str, attempt: Uuid) -> String {
    format!(
        "/api/v1/projects/{}/attempts/{attempt}/failed-flows",
        escape(project)
    )
}

/// What a failed history row offers: the flow's command and its attempt's manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RerunCell {
    /// The command, or why there is none.
    pub repro: ReproCommandView,
    /// Where the manifest of the row's attempt is.
    pub manifest: String,
}

/// The command, selectable, with the note on what was left out and what to fill in.
#[component]
fn CommandLine(
    /// The command and what it left out.
    repro: ReproCommandView,
) -> impl IntoView {
    let ReproCommandView {
        command,
        unavailable,
        left_out,
    } = repro;
    match command {
        Some(command) => {
            let copy = command.clone();
            let placeholders = command.contains("[REDACTED]").then_some(
                "Fill in each [REDACTED] value before you run it: Fleet never shows those values.",
            );
            let left = (!left_out.is_empty())
                .then(|| format!("Left out of the command: {}.", left_out.join(", ")));
            view! {
                <div class="space-y-2" data-testid="rerun-command">
                    <code class="block break-all font-mono text-[13px] text-fg select-all">{command}</code>
                    <div>
                        <button
                            type="button"
                            class=SMALL_BUTTON
                            aria-label="Copy maestro command"
                            data-testid="copy-command"
                            on:click=move |_| copy_text(&copy)
                        >
                            "Copy maestro command"
                        </button>
                    </div>
                    {left.map(|t| view! { <p class="text-sm text-muted" data-testid="rerun-left-out">{t}</p> })}
                    {placeholders.map(|t| view! { <p class="text-sm text-muted">{t}</p> })}
                </div>
            }
            .into_any()
        }
        None => view! {
            <p class="text-sm text-muted" data-testid="rerun-unavailable">
                {unavailable.unwrap_or_else(|| "Fleet can't give a rerun command for this flow.".to_owned())}
            </p>
        }
        .into_any(),
    }
}

/// Screen D's rerun section: this flow's command, and the download of its attempt's failed flows.
#[component]
pub fn RerunSection(
    /// The command for this flow.
    repro: ReproCommandView,
    /// Where the attempt's failed flows download from (see [`manifest_href`]).
    manifest: String,
) -> impl IntoView {
    view! {
        <section class="space-y-3 border-t border-line pt-4" aria-labelledby="rerun-h" data-testid="rerun">
            <h2 id="rerun-h" class="text-base font-semibold">"Rerun"</h2>
            <CommandLine repro />
            <p class="text-sm">
                <a class="text-primary underline" data-testid="failed-flows-link" href=manifest>
                    "Download the failed flows of this attempt"
                </a>
                <span class="text-muted">" as a plan manifest for "</span>
                <code class="font-mono text-[13px]">"fleet-agent run --plan"</code>
            </p>
        </section>
    }
}

/// A failed history row's rerun disclosure: the command behind a summary, and the manifest.
#[component]
pub fn RerunDisclosure(
    /// What the row offers.
    cell: RerunCell,
) -> impl IntoView {
    view! {
        <details class="mt-1.5 text-[12.5px] text-muted" data-testid="rerun-details">
            <summary class="inline-flex min-h-8 cursor-pointer items-center text-fg underline decoration-line-strong underline-offset-[3px]">
                "Rerun"
            </summary>
            <div class="mt-1.5 space-y-2">
                <CommandLine repro=cell.repro />
                <p>
                    <a class="text-primary underline" data-testid="failed-flows-link" href=cell.manifest>
                        "Failed flows of this attempt"
                    </a>
                </p>
            </div>
        </details>
    }
}

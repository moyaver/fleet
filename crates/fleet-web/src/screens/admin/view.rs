// SPDX-License-Identifier: AGPL-3.0-only
//! Screen G's tables and sections. Each area is a section with a rule above it, a short note, and
//! a table in the same scroll box and markup as the history table (`tbl`, `tbl-cards`). Every cell
//! is text: a value a user or an agent supplied is shown as the text it is.

use fleet_protocol::rest::admin::{
    AdminAgentRow, AdminApiKeyView, AdminArtifactClassView, AdminGapRow, AdminGroupMappingView,
    AdminIngestionView, AdminLagRow, AdminProjectLimitsView, AdminProjectView, AdminRejectionRow,
    AdminRetentionClassView, AdminRoleBindingView, AdminSettingView, AdminTrustPolicyView,
};
use fleet_ui::{
    charts::format::format_duration,
    components::display::{EmptyState, TableBox},
};
use leptos::prelude::*;

use super::model::{
    key_status, lag_text, scope_text, seconds_ago_text, seq_range, set_by_text, source_text,
    ttl_text,
};
use crate::{
    app::ErrorNotice,
    clock::{now_ms, unix_ms, utc_offset_minutes},
    screens::{
        common::Ctx,
        parts::{Fetched, loaded},
    },
};

/// The page's context for its times: the viewer's clock and offset.
#[must_use]
pub fn ctx() -> Ctx {
    Ctx {
        project: String::new(),
        now_ms: now_ms(),
        utc_offset_minutes: utc_offset_minutes(),
    }
}

/// A table of text cells under `headers`. On a phone each cell is labelled by its header.
#[must_use]
pub fn grid(
    caption: &'static str,
    headers: &'static [&'static str],
    rows: Vec<Vec<String>>,
) -> AnyView {
    view! {
        <TableBox label=caption>
            <table class="tbl tbl-cards">
                <caption class="sr-only">{caption}</caption>
                <thead>
                    <tr>
                        {headers
                            .iter()
                            .map(|h| view! { <th scope="col">{*h}</th> })
                            .collect_view()}
                    </tr>
                </thead>
                <tbody>
                    {rows
                        .into_iter()
                        .map(|row| {
                            view! {
                                <tr>
                                    {row
                                        .into_iter()
                                        .zip(headers.iter())
                                        .map(|(cell, h)| view! { <td data-label=*h>{cell}</td> })
                                        .collect_view()}
                                </tr>
                            }
                        })
                        .collect_view()}
                </tbody>
            </table>
        </TableBox>
    }
    .into_any()
}

/// A table whose last column holds each row's actions (a form or a disclosure), under the
/// `headers`' last name. `actions` has one entry per row.
#[must_use]
pub fn grid_actions(
    caption: &'static str,
    headers: &'static [&'static str],
    rows: Vec<Vec<String>>,
    actions: Vec<AnyView>,
) -> AnyView {
    view! {
        <TableBox label=caption>
            <table class="tbl tbl-cards">
                <caption class="sr-only">{caption}</caption>
                <thead>
                    <tr>
                        {headers
                            .iter()
                            .map(|h| view! { <th scope="col">{*h}</th> })
                            .collect_view()}
                    </tr>
                </thead>
                <tbody>
                    {rows
                        .into_iter()
                        .zip(actions)
                        .map(|(row, action)| {
                            let last = headers.last().copied().unwrap_or_default();
                            view! {
                                <tr>
                                    {row
                                        .into_iter()
                                        .zip(headers.iter())
                                        .map(|(cell, h)| view! { <td data-label=*h>{cell}</td> })
                                        .collect_view()}
                                    <td data-label=last>{action}</td>
                                </tr>
                            }
                        })
                        .collect_view()}
                </tbody>
            </table>
        </TableBox>
    }
    .into_any()
}

/// [`grid_actions`], or an empty state saying what would fill the table.
#[must_use]
pub fn grid_actions_or_empty(
    caption: &'static str,
    headers: &'static [&'static str],
    rows: Vec<Vec<String>>,
    actions: Vec<AnyView>,
    empty: &'static str,
) -> AnyView {
    if rows.is_empty() {
        view! { <EmptyState title=empty /> }.into_any()
    } else {
        grid_actions(caption, headers, rows, actions)
    }
}

/// A table, or an empty state saying what would fill it.
#[must_use]
pub fn grid_or_empty(
    caption: &'static str,
    headers: &'static [&'static str],
    rows: Vec<Vec<String>>,
    empty: &'static str,
) -> AnyView {
    if rows.is_empty() {
        view! { <EmptyState title=empty /> }.into_any()
    } else {
        grid(caption, headers, rows)
    }
}

/// One area: its rule, heading, note, and its data once it has arrived. A refusal or a failed
/// fetch shows its message beside the section, and data already on screen stays.
#[must_use]
pub fn section<T: Clone + Send + Sync + 'static>(
    id: &'static str,
    title: &'static str,
    note: &'static str,
    fetched: Fetched<T>,
    ready: impl Fn(T) -> AnyView + Clone + Send + 'static,
) -> AnyView {
    let heading = format!("{id}-heading");
    let labelled_by = heading.clone();
    let testid = format!("admin-{id}");
    view! {
        <section id=id data-testid=testid aria-labelledby=labelled_by class="mt-9 border-t border-line pt-4">
            <div class="flex flex-wrap items-baseline gap-x-4 gap-y-1 pb-3">
                <h3 id=heading class="text-[15px] font-semibold">{title}</h3>
                <p class="note">{note}</p>
            </div>
            <ErrorNotice error=fetched.error />
            {move || loaded(fetched, ready.clone())}
        </section>
    }
    .into_any()
}

/// Projects, for onboarding.
#[must_use]
pub fn projects_rows(rows: &[AdminProjectView], ctx: &Ctx) -> Vec<Vec<String>> {
    rows.iter()
        .map(|p| {
            vec![
                p.slug.clone(),
                p.name.clone(),
                p.default_branch.clone(),
                p.active_api_keys.to_string(),
                p.active_trust_policies.to_string(),
                if p.archived_at.is_some() {
                    "archived"
                } else {
                    "active"
                }
                .to_owned(),
                ctx.time(unix_ms(p.created_at)),
            ]
        })
        .collect()
}

/// Publisher trust policies: who may publish with a CI token, and what they match.
#[must_use]
pub fn trust_rows(rows: &[AdminTrustPolicyView], ctx: &Ctx) -> Vec<Vec<String>> {
    rows.iter()
        .map(|t| {
            vec![
                t.project.clone(),
                t.name.clone(),
                t.issuer.clone(),
                t.repository_id.clone(),
                t.repository_owner_id.clone(),
                if t.trusted_refs.is_empty() {
                    "none".to_owned()
                } else {
                    t.trusted_refs.join(", ")
                },
                if t.allowed_events.is_empty() {
                    "default set".to_owned()
                } else {
                    t.allowed_events.join(", ")
                },
                t.environment.clone().unwrap_or_else(|| "any".to_owned()),
                t.pr_policy.clone(),
                if t.revoked_at.is_some() {
                    "revoked"
                } else {
                    "active"
                }
                .to_owned(),
                source_text(&t.source),
                ctx.time(unix_ms(t.created_at)),
            ]
        })
        .collect()
}

/// API keys: what identifies them and whether they work. The secret is never in the data.
#[must_use]
pub fn key_rows(rows: &[AdminApiKeyView], ctx: &Ctx) -> Vec<Vec<String>> {
    let now = time::OffsetDateTime::from_unix_timestamp(ctx.now_ms.div_euclid(1000))
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
    rows.iter()
        .map(|k| {
            vec![
                k.project.clone(),
                k.name.clone(),
                k.prefix.clone(),
                k.trust_level.clone(),
                key_status(k.revoked_at, k.expires_at, now).to_owned(),
                k.expires_at
                    .map_or_else(|| "—".to_owned(), |t| ctx.time(unix_ms(t))),
                k.created_by.clone().unwrap_or_else(|| "—".to_owned()),
                k.last_used_at
                    .map_or_else(|| "never".to_owned(), |t| ctx.ago(unix_ms(t))),
                ctx.time(unix_ms(k.created_at)),
            ]
        })
        .collect()
}

/// Roles bound to people.
#[must_use]
pub fn binding_rows(rows: &[AdminRoleBindingView], ctx: &Ctx) -> Vec<Vec<String>> {
    rows.iter()
        .map(|b| {
            vec![
                b.user.clone(),
                scope_text(b.project.as_deref()),
                b.role.clone(),
                ctx.time(unix_ms(b.created_at)),
            ]
        })
        .collect()
}

/// Identity-provider groups that grant roles.
#[must_use]
pub fn group_rows(rows: &[AdminGroupMappingView]) -> Vec<Vec<String>> {
    rows.iter()
        .map(|g| {
            vec![
                g.issuer.clone(),
                g.group.clone(),
                scope_text(g.project.as_deref()),
                g.role.clone(),
                source_text(&g.source),
            ]
        })
        .collect()
}

/// Retention classes: each project's TTL per artifact class.
#[must_use]
pub fn class_rows(rows: &[AdminRetentionClassView]) -> Vec<Vec<String>> {
    rows.iter()
        .map(|c| vec![c.project.clone(), c.class.clone(), ttl_text(c.ttl_seconds)])
        .collect()
}

/// Configuration settings: key, value and where the value comes from.
#[must_use]
pub fn setting_rows(rows: &[AdminSettingView]) -> Vec<Vec<String>> {
    rows.iter()
        .map(|s| vec![s.key.clone(), s.value.clone(), s.source.clone()])
        .collect()
}

/// Each project's artifact classes: whether it accepts them, its largest file, the configured cap
/// and where the limit comes from.
#[must_use]
pub fn artifact_class_rows(rows: &[AdminArtifactClassView]) -> Vec<Vec<String>> {
    rows.iter()
        .map(|c| {
            vec![
                c.project.clone(),
                c.class.clone(),
                if c.accepted { "yes" } else { "no" }.to_owned(),
                c.max_bytes.to_string(),
                c.configured_max_bytes.to_string(),
                set_by_text(c.overridden).to_owned(),
            ]
        })
        .collect()
}

/// Each project's daily quota: bytes and files, each with where its limit comes from.
#[must_use]
pub fn quota_rows(rows: &[AdminProjectLimitsView]) -> Vec<Vec<String>> {
    rows.iter()
        .map(|p| {
            vec![
                p.project.clone(),
                format!(
                    "{} bytes ({})",
                    p.bytes_per_day,
                    set_by_text(p.bytes_per_day_overridden)
                ),
                format!(
                    "{} files ({})",
                    p.files_per_day,
                    set_by_text(p.files_per_day_overridden)
                ),
            ]
        })
        .collect()
}

/// Each project's metric intervals, calm and under pressure, each with where it comes from.
#[must_use]
pub fn metric_rows(rows: &[AdminProjectLimitsView]) -> Vec<Vec<String>> {
    rows.iter()
        .map(|p| {
            vec![
                p.project.clone(),
                format!(
                    "{} ({})",
                    format_duration(p.sample_interval_ms),
                    set_by_text(p.sample_interval_overridden)
                ),
                format!(
                    "{} ({})",
                    format_duration(p.pressure_sample_interval_ms),
                    set_by_text(p.pressure_sample_interval_overridden)
                ),
            ]
        })
        .collect()
}

/// Agent versions seen, and whether this server speaks their protocol.
#[must_use]
pub fn agent_rows(rows: &[AdminAgentRow], ctx: &Ctx) -> Vec<Vec<String>> {
    rows.iter()
        .map(|a| {
            vec![
                a.agent_version.clone(),
                a.protocol_version.to_string(),
                a.runs.to_string(),
                a.open_runs.to_string(),
                ctx.time(unix_ms(a.last_started_at)),
                if a.compatible { "yes" } else { "no" }.to_owned(),
            ]
        })
        .collect()
}

/// Refused events, newest first.
#[must_use]
pub fn rejection_rows(rows: &[AdminRejectionRow], ctx: &Ctx) -> Vec<Vec<String>> {
    rows.iter()
        .map(|r| {
            vec![
                ctx.time(unix_ms(r.rejected_at)),
                r.project.clone(),
                r.agent_run_id.to_string(),
                r.agent_version.clone(),
                r.seq.to_string(),
                r.code.clone(),
                r.message.clone().unwrap_or_else(|| "—".to_owned()),
            ]
        })
        .collect()
}

/// Losses the agents reported, newest first.
#[must_use]
pub fn gap_rows(rows: &[AdminGapRow], ctx: &Ctx) -> Vec<Vec<String>> {
    rows.iter()
        .map(|g| {
            vec![
                ctx.time(unix_ms(g.recorded_at)),
                g.project.clone(),
                g.agent_run_id.to_string(),
                g.stream.clone(),
                seq_range(g.from_seq, g.to_seq),
                g.reason.clone(),
            ]
        })
        .collect()
}

/// Open runs: how long since the server last heard from each, and how far behind its events are.
#[must_use]
pub fn lag_rows(rows: &[AdminLagRow], ctx: &Ctx) -> Vec<Vec<String>> {
    rows.iter()
        .map(|r| {
            vec![
                r.project.clone(),
                r.agent_run_id.to_string(),
                r.agent_version.clone(),
                r.state.clone(),
                seconds_ago_text(r.seconds_since_frame),
                r.committed_seq.to_string(),
                lag_text(r.newest_event_lag_ms),
                ctx.time(unix_ms(r.started_at)),
            ]
        })
        .collect()
}

/// The ingestion page's three tables, each with its own empty state.
#[must_use]
pub fn ingestion_view(view: &AdminIngestionView, ctx: &Ctx) -> AnyView {
    let rejections = rejection_rows(&view.rejections, ctx);
    let gaps = gap_rows(&view.gaps, ctx);
    let lags = lag_rows(&view.open_runs, ctx);
    view! {
        <div class="space-y-6">
            <div>
                <h4 class="pb-2 text-sm font-semibold">"Refused events"</h4>
                {grid_or_empty(
                    "Refused events, newest first",
                    &["When", "Project", "Run", "Agent", "Seq", "Code", "Message"],
                    rejections,
                    "No event has been refused",
                )}
            </div>
            <div>
                <h4 class="pb-2 text-sm font-semibold">"Reported losses"</h4>
                {grid_or_empty(
                    "Losses the agents reported, newest first",
                    &["When", "Project", "Run", "Stream", "Missing", "Reason"],
                    gaps,
                    "No loss has been reported",
                )}
            </div>
            <div>
                <h4 class="pb-2 text-sm font-semibold">"Open runs"</h4>
                {grid_or_empty(
                    "Open runs, silent longest first",
                    &["Project", "Run", "Agent", "State", "Last frame", "Committed seq", "Newest event lag", "Started"],
                    lags,
                    "No run is open",
                )}
            </div>
        </div>
    }
    .into_any()
}
#[cfg(test)]
mod tests {
    use fleet_protocol::rest::admin::{AdminArtifactClassView, AdminProjectLimitsView};

    use super::{artifact_class_rows, metric_rows, quota_rows};

    fn class(accepted: bool, overridden: bool) -> AdminArtifactClassView {
        AdminArtifactClassView {
            project: "app".into(),
            class: "junit".into(),
            accepted,
            max_bytes: 1_024,
            configured_max_bytes: 4_096,
            overridden,
        }
    }

    fn limits() -> AdminProjectLimitsView {
        AdminProjectLimitsView {
            project: "app".into(),
            bytes_per_day: 2_048,
            bytes_per_day_overridden: true,
            files_per_day: 300,
            files_per_day_overridden: false,
            sample_interval_ms: 2_500,
            sample_interval_overridden: false,
            pressure_sample_interval_ms: 15_000,
            pressure_sample_interval_overridden: true,
        }
    }

    #[test]
    fn a_class_row_shows_its_limit_the_cap_and_where_the_limit_comes_from() {
        assert_eq!(
            artifact_class_rows(&[class(true, true)]),
            vec![vec!["app", "junit", "yes", "1024", "4096", "dashboard"]]
        );
        assert_eq!(
            artifact_class_rows(&[class(false, false)]),
            vec![vec!["app", "junit", "no", "1024", "4096", "from config"]]
        );
    }

    #[test]
    fn a_quota_row_names_the_source_of_each_limit() {
        assert_eq!(
            quota_rows(&[limits()]),
            vec![vec![
                "app",
                "2048 bytes (dashboard)",
                "300 files (from config)"
            ]]
        );
    }

    #[test]
    fn a_metric_row_shows_the_interval_in_force_and_its_source() {
        assert_eq!(
            metric_rows(&[limits()]),
            vec![vec!["app", "2.5 s (from config)", "15 s (dashboard)"]]
        );
    }
}

// SPDX-License-Identifier: AGPL-3.0-only
//! The flow catalog (issue #85): every flow the project knows, whether it has run or not.
//!
//! A flow becomes known from three places. The agent's workspace catalog (`hello.catalog`) names
//! the flow files it found, with their tags; a stored plan names the flows it assigns; and a run
//! reports a flow by its JUnit file. The first two write `flow_definitions` rows with no executions,
//! which is what makes a never-run flow listable. Every write is idempotent, and a path that isn't a
//! repository-relative path is dropped, never stored (I3: nothing is recorded that no file says).

use std::collections::{BTreeMap, BTreeSet};

use fleet_protocol::{
    frame::{
        FlowCatalog, MAX_CATALOG_FLOWS, MAX_CATALOG_PATH_BYTES, MAX_CATALOG_TAG_BYTES,
        MAX_CATALOG_TAGS,
    },
    rest::history::{CatalogFlowView, CatalogLastRunView, FlowCatalogPage},
};
use sqlx::{Postgres, QueryBuilder};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{Result, Store, screens::Filters};

/// Whether `path` names a file inside the repository: relative, with no `..` and no `\` or `:`.
fn repo_relative(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= MAX_CATALOG_PATH_BYTES
        && !path.contains(['\\', ':'])
        && std::path::Path::new(path)
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
}

/// The tags a header may carry, bounded as the agent bounds them.
fn bounded_tags(tags: &[String]) -> Vec<String> {
    tags.iter()
        .filter(|t| !t.is_empty() && t.len() <= MAX_CATALOG_TAG_BYTES)
        .take(MAX_CATALOG_TAGS)
        .cloned()
        .collect()
}

/// The name a flow is listed under when its header has none: its file name without extension, as
/// Maestro names it.
fn stem_of(path: &str) -> String {
    std::path::Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(path)
        .to_owned()
}

/// Records the workspace catalog of a run: each flow's name and tags, replacing the tags it had.
/// A flow the run's workspace no longer has keeps its row and its history; only its tags are
/// refreshed from what the workspace says now.
pub(crate) async fn upsert_catalog(
    tx: &mut sqlx::PgConnection,
    project: Uuid,
    catalog: &FlowCatalog,
) -> Result<()> {
    for flow in catalog.flows.iter().take(MAX_CATALOG_FLOWS) {
        if !repo_relative(&flow.path) {
            tracing::warn!(path = %flow.path, "catalog flow is not a repository path; not recorded");
            continue;
        }
        let name = flow.name.clone().unwrap_or_else(|| stem_of(&flow.path));
        let tags = bounded_tags(&flow.tags);
        sqlx::query!(
            "INSERT INTO flow_definitions (project_id, repo_path, last_name, tags)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (project_id, repo_path)
             DO UPDATE SET tags = EXCLUDED.tags, last_name = coalesce(flow_definitions.last_name, EXCLUDED.last_name)",
            project,
            flow.path,
            name,
            &tags
        )
        .execute(&mut *tx)
        .await?;
    }
    Ok(())
}

/// Records the flows a stored plan assigns, so a flow the plan will run is listed before its first
/// run. Tags a plan doesn't know are left as they are.
pub(crate) async fn upsert_plan_flows(
    tx: &mut sqlx::PgConnection,
    project: Uuid,
    flows: &[&str],
) -> Result<()> {
    for path in flows {
        if !repo_relative(path) {
            continue;
        }
        sqlx::query!(
            "INSERT INTO flow_definitions (project_id, repo_path, last_name) VALUES ($1, $2, $3)
             ON CONFLICT (project_id, repo_path) DO NOTHING",
            project,
            path,
            stem_of(path)
        )
        .execute(&mut *tx)
        .await?;
    }
    Ok(())
}

/// The catalog's flows, keyed by identity: each one's name and tags.
type Known = BTreeMap<String, (String, Vec<String>)>;

/// The known flow definitions of a project: identity to (name, tags).
async fn definitions(tx: &mut sqlx::PgConnection, project: Uuid) -> Result<Known> {
    let rows = sqlx::query!(
        "SELECT repo_path, coalesce(last_name, repo_path) AS \"name!\", tags
           FROM flow_definitions WHERE project_id = $1",
        project
    )
    .fetch_all(&mut *tx)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| (r.repo_path, (r.name, r.tags)))
        .collect())
}

/// What the flow catalog lists (issue #85).
#[derive(Debug, Clone, Copy)]
pub struct CatalogQuery<'a> {
    /// The filters. The flow's text and tag narrow flows; any other filter keeps only the flows
    /// with an execution in the window that passes all of them, so a never-run flow is listed only
    /// when no such filter is set.
    pub filters: Filters<'a>,
    /// Start of the window the execution filters look at, inclusive.
    pub from: OffsetDateTime,
    /// End of that window, exclusive.
    pub to: OffsetDateTime,
    /// The identity of the last flow of the previous page.
    pub after: Option<&'a str>,
    /// Flows per page.
    pub limit: i64,
}

#[derive(sqlx::FromRow)]
struct CountRow {
    flow: String,
    executions: i64,
    name: String,
}

#[derive(sqlx::FromRow)]
struct LastRow {
    flow: String,
    flow_id: Uuid,
    state: String,
    finality: String,
    outcome_source: Option<String>,
    at: Option<OffsetDateTime>,
}

impl Store {
    /// One page of the project's flow catalog, sorted by flow identity: every flow the project knows
    /// (a workspace or plan names it, or a run executed it), with its tags, how many executions it
    /// has ever had (zero is "never run"), and its latest finished execution. The page's
    /// `next_cursor` is the identity of its last flow.
    pub async fn flow_catalog(
        &self,
        project: Uuid,
        q: &CatalogQuery<'_>,
    ) -> Result<FlowCatalogPage> {
        let mut tx = self.pool().begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *tx)
            .await?;
        let known = definitions(&mut tx, project).await?;
        let counts: Vec<CountRow> = sqlx::query_as(
            "SELECT coalesce(d.repo_path, f.flow_name) AS flow, count(*) AS executions,
                    max(f.flow_name) AS name
               FROM flow_executions f
               LEFT JOIN flow_definitions d ON d.id = f.flow_definition_id
              WHERE f.project_id = $1
              GROUP BY 1",
        )
        .bind(project)
        .fetch_all(&mut *tx)
        .await?;
        let lasts: Vec<LastRow> = sqlx::query_as(
            "SELECT DISTINCT ON (flow) flow, flow_id, state, finality, outcome_source, at
               FROM (SELECT coalesce(d.repo_path, f.flow_name) AS flow, f.id AS flow_id, f.state,
                            f.finality::text AS finality, f.outcome_source::text AS outcome_source,
                            coalesce(f.ended_at, f.started_at) AS at
                       FROM flow_executions f
                       LEFT JOIN flow_definitions d ON d.id = f.flow_definition_id
                      WHERE f.project_id = $1 AND f.state NOT IN ('pending', 'running')) x
              ORDER BY flow, at DESC, flow_id DESC",
        )
        .bind(project)
        .fetch_all(&mut *tx)
        .await?;
        let matching = matching_flows(&mut tx, project, q).await?;
        tx.commit().await?;

        let mut flows: BTreeMap<String, CatalogFlowView> = BTreeMap::new();
        for (flow, (name, tags)) in known {
            flows.insert(
                flow.clone(),
                CatalogFlowView {
                    flow,
                    name,
                    tags,
                    executions: 0,
                    last: None,
                },
            );
        }
        for c in counts {
            let entry = flows
                .entry(c.flow.clone())
                .or_insert_with(|| CatalogFlowView {
                    flow: c.flow.clone(),
                    name: c.name.clone(),
                    tags: Vec::new(),
                    executions: 0,
                    last: None,
                });
            entry.executions = c.executions;
        }
        for l in lasts {
            if let (Some(entry), Some(at)) = (flows.get_mut(&l.flow), l.at) {
                entry.last = Some(CatalogLastRunView {
                    flow_id: l.flow_id,
                    state: l.state,
                    finality: l.finality,
                    outcome_source: l.outcome_source,
                    at,
                });
            }
        }

        let flow_text = q.filters.flow.map(str::to_lowercase);
        let mut items = Vec::new();
        let mut more = false;
        for (identity, entry) in flows {
            if q.after.is_some_and(|after| identity.as_str() <= after) {
                continue;
            }
            let keep = flow_text.as_ref().is_none_or(|text| {
                identity.to_lowercase().contains(text) || entry.name.to_lowercase().contains(text)
            }) && q
                .filters
                .tag
                .is_none_or(|tag| entry.tags.iter().any(|t| t.eq_ignore_ascii_case(tag)))
                && matching.as_ref().is_none_or(|set| set.contains(&identity));
            if !keep {
                continue;
            }
            if i64::try_from(items.len()).is_ok_and(|n| n >= q.limit) {
                more = true;
                break;
            }
            items.push(entry);
        }
        let next_cursor = more.then(|| items.last().map(|e| e.flow.clone())).flatten();
        Ok(FlowCatalogPage { items, next_cursor })
    }
}

/// The flows with an execution in the window that passes every execution filter of `q`; `None`
/// when no such filter is set, so every flow may be listed.
async fn matching_flows(
    tx: &mut sqlx::PgConnection,
    project: Uuid,
    q: &CatalogQuery<'_>,
) -> Result<Option<BTreeSet<String>>> {
    if !q.filters.narrows_executions() {
        return Ok(None);
    }
    let mut qb = QueryBuilder::<Postgres>::new(
        "SELECT DISTINCT coalesce(d.repo_path, f.flow_name) AS flow ",
    );
    qb.push(crate::screens::FROM_EXECUTIONS_CLAUSE)
        .push(" WHERE f.project_id = ")
        .push_bind(project)
        .push(" AND f.state NOT IN ('pending', 'running')")
        .push(" AND coalesce(f.ended_at, f.started_at) >= ")
        .push_bind(q.from)
        .push(" AND coalesce(f.ended_at, f.started_at) < ")
        .push_bind(q.to);
    crate::screens::push_filter_clauses(&mut qb, &q.filters);
    let flows: Vec<String> = qb.build_query_scalar().fetch_all(&mut *tx).await?;
    Ok(Some(flows.into_iter().collect()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_repository_relative_paths_are_recorded() {
        assert!(repo_relative("flows/login.yaml"));
        assert!(repo_relative("app/flows/a.yml"));
        assert!(!repo_relative("/etc/passwd"));
        assert!(!repo_relative("../secret.yaml"));
        assert!(!repo_relative("flows\\a.yaml"));
        assert!(!repo_relative("c:flows.yaml"));
        assert!(!repo_relative(""));
    }

    #[test]
    fn tags_are_bounded_and_a_missing_name_is_the_file_stem() {
        let tags: Vec<String> = (0..40).map(|i| format!("t{i}")).collect();
        assert_eq!(bounded_tags(&tags).len(), MAX_CATALOG_TAGS);
        assert_eq!(
            bounded_tags(&[String::new(), "x".repeat(65), "ok".into()]),
            ["ok"]
        );
        assert_eq!(stem_of("flows/checkout.yaml"), "checkout");
    }
}

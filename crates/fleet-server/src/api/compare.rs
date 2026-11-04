// SPDX-License-Identifier: AGPL-3.0-only
//! Screen F's REST read (PLAN §10.3): two attempts compared flow by flow, with the mismatches
//! explained.
//!
//! An explanation is a fact the two sides' stored evidence states: different condition
//! fingerprints (with the dimensions that differ), different failure fingerprints, a flow one
//! side never executed, outcomes resting on different provenance. When the evidence says nothing
//! differs, the row has no explanation; a reason is never invented. Authorization is the
//! history's: `ReadRuns` on the project, and an attempt of another project reads as missing.

use std::collections::BTreeMap;

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use fleet_protocol::rest::{
    FlowCounts,
    history::{
        ChangeKind, CompareFlowView, CompareRowView, CompareSideView, CompareView, DimChangeView,
        EvidenceView, Explanation, Side,
    },
};
use fleet_store::screens::{CompareSide, SideFlow};
use serde::Deserialize;
use uuid::Uuid;

use super::{ApiError, error, internal, read_project};
use crate::{session::Caller, state::AppState};

#[derive(Debug, Deserialize)]
pub(super) struct CompareParams {
    a: Uuid,
    b: Uuid,
}

/// Screen F: every flow either attempt executed, A against B, with evidence-based explanations.
#[utoipa::path(get, path = "/api/v1/projects/{project}/compare",
    params(("project" = String, Path, description = "Project slug"),
           ("a" = Uuid, Query, description = "The first pipeline attempt"),
           ("b" = Uuid, Query, description = "The second pipeline attempt")),
    responses((status = 200, body = CompareView), (status = 404, body = ApiError)))]
pub(super) async fn compare(
    State(state): State<AppState>,
    Caller(principal): Caller,
    Path(project): Path<String>,
    Query(q): Query<CompareParams>,
) -> Response {
    let project = match read_project(&state, &principal, &project).await {
        Ok(p) => p,
        Err(r) => return *r,
    };
    match state.store.compare(project, q.a, q.b).await {
        Ok(Some((a, b))) => {
            let truncated = a.truncated || b.truncated;
            Json(CompareView {
                rows: rows(&a.flows, &b.flows),
                truncated,
                a: side(a),
                b: side(b),
            })
            .into_response()
        }
        Ok(None) => error(StatusCode::NOT_FOUND, "no such attempt"),
        Err(e) => internal(&e),
    }
}

fn side(s: CompareSide) -> CompareSideView {
    CompareSideView {
        flows: counts(&s),
        attempt: s.attempt,
    }
}

fn counts(s: &CompareSide) -> FlowCounts {
    let mut c = FlowCounts::default();
    for f in &s.flows {
        let n = match f.view.state.as_str() {
            "pending" => &mut c.pending,
            "running" => &mut c.running,
            "passed" => &mut c.passed,
            "failed" => &mut c.failed,
            "skipped" => &mut c.skipped,
            "interrupted" => &mut c.interrupted,
            _ => &mut c.unknown,
        };
        *n += 1;
    }
    c
}

/// A against B, one row per flow, in flow order.
///
/// A flow is its repository path once a final report named one, and its name before that. So a
/// flow one side only knows by name is the same flow as the other side's path-identified one
/// when no other flow of either side could be it: same name, one candidate on each side.
fn rows(a: &[SideFlow], b: &[SideFlow]) -> Vec<CompareRowView> {
    let mut by_flow: BTreeMap<&str, (Option<&SideFlow>, Option<&SideFlow>)> = BTreeMap::new();
    for f in a {
        by_flow.entry(&f.flow).or_default().0 = Some(f);
    }
    for f in b {
        by_flow.entry(&f.flow).or_default().1 = Some(f);
    }
    let only = |side: usize| -> Vec<&SideFlow> {
        by_flow
            .values()
            .filter_map(|(x, y)| match (x, y) {
                (Some(x), None) if side == 0 => Some(*x),
                (None, Some(y)) if side == 1 => Some(*y),
                _ => None,
            })
            .collect()
    };
    let (only_a, only_b) = (only(0), only(1));
    let named =
        |list: &[&SideFlow], name: &str| list.iter().filter(|f| f.view.name == name).count();
    let mut merged = Vec::new();
    for fa in &only_a {
        let name = fa.view.name.as_str();
        let candidate = only_b.iter().find(|fb| fb.view.name == name);
        if let Some(fb) = candidate
            && named(&only_a, name) == 1
            && named(&only_b, name) == 1
            && (fa.flow == name || fb.flow == name)
        {
            merged.push((*fa, *fb));
        }
    }
    for (fa, fb) in merged {
        by_flow.remove(fa.flow.as_str());
        by_flow.remove(fb.flow.as_str());
        let key = if fa.flow == fa.view.name {
            fb.flow.as_str()
        } else {
            fa.flow.as_str()
        };
        by_flow.insert(key, (Some(fa), Some(fb)));
    }
    by_flow
        .into_iter()
        .map(|(flow, (a, b))| row(flow, a, b))
        .collect()
}

fn row(flow: &str, a: Option<&SideFlow>, b: Option<&SideFlow>) -> CompareRowView {
    let (change, explanations) = match (a, b) {
        (Some(a), Some(b)) => (change(&a.view, &b.view), explain(a, b)),
        (Some(_), None) => (
            ChangeKind::OnlyInA,
            vec![Explanation::FlowMissing { side: Side::B }],
        ),
        (None, Some(_)) => (
            ChangeKind::OnlyInB,
            vec![Explanation::FlowMissing { side: Side::A }],
        ),
        (None, None) => (ChangeKind::Unchanged, Vec::new()),
    };
    CompareRowView {
        flow: flow.to_owned(),
        change,
        a: a.map(|f| f.view.clone()),
        b: b.map(|f| f.view.clone()),
        explanations,
    }
}

fn change(a: &CompareFlowView, b: &CompareFlowView) -> ChangeKind {
    match (a.state.as_str(), b.state.as_str()) {
        (x, y) if x == y => ChangeKind::Unchanged,
        ("passed", "failed") => ChangeKind::NewlyFailing,
        ("failed", "passed") => ChangeKind::Recovered,
        _ => ChangeKind::Changed,
    }
}

/// What the two executions' stored facts say differs.
fn explain(a: &SideFlow, b: &SideFlow) -> Vec<Explanation> {
    let mut out = Vec::new();
    if let (Some(ca), Some(cb)) = (&a.view.condition_fingerprint, &b.view.condition_fingerprint)
        && ca != cb
    {
        let dims = a
            .dims
            .keys()
            .chain(b.dims.keys())
            .collect::<std::collections::BTreeSet<_>>();
        let unknown = "unknown".to_owned();
        let changes = dims
            .into_iter()
            .filter_map(|dim| {
                let (va, vb) = (
                    a.dims.get(dim).unwrap_or(&unknown),
                    b.dims.get(dim).unwrap_or(&unknown),
                );
                (va != vb).then(|| DimChangeView {
                    dim: dim.clone(),
                    a: va.clone(),
                    b: vb.clone(),
                })
            })
            .collect();
        out.push(Explanation::ConditionChanged {
            a: ca.clone(),
            b: cb.clone(),
            changes,
        });
    }
    if let (Some(fa), Some(fb)) = (&a.view.failure_fingerprint, &b.view.failure_fingerprint)
        && fa != fb
    {
        out.push(Explanation::FailureChanged {
            a: fa.clone(),
            b: fb.clone(),
        });
    }
    let evidence = |f: &CompareFlowView| EvidenceView {
        outcome_source: f.outcome_source.clone(),
        finality: f.finality.clone(),
    };
    if evidence(&a.view) != evidence(&b.view) {
        out.push(Explanation::ProvenanceDiffers {
            a: evidence(&a.view),
            b: evidence(&b.view),
        });
    }
    out
}

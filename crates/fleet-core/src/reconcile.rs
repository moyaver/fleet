// SPDX-License-Identifier: AGPL-3.0-only
//! Matching the final report's test cases to the flow executions seen live (research 05 §8.3).
//!
//! Report suites map to shards by position (Maestro writes one `<testsuite>` per shard, in shard
//! order; research 08 Q9). Within a shard, test cases align with live flows in order, so a flow
//! that ran twice matches k-th to k-th. A live flow missing from the report keeps its live
//! outcome; a test case with no live flow is reported as unmatched, never invented.

use fleet_protocol::event::MatchMethod;

/// What a live flow or a test case is known by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlowKey {
    /// Flow name.
    pub name: String,
    /// Flow file path, in whatever form the source wrote it (absolute or relative).
    pub path: Option<String>,
}

/// One test case's match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaseMatch {
    /// `<testsuite>` index.
    pub suite: usize,
    /// `<testcase>` index within the suite.
    pub case: usize,
    /// `(shard index, live flow index)` it matched, if any.
    pub live: Option<(usize, usize)>,
    /// How.
    pub method: MatchMethod,
}

/// Matches report suites (`report[suite][case]`) to live shards (`live[shard][flow]`).
#[must_use]
pub fn reconcile(live: &[Vec<FlowKey>], report: &[Vec<FlowKey>]) -> Vec<CaseMatch> {
    let mut out = Vec::new();
    for (suite, cases) in report.iter().enumerate() {
        let shard = if live.len() == report.len() || live.len() == 1 && report.len() == 1 {
            Some(suite)
        } else {
            None
        };
        let flows = shard
            .and_then(|s| live.get(s))
            .map_or(&[][..], Vec::as_slice);
        for (case, matched) in align(flows, cases).into_iter().enumerate() {
            let (live, method) = match (shard, matched) {
                (Some(s), Some((flow, method))) => (Some((s, flow)), method),
                _ => (None, MatchMethod::Unmatched),
            };
            out.push(CaseMatch {
                suite,
                case,
                live,
                method,
            });
        }
    }
    out
}

/// For each case, the live flow it aligns with. Order-preserving minimum-cost alignment:
/// same file and name costs 0, same name only 1, skipping either side 2.
/// Cost of leaving a flow or a case unmatched.
const SKIP: u32 = 2;

fn align(flows: &[FlowKey], cases: &[FlowKey]) -> Vec<Option<(usize, MatchMethod)>> {
    let exact = |f: &FlowKey, c: &FlowKey| {
        f.name == c.name && same_file(f.path.as_deref(), c.path.as_deref())
    };
    if flows.len() == cases.len() && flows.iter().zip(cases).all(|(f, c)| exact(f, c)) {
        return (0..cases.len())
            .map(|i| Some((i, MatchMethod::Positional)))
            .collect();
    }
    let pair_cost = |f: &FlowKey, c: &FlowKey| {
        if exact(f, c) {
            Some(0)
        } else if f.name == c.name {
            Some(1)
        } else {
            None
        }
    };
    let (n, m) = (flows.len(), cases.len());
    // cost[i][j]: best cost aligning flows[i..] with cases[j..].
    let mut cost = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..=n).rev() {
        for j in (0..=m).rev() {
            let mut best = u32::MAX;
            if i < n {
                best = best.min(SKIP + cell(&cost, i + 1, j));
            }
            if j < m {
                best = best.min(SKIP + cell(&cost, i, j + 1));
            }
            if let (Some(f), Some(c)) = (flows.get(i), cases.get(j))
                && let Some(p) = pair_cost(f, c)
            {
                best = best.min(p + cell(&cost, i + 1, j + 1));
            }
            if let Some(row) = cost.get_mut(i)
                && let Some(slot) = row.get_mut(j)
            {
                *slot = if i == n && j == m { 0 } else { best };
            }
        }
    }
    let mut out = vec![None; m];
    let (mut i, mut j) = (0, 0);
    while i < n || j < m {
        let here = cell(&cost, i, j);
        if let (Some(f), Some(c)) = (flows.get(i), cases.get(j))
            && let Some(p) = pair_cost(f, c)
            && here == p + cell(&cost, i + 1, j + 1)
        {
            if let Some(slot) = out.get_mut(j) {
                *slot = Some((
                    i,
                    if p == 0 {
                        MatchMethod::Aligned
                    } else {
                        MatchMethod::Name
                    },
                ));
            }
            i += 1;
            j += 1;
        } else if j < m && (i == n || here == SKIP + cell(&cost, i, j + 1)) {
            j += 1;
        } else {
            i += 1;
        }
    }
    out
}

fn cell(cost: &[Vec<u32>], i: usize, j: usize) -> u32 {
    cost.get(i)
        .and_then(|r| r.get(j))
        .copied()
        .unwrap_or(u32::MAX / 4)
}

/// Whether two flow paths name the same file: equal, or one is a path-component suffix of the
/// other (Maestro writes the plan with absolute paths and JUnit relative to its working directory).
/// A missing path matches anything.
#[must_use]
pub fn same_file(a: Option<&str>, b: Option<&str>) -> bool {
    let (Some(a), Some(b)) = (a, b) else {
        return true;
    };
    let (a, b) = (a.trim_start_matches("./"), b.trim_start_matches("./"));
    let (long, short) = if a.len() >= b.len() { (a, b) } else { (b, a) };
    long == short || long.ends_with(&format!("/{short}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(name: &str, path: &str) -> FlowKey {
        FlowKey {
            name: name.into(),
            path: Some(path.into()),
        }
    }

    #[test]
    fn paths_match_on_component_suffixes() {
        assert!(same_file(Some("/repo/flows/a.yaml"), Some("flows/a.yaml")));
        assert!(same_file(Some("./flows/a.yaml"), Some("flows/a.yaml")));
        assert!(
            !same_file(Some("/repo/flows/ba.yaml"), Some("a.yaml")),
            "component boundary"
        );
        assert!(same_file(None, Some("x.yaml")));
    }

    #[test]
    fn the_normal_case_is_positional() {
        let live = vec![
            vec![k("a", "/r/a.yaml"), k("b", "/r/b.yaml")],
            vec![k("c", "/r/c.yaml")],
        ];
        let report = vec![
            vec![k("a", "a.yaml"), k("b", "b.yaml")],
            vec![k("c", "c.yaml")],
        ];
        let m = reconcile(&live, &report);
        assert!(
            m.iter().all(|c| c.method == MatchMethod::Positional),
            "{m:?}"
        );
        assert_eq!(m[2].live, Some((1, 0)));
    }

    #[test]
    fn duplicates_match_in_order_and_gaps_are_skipped() {
        // The live parser missed the first `b`; the second `a` must still pair with the second `a`.
        let live = vec![vec![k("a", "a.yaml"), k("a", "a.yaml"), k("c", "c.yaml")]];
        let report = vec![vec![
            k("a", "a.yaml"),
            k("b", "b.yaml"),
            k("a", "a.yaml"),
            k("c", "c.yaml"),
        ]];
        let m = reconcile(&live, &report);
        let pairs: Vec<_> = m.iter().map(|c| c.live.map(|(_, f)| f)).collect();
        assert_eq!(pairs, [Some(0), None, Some(1), Some(2)]);
        assert_eq!(m[1].method, MatchMethod::Unmatched);
    }

    #[test]
    fn a_moved_file_still_matches_by_name() {
        let live = vec![vec![k("login", "old/login.yaml")]];
        let report = vec![vec![k("login", "new/login.yaml")]];
        assert_eq!(reconcile(&live, &report)[0].method, MatchMethod::Name);
    }

    #[test]
    fn suites_that_dont_map_to_shards_stay_unmatched() {
        let live = vec![vec![k("a", "a.yaml")], vec![k("a", "a.yaml")]];
        let report = vec![vec![k("a", "a.yaml")]];
        assert!(reconcile(&live, &report).iter().all(|c| c.live.is_none()));
    }
}

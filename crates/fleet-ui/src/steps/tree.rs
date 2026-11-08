// SPDX-License-Identifier: AGPL-3.0-only
//! Builds the tree from step executions and drives its expand/focus state.

use std::collections::{HashMap, HashSet};

use fleet_protocol::event::StepStatus;
use fleet_protocol::journal::SourceKind;

use crate::mark::Mark;
use crate::provenance::Provenance;

/// One step execution, in the order the steps started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepExec {
    /// Unique id of this execution.
    pub id: String,
    /// Id of the enclosing step (a `runFlow`, `repeat`, ...). It must start earlier in the list.
    pub parent: Option<String>,
    /// Maestro's description of the step.
    pub label: String,
    /// The number the step is shown with, such as `6` or `2.1`; `None` shows no number.
    pub number: Option<String>,
    /// Current status.
    pub status: StepStatus,
    /// Where the status came from (invariant I4).
    pub source: SourceKind,
    /// Duration once the step ended.
    pub duration_ms: Option<u64>,
    /// One-based attempt number; above 1 for retries of the same step.
    pub attempt: u32,
    /// The flow is still running, but its agent has gone quiet (stale or disconnected): a value
    /// still read from the live log is the last one known, so it shows as stale (issue #148).
    pub quiet: bool,
}

/// The provenance a step row shows. A value read from the live log is provisional, and it is stale
/// once its run has gone quiet: the same rule as the flow's own badge (`flow_provenance`). Every
/// other source keeps its own provenance (see [`provenance_of`]).
#[must_use]
pub fn row_provenance(source: SourceKind, quiet: bool) -> Provenance {
    match provenance_of(source) {
        Provenance::Provisional if quiet => Provenance::Stale,
        other => other,
    }
}

/// A step tree.
#[derive(Debug, Clone, Default)]
pub struct StepTree {
    execs: Vec<StepExec>,
    parent: Vec<Option<usize>>,
    children: Vec<Vec<usize>>,
    roots: Vec<usize>,
}

impl StepTree {
    /// Builds a tree. Duplicate ids are dropped; an unknown or later parent makes a root.
    #[must_use]
    pub fn build(input: &[StepExec]) -> Self {
        let mut tree = Self::default();
        let mut index: HashMap<&str, usize> = HashMap::new();
        for exec in input {
            if index.contains_key(exec.id.as_str()) {
                continue;
            }
            let at = tree.execs.len();
            let parent = exec.parent.as_deref().and_then(|p| index.get(p).copied());
            index.insert(exec.id.as_str(), at);
            tree.execs.push(exec.clone());
            tree.parent.push(parent);
            tree.children.push(Vec::new());
            match parent {
                Some(p) => {
                    if let Some(c) = tree.children.get_mut(p) {
                        c.push(at);
                    }
                }
                None => tree.roots.push(at),
            }
        }
        tree
    }

    /// The number of steps in the tree.
    #[must_use]
    pub fn len(&self) -> usize {
        self.execs.len()
    }

    /// Whether the tree has no steps.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.execs.is_empty()
    }

    fn exec(&self, at: usize) -> Option<&StepExec> {
        self.execs.get(at)
    }

    fn kids(&self, at: usize) -> &[usize] {
        self.children.get(at).map_or(&[], Vec::as_slice)
    }

    fn at_id(&self, id: &str) -> Option<usize> {
        self.execs.iter().position(|e| e.id == id)
    }

    fn depth(&self, mut at: usize) -> usize {
        let mut depth = 1;
        while let Some(Some(p)) = self.parent.get(at) {
            depth += 1;
            at = *p;
        }
        depth
    }

    /// The number of attempts of the step that `at` is one attempt of: siblings with the same label.
    fn attempts_total(&self, at: usize) -> u32 {
        let (Some(me), Some(parent)) = (self.exec(at), self.parent.get(at)) else {
            return 1;
        };
        let siblings: &[usize] = match parent {
            Some(p) => self.kids(*p),
            None => &self.roots,
        };
        siblings
            .iter()
            .filter_map(|s| self.exec(*s))
            .filter(|s| s.label == me.label)
            .map(|s| s.attempt)
            .max()
            .unwrap_or(1)
            .max(me.attempt)
    }
}

/// A navigation key from the WAI-ARIA tree pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// Move to the previous visible step.
    Up,
    /// Move to the next visible step.
    Down,
    /// Collapse, or move to the parent.
    Left,
    /// Expand, or move to the first child.
    Right,
    /// Move to the first visible step.
    Home,
    /// Move to the last visible step.
    End,
}

/// Which steps are expanded and which one has focus.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TreeState {
    expanded: HashSet<String>,
    focused: Option<String>,
}

/// One visible row, with the ARIA tree attributes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// Step id.
    pub id: String,
    /// Step description, rendered as a text node.
    pub label: String,
    /// The number the step is shown with, if any.
    pub number: Option<String>,
    /// `aria-level`, one-based.
    pub level: usize,
    /// `aria-posinset` among siblings, one-based.
    pub pos_in_set: usize,
    /// `aria-setsize`: the number of siblings.
    pub set_size: usize,
    /// `aria-expanded`: `None` for leaves.
    pub expanded: Option<bool>,
    /// Whether this row has focus (`tabindex=0`).
    pub focused: bool,
    /// Status.
    pub status: StepStatus,
    /// Provenance of the status.
    pub source: SourceKind,
    /// The provenance the row shows (see [`row_provenance`]): its mark is shown only for an exception.
    pub provenance: Provenance,
    /// Duration text, or `None` while unknown.
    pub duration: Option<String>,
    /// For retries: `attempt 2 of 3`.
    pub attempt: Option<String>,
}

impl TreeState {
    /// Expands every step on the way to a failed or running step, and focuses the first row.
    #[must_use]
    pub fn open_to_attention(tree: &StepTree) -> Self {
        let mut state = Self::default();
        for (at, exec) in tree.execs.iter().enumerate() {
            if matches!(exec.status, StepStatus::Failed | StepStatus::Running) {
                let mut p = tree.parent.get(at).copied().flatten();
                while let Some(i) = p {
                    if let Some(e) = tree.exec(i) {
                        state.expanded.insert(e.id.clone());
                    }
                    p = tree.parent.get(i).copied().flatten();
                }
            }
        }
        state.focused = tree
            .roots
            .first()
            .and_then(|r| tree.exec(*r))
            .map(|e| e.id.clone());
        state
    }

    /// The focused step id.
    #[must_use]
    pub fn focused(&self) -> Option<&str> {
        self.focused.as_deref()
    }

    /// Focuses a step (mouse click); unknown ids are ignored.
    pub fn focus(&mut self, tree: &StepTree, id: &str) {
        if tree.at_id(id).is_some() {
            self.focused = Some(id.to_owned());
        }
    }

    /// Toggles a step that has children.
    pub fn toggle(&mut self, tree: &StepTree, id: &str) {
        let Some(at) = tree.at_id(id) else { return };
        if tree.kids(at).is_empty() {
            return;
        }
        if !self.expanded.remove(id) {
            self.expanded.insert(id.to_owned());
        }
    }

    fn visible(&self, tree: &StepTree) -> Vec<usize> {
        let mut out = Vec::new();
        let mut stack: Vec<usize> = tree.roots.iter().rev().copied().collect();
        while let Some(at) = stack.pop() {
            out.push(at);
            let open = tree.exec(at).is_some_and(|e| self.expanded.contains(&e.id));
            if open {
                stack.extend(tree.kids(at).iter().rev());
            }
        }
        out
    }

    /// The rows to render, in order.
    #[must_use]
    pub fn rows(&self, tree: &StepTree) -> Vec<Row> {
        self.visible(tree)
            .into_iter()
            .filter_map(|at| {
                let e = tree.exec(at)?;
                let siblings: &[usize] = match tree.parent.get(at).copied().flatten() {
                    Some(p) => tree.kids(p),
                    None => &tree.roots,
                };
                let total = tree.attempts_total(at);
                Some(Row {
                    id: e.id.clone(),
                    label: e.label.clone(),
                    number: e.number.clone(),
                    level: tree.depth(at),
                    pos_in_set: siblings.iter().position(|s| *s == at).map_or(1, |p| p + 1),
                    set_size: siblings.len(),
                    expanded: (!tree.kids(at).is_empty()).then(|| self.expanded.contains(&e.id)),
                    focused: self.focused.as_deref() == Some(e.id.as_str()),
                    status: e.status,
                    source: e.source,
                    provenance: row_provenance(e.source, e.quiet),
                    duration: e.duration_ms.map(format_duration),
                    attempt: (total > 1).then(|| format!("attempt {} of {total}", e.attempt)),
                })
            })
            .collect()
    }

    /// Applies a key press, following the WAI-ARIA tree pattern.
    pub fn press(&mut self, tree: &StepTree, key: Key) {
        let visible = self.visible(tree);
        let current = self
            .focused
            .as_deref()
            .and_then(|id| tree.at_id(id))
            .and_then(|at| visible.iter().position(|v| *v == at).map(|pos| (pos, at)));
        let Some((pos, at)) = current else {
            self.focus_visible(tree, visible.first().copied());
            return;
        };
        let open = tree.exec(at).is_some_and(|e| self.expanded.contains(&e.id));
        let has_kids = !tree.kids(at).is_empty();
        let target = match key {
            Key::Up => pos.checked_sub(1).and_then(|p| visible.get(p).copied()),
            Key::Down => visible.get(pos + 1).copied(),
            Key::Home => visible.first().copied(),
            Key::End => visible.last().copied(),
            Key::Right if has_kids && !open => {
                if let Some(e) = tree.exec(at) {
                    self.expanded.insert(e.id.clone());
                }
                None
            }
            Key::Right => tree.kids(at).first().copied().filter(|_| has_kids),
            Key::Left if open => {
                if let Some(e) = tree.exec(at) {
                    self.expanded.remove(&e.id);
                }
                None
            }
            Key::Left => tree.parent.get(at).copied().flatten(),
        };
        self.focus_visible(tree, target);
    }

    fn focus_visible(&mut self, tree: &StepTree, target: Option<usize>) {
        if let Some(e) = target.and_then(|t| tree.exec(t)) {
            self.focused = Some(e.id.clone());
        }
    }
}

/// The status word, in the one Title Case vocabulary. A step that completed reads `Passed`.
#[must_use]
pub fn status_label(status: StepStatus) -> &'static str {
    match status {
        StepStatus::Running => "Running",
        StepStatus::Completed => "Passed",
        StepStatus::Failed => "Failed",
        StepStatus::Warned => "Warned",
        StepStatus::Skipped => "Skipped",
        StepStatus::Unknown => "Unknown",
    }
}

/// The icon and colour that go with the word.
#[must_use]
pub fn status_mark(status: StepStatus) -> Mark {
    match status {
        StepStatus::Running => Mark::Running,
        StepStatus::Completed => Mark::Passed,
        StepStatus::Failed => Mark::Failed,
        StepStatus::Warned => Mark::Interrupted,
        StepStatus::Skipped => Mark::Skipped,
        StepStatus::Unknown => Mark::Unknown,
    }
}

/// The provenance badge for evidence of this kind (invariant I4): live console and debug-log
/// readings are provisional until a final file settles them. A step settled from `commands.json`
/// (issue #148) is verified, as a flow's outcome from that file is.
#[must_use]
pub fn provenance_of(source: SourceKind) -> Provenance {
    match source {
        SourceKind::Declared | SourceKind::AgentObserved | SourceKind::ProcessExit => {
            Provenance::Observed
        }
        SourceKind::ArtifactFile | SourceKind::JunitFinal => Provenance::Final,
        SourceKind::LiveConsole | SourceKind::DebugLog => Provenance::Provisional,
        SourceKind::ServerInferred => Provenance::Inferred,
        SourceKind::Unrecognized => Provenance::Missing,
    }
}

/// `340 ms`, `1.2 s`, `2 m 05 s`.
#[must_use]
pub fn format_duration(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms} ms")
    } else if ms < 60_000 {
        format!("{}.{} s", ms / 1000, (ms % 1000) / 100)
    } else {
        format!("{} m {:02} s", ms / 60_000, (ms % 60_000) / 1000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(id: &str, parent: Option<&str>, status: StepStatus) -> StepExec {
        StepExec {
            id: id.into(),
            parent: parent.map(Into::into),
            label: format!("step {id}"),
            number: None,
            status,
            source: SourceKind::LiveConsole,
            duration_ms: None,
            attempt: 1,
            quiet: false,
        }
    }

    /// root flow `f` { `a`, `sub` { `b`, `c`(failed) }, `d` }
    fn sample() -> StepTree {
        StepTree::build(&[
            step("f", None, StepStatus::Failed),
            step("a", Some("f"), StepStatus::Completed),
            step("sub", Some("f"), StepStatus::Failed),
            step("b", Some("sub"), StepStatus::Completed),
            step("c", Some("sub"), StepStatus::Failed),
            step("d", Some("f"), StepStatus::Skipped),
        ])
    }

    fn ids(rows: &[Row]) -> Vec<&str> {
        rows.iter().map(|r| r.id.as_str()).collect()
    }

    #[test]
    fn collapsed_tree_shows_roots_and_attention_opens_the_path_to_failures() {
        let tree = sample();
        assert_eq!(ids(&TreeState::default().rows(&tree)), ["f"]);
        let rows = TreeState::open_to_attention(&tree).rows(&tree);
        assert_eq!(ids(&rows), ["f", "a", "sub", "b", "c", "d"]);
    }

    #[test]
    fn rows_carry_aria_level_position_and_expanded() {
        let tree = sample();
        let rows = TreeState::open_to_attention(&tree).rows(&tree);
        let sub = &rows[2];
        assert_eq!(
            (sub.level, sub.pos_in_set, sub.set_size, sub.expanded),
            (2, 2, 3, Some(true))
        );
        let c = &rows[4];
        assert_eq!(
            (c.level, c.pos_in_set, c.set_size, c.expanded),
            (3, 2, 2, None)
        );
        assert_eq!(rows[0].level, 1);
    }

    #[test]
    fn retries_are_shown_as_attempts_and_single_runs_are_not() {
        let mut first = step("t1", Some("f"), StepStatus::Failed);
        first.label = "tapOn: Login".into();
        let mut second = step("t2", Some("f"), StepStatus::Completed);
        second.label = "tapOn: Login".into();
        second.attempt = 2;
        let tree = StepTree::build(&[
            step("f", None, StepStatus::Running),
            first,
            second,
            step("x", Some("f"), StepStatus::Completed),
        ]);
        let mut state = TreeState::default();
        state.toggle(&tree, "f");
        let rows = state.rows(&tree);
        assert_eq!(rows[1].attempt.as_deref(), Some("attempt 1 of 2"));
        assert_eq!(rows[2].attempt.as_deref(), Some("attempt 2 of 2"));
        assert_eq!(rows[3].attempt, None);
    }

    #[test]
    fn durations_status_and_provenance_pass_through() {
        let mut s = step("f", None, StepStatus::Warned);
        s.duration_ms = Some(1250);
        s.source = SourceKind::ArtifactFile;
        let rows = TreeState::default().rows(&StepTree::build(&[s]));
        assert_eq!(rows[0].duration.as_deref(), Some("1.2 s"));
        assert_eq!(rows[0].status, StepStatus::Warned);
        assert_eq!(rows[0].source, SourceKind::ArtifactFile);
        assert_eq!(format_duration(340), "340 ms");
        assert_eq!(format_duration(125_000), "2 m 05 s");
        assert_eq!(format_duration(59_999), "59.9 s");
    }

    #[test]
    fn down_up_home_end_move_over_visible_rows_only() {
        let tree = sample();
        let mut s = TreeState::default();
        s.toggle(&tree, "f");
        s.focus(&tree, "f");
        s.press(&tree, Key::Down);
        s.press(&tree, Key::Down);
        assert_eq!(s.focused(), Some("sub"));
        s.press(&tree, Key::Down);
        assert_eq!(
            s.focused(),
            Some("d"),
            "children of a collapsed node are skipped"
        );
        s.press(&tree, Key::Down);
        assert_eq!(s.focused(), Some("d"), "no wrap at the end");
        s.press(&tree, Key::Up);
        assert_eq!(s.focused(), Some("sub"));
        s.press(&tree, Key::Home);
        assert_eq!(s.focused(), Some("f"));
        s.press(&tree, Key::Up);
        assert_eq!(s.focused(), Some("f"));
        s.press(&tree, Key::End);
        assert_eq!(s.focused(), Some("d"));
    }

    #[test]
    fn right_expands_then_enters_and_left_collapses_then_exits() {
        let tree = sample();
        let mut s = TreeState::default();
        s.focus(&tree, "f");
        s.press(&tree, Key::Right);
        assert_eq!(s.rows(&tree)[0].expanded, Some(true));
        assert_eq!(s.focused(), Some("f"), "expanding keeps focus");
        s.press(&tree, Key::Right);
        assert_eq!(s.focused(), Some("a"), "open node: first child");
        s.press(&tree, Key::Right);
        assert_eq!(s.focused(), Some("a"), "leaf: nothing");
        s.press(&tree, Key::Left);
        assert_eq!(s.focused(), Some("f"), "leaf: parent");
        s.press(&tree, Key::Left);
        assert_eq!(ids(&s.rows(&tree)), ["f"], "open node collapses");
        s.press(&tree, Key::Left);
        assert_eq!(s.focused(), Some("f"), "root: nothing");
    }

    #[test]
    fn hostile_input_duplicate_ids_orphans_and_cycles_still_build() {
        let tree = StepTree::build(&[
            step("a", Some("b"), StepStatus::Completed),
            step("b", Some("a"), StepStatus::Completed),
            step("a", None, StepStatus::Failed),
            step("o", Some("ghost"), StepStatus::Completed),
        ]);
        assert_eq!(tree.len(), 3);
        assert_eq!(ids(&TreeState::default().rows(&tree)), ["a", "o"]);
        assert!(StepTree::build(&[]).is_empty());
        let mut s = TreeState::default();
        s.press(&StepTree::build(&[]), Key::Down);
        assert_eq!(s.focused(), None);
    }

    #[test]
    fn every_status_has_distinct_text() {
        let all = [
            StepStatus::Running,
            StepStatus::Completed,
            StepStatus::Failed,
            StepStatus::Warned,
            StepStatus::Skipped,
            StepStatus::Unknown,
        ];
        let labels: HashSet<_> = all.into_iter().map(status_label).collect();
        assert_eq!(labels.len(), all.len());
    }

    #[test]
    fn a_live_value_is_stale_once_its_run_has_gone_quiet_and_settled_ones_are_not() {
        assert_eq!(
            row_provenance(SourceKind::LiveConsole, false),
            Provenance::Provisional
        );
        assert_eq!(
            row_provenance(SourceKind::DebugLog, true),
            Provenance::Stale
        );
        assert_eq!(
            row_provenance(SourceKind::ArtifactFile, true),
            Provenance::Final,
            "a settled step is not a live value"
        );
        assert_eq!(
            row_provenance(SourceKind::ServerInferred, true),
            Provenance::Inferred
        );
    }

    #[test]
    fn evidence_maps_to_the_shared_provenance_and_tone_roles() {
        assert_eq!(
            provenance_of(SourceKind::LiveConsole),
            Provenance::Provisional
        );
        assert_eq!(provenance_of(SourceKind::JunitFinal), Provenance::Final);
        assert_eq!(provenance_of(SourceKind::ArtifactFile), Provenance::Final);
        assert_eq!(
            provenance_of(SourceKind::AgentObserved),
            Provenance::Observed
        );
        assert_eq!(
            provenance_of(SourceKind::ServerInferred),
            Provenance::Inferred
        );
        assert_eq!(provenance_of(SourceKind::Unrecognized), Provenance::Missing);
        assert_eq!(status_mark(StepStatus::Failed), Mark::Failed);
        assert_eq!(status_mark(StepStatus::Completed), Mark::Passed);
        assert_eq!(status_mark(StepStatus::Unknown), Mark::Unknown);
        assert_eq!(status_label(StepStatus::Completed), "Passed");
        assert_eq!(status_label(StepStatus::Warned), "Warned");
    }

    #[test]
    fn toggle_ignores_leaves_and_unknown_ids() {
        let tree = sample();
        let mut s = TreeState::default();
        s.toggle(&tree, "a");
        s.toggle(&tree, "nope");
        assert_eq!(s, TreeState::default());
    }
}

// SPDX-License-Identifier: AGPL-3.0-only
//! Table view-model: sort by one column, filter by text, choose visible columns, and derive the
//! rows to show. Rows are lists of cell text; the view renders them as text nodes.

use std::cmp::Ordering;

/// Sort direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Smallest first.
    Ascending,
    /// Largest first.
    Descending,
}

/// The active sort: one column and a direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sort {
    /// Index of the sorted column.
    pub column: usize,
    /// Direction of the sort.
    pub direction: Direction,
}

/// What the user chose in a table: sort, filter and visible columns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableState {
    visible: Vec<bool>,
    sort: Option<Sort>,
    filter: String,
    filter_column: Option<usize>,
}

impl TableState {
    /// A state for a table with `columns` columns, all visible, unsorted and unfiltered.
    #[must_use]
    pub fn new(columns: usize) -> Self {
        Self {
            visible: vec![true; columns],
            sort: None,
            filter: String::new(),
            filter_column: None,
        }
    }

    /// The active sort, if any.
    #[must_use]
    pub fn sort(&self) -> Option<Sort> {
        self.sort
    }

    /// Cycles a column header: unsorted → ascending → descending → unsorted. Another column
    /// starts at ascending.
    pub fn toggle_sort(&mut self, column: usize) {
        self.sort = match self.sort {
            Some(Sort {
                column: c,
                direction: Direction::Ascending,
            }) if c == column => Some(Sort {
                column,
                direction: Direction::Descending,
            }),
            Some(Sort {
                column: c,
                direction: Direction::Descending,
            }) if c == column => None,
            _ => Some(Sort {
                column,
                direction: Direction::Ascending,
            }),
        };
    }

    /// Keeps rows with a cell containing `text`, ignoring case and surrounding whitespace.
    pub fn set_filter(&mut self, text: &str) {
        self.filter = text.trim().to_lowercase();
    }

    /// Restricts the filter to one column; `None` matches any visible column.
    pub fn set_filter_column(&mut self, column: Option<usize>) {
        self.filter_column = column;
    }

    /// Shows or hides a column. The last visible column cannot be hidden.
    pub fn toggle_column(&mut self, column: usize) {
        let Some(shown) = self.visible.get(column).copied() else {
            return;
        };
        if shown && self.visible_columns().len() == 1 {
            return;
        }
        if let Some(slot) = self.visible.get_mut(column) {
            *slot = !shown;
        }
    }

    /// Whether `column` is shown.
    #[must_use]
    pub fn is_visible(&self, column: usize) -> bool {
        self.visible.get(column).copied().unwrap_or(false)
    }

    /// The indices of the shown columns, in order.
    #[must_use]
    pub fn visible_columns(&self) -> Vec<usize> {
        (0..self.visible.len())
            .filter(|&c| self.is_visible(c))
            .collect()
    }

    /// The indices of `rows` to show, in display order: filtered, then sorted. Numbers sort by
    /// value and before text; text sorts ignoring case; equal keys keep their original order in
    /// both directions.
    #[must_use]
    pub fn derive(&self, rows: &[Vec<String>]) -> Vec<usize> {
        let columns = self.visible_columns();
        let mut shown: Vec<usize> = (0..rows.len())
            .filter(|&i| {
                self.filter.is_empty()
                    || rows.get(i).is_some_and(|row| match self.filter_column {
                        Some(c) => cell(row, c).to_lowercase().contains(&self.filter),
                        None => columns
                            .iter()
                            .any(|&c| cell(row, c).to_lowercase().contains(&self.filter)),
                    })
            })
            .collect();
        if let Some(Sort { column, direction }) = self.sort {
            // `sort_by` is stable, so ties keep source order; descending reverses the
            // comparison, not the list.
            shown.sort_by(|&a, &b| {
                let order = Key::of(rows.get(a), column).cmp(&Key::of(rows.get(b), column));
                match direction {
                    Direction::Ascending => order,
                    Direction::Descending => order.reverse(),
                }
            });
        }
        shown
    }
}

fn cell(row: &[String], column: usize) -> &str {
    row.get(column).map_or("", String::as_str)
}

/// A sort key: numbers by value, then text ignoring case. A total order, so sorting never panics.
#[derive(Debug, PartialEq)]
enum Key {
    Number(f64),
    Text(String),
}

impl Key {
    fn of(row: Option<&Vec<String>>, column: usize) -> Self {
        let text = row.map_or("", |r| cell(r, column)).trim();
        match text.parse::<f64>() {
            Ok(n) if n.is_finite() => Self::Number(n),
            _ => Self::Text(text.to_lowercase()),
        }
    }
}

impl Eq for Key {}

impl PartialOrd for Key {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Key {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Self::Number(a), Self::Number(b)) => a.total_cmp(b),
            (Self::Number(_), Self::Text(_)) => Ordering::Less,
            (Self::Text(_), Self::Number(_)) => Ordering::Greater,
            (Self::Text(a), Self::Text(b)) => a.cmp(b),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(cells: &[&[&str]]) -> Vec<Vec<String>> {
        cells
            .iter()
            .map(|r| r.iter().map(|c| (*c).to_owned()).collect())
            .collect()
    }

    #[test]
    fn an_untouched_table_shows_rows_in_source_order() {
        let r = rows(&[&["b"], &["a"]]);
        assert_eq!(TableState::new(1).derive(&r), [0, 1]);
    }

    #[test]
    fn header_clicks_cycle_ascending_descending_unsorted() {
        let mut t = TableState::new(2);
        let col = |direction| {
            Some(Sort {
                column: 1,
                direction,
            })
        };
        t.toggle_sort(1);
        assert_eq!(t.sort(), col(Direction::Ascending));
        t.toggle_sort(1);
        assert_eq!(t.sort(), col(Direction::Descending));
        t.toggle_sort(1);
        assert_eq!(t.sort(), None);
        t.toggle_sort(1);
        t.toggle_sort(0);
        assert_eq!(
            t.sort(),
            Some(Sort {
                column: 0,
                direction: Direction::Ascending
            })
        );
    }

    #[test]
    fn numbers_sort_by_value_before_text_and_text_ignores_case() {
        let r = rows(&[&["10"], &["9"], &["banana"], &["100"], &["Apple"]]);
        let mut t = TableState::new(1);
        t.toggle_sort(0);
        assert_eq!(t.derive(&r), [1, 0, 3, 4, 2]);
    }

    #[test]
    fn ties_keep_source_order_in_both_directions() {
        let r = rows(&[&["x", "1"], &["y", "1"], &["z", "2"]]);
        let mut t = TableState::new(2);
        t.toggle_sort(1);
        assert_eq!(t.derive(&r), [0, 1, 2]);
        t.toggle_sort(1);
        assert_eq!(t.derive(&r), [2, 0, 1]);
    }

    #[test]
    fn the_filter_matches_any_visible_cell_ignoring_case() {
        let r = rows(&[
            &["Pixel 8", "online"],
            &["iPhone", "stale"],
            &["pixel 7", "stale"],
        ]);
        let mut t = TableState::new(2);
        t.set_filter("  PIXEL ");
        assert_eq!(t.derive(&r), [0, 2]);
        t.set_filter("");
        assert_eq!(t.derive(&r), [0, 1, 2]);
    }

    #[test]
    fn a_column_filter_ignores_other_columns_and_hidden_columns_do_not_match() {
        let r = rows(&[&["pixel", "stale"], &["stale", "pixel"]]);
        let mut t = TableState::new(2);
        t.set_filter("stale");
        t.set_filter_column(Some(1));
        assert_eq!(t.derive(&r), [0]);
        t.set_filter_column(None);
        t.toggle_column(1);
        assert_eq!(t.derive(&r), [1]);
    }

    #[test]
    fn filter_applies_before_sort() {
        let r = rows(&[&["b", "x"], &["a", "y"], &["c", "x"]]);
        let mut t = TableState::new(2);
        t.set_filter("x");
        t.toggle_sort(0);
        t.toggle_sort(0);
        assert_eq!(t.derive(&r), [2, 0]);
    }

    #[test]
    fn columns_can_be_hidden_but_never_all_of_them() {
        let mut t = TableState::new(2);
        t.toggle_column(0);
        assert_eq!(t.visible_columns(), [1]);
        assert!(!t.is_visible(0));
        t.toggle_column(1);
        assert_eq!(t.visible_columns(), [1]);
        t.toggle_column(0);
        assert_eq!(t.visible_columns(), [0, 1]);
    }

    #[test]
    fn short_rows_count_missing_cells_as_empty() {
        let r = rows(&[&["a", "z"], &["b"]]);
        let mut t = TableState::new(2);
        t.toggle_sort(1);
        assert_eq!(t.derive(&r), [1, 0]);
    }
}

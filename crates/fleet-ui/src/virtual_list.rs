// SPDX-License-Identifier: AGPL-3.0-only
//! Fixed-row-height windowing for long lists (PLAN §5.4): which rows to render for a scroll
//! position, and how much padding stands in for the rest.

/// The rows to render and the padding above and below them, in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    /// Index of the first rendered row.
    pub start: usize,
    /// One past the index of the last rendered row.
    pub end: usize,
    /// Height of the spacer standing in for rows before `start`.
    pub pad_top: u64,
    /// Height of the spacer standing in for rows from `end` on.
    pub pad_bottom: u64,
}

/// The window for `count` rows of `row_height` pixels in a viewport `viewport_height` pixels
/// tall scrolled to `scroll_top`, with `overscan` extra rows on each side. A scroll position past
/// the end is clamped to the end; zero rows or a zero row height give an empty window.
#[must_use]
pub fn window(
    scroll_top: u64,
    viewport_height: u64,
    row_height: u64,
    count: usize,
    overscan: usize,
) -> Window {
    if count == 0 || row_height == 0 {
        return Window {
            start: 0,
            end: 0,
            pad_top: 0,
            pad_bottom: 0,
        };
    }
    let rows = to_u64(count);
    let overscan = to_u64(overscan);
    let max_scroll = (rows * row_height).saturating_sub(viewport_height);
    let top = scroll_top.min(max_scroll);
    let first = top / row_height;
    let last = (top + viewport_height).div_ceil(row_height).min(rows);
    let start = first.saturating_sub(overscan);
    let end = (last + overscan).min(rows);
    Window {
        start: to_usize(start),
        end: to_usize(end),
        pad_top: start * row_height,
        pad_bottom: (rows - end) * row_height,
    }
}

fn to_u64(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

fn to_usize(n: u64) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_of_a_long_list_renders_visible_rows_plus_overscan_below() {
        // 100px viewport / 20px rows = rows 0..5 visible, plus 2 overscan.
        assert_eq!(
            window(0, 100, 20, 1000, 2),
            Window {
                start: 0,
                end: 7,
                pad_top: 0,
                pad_bottom: 19_860
            }
        );
    }

    #[test]
    fn mid_list_partial_rows_count_and_overscan_applies_both_ways() {
        // Rows 10..16 touch [205, 305): row 10 starts at 200, row 15 ends at 320.
        assert_eq!(
            window(205, 100, 20, 1000, 2),
            Window {
                start: 8,
                end: 18,
                pad_top: 160,
                pad_bottom: 19_640
            }
        );
    }

    #[test]
    fn scrolling_past_the_end_clamps_to_the_last_rows() {
        // Total height 20_000; max scroll 19_900 shows rows 995..1000.
        assert_eq!(
            window(1_000_000, 100, 20, 1000, 2),
            Window {
                start: 993,
                end: 1000,
                pad_top: 19_860,
                pad_bottom: 0
            }
        );
    }

    #[test]
    fn a_short_list_renders_entirely() {
        assert_eq!(
            window(0, 100, 20, 3, 5),
            Window {
                start: 0,
                end: 3,
                pad_top: 0,
                pad_bottom: 0
            }
        );
    }

    #[test]
    fn empty_input_gives_an_empty_window() {
        let empty = Window {
            start: 0,
            end: 0,
            pad_top: 0,
            pad_bottom: 0,
        };
        assert_eq!(window(50, 100, 20, 0, 2), empty);
        assert_eq!(window(50, 100, 0, 10, 2), empty);
    }
}

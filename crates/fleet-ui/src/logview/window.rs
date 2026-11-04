// SPDX-License-Identifier: AGPL-3.0-only
//! Scroll math for jumping to a line. Windowing itself is `crate::virtual_list::window`.

/// The scroll offset that puts `index` at the top of the viewport, clamped to the content.
#[must_use]
pub fn scroll_to(index: usize, total: usize, row_px: u32, viewport_px: u32) -> u64 {
    let row = u64::from(row_px.max(1));
    let max = (row * total as u64).saturating_sub(u64::from(viewport_px));
    (row * index as u64).min(max)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scroll_to_clamps_so_the_last_page_stays_full() {
        assert_eq!(scroll_to(3, 100, 20, 400), 60);
        assert_eq!(scroll_to(99, 100, 20, 400), 1600);
    }
}

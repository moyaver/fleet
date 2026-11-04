// SPDX-License-Identifier: AGPL-3.0-only
//! Pagination math: page count, the item range of a page, and the page links to show.

use std::ops::Range;

/// One entry in the row of page links.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageItem {
    /// A link to this 1-based page.
    Page(usize),
    /// A run of skipped pages ("…").
    Gap,
}

/// A page of a list: always valid, even for an empty list or an out-of-range request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pages {
    total_items: usize,
    page_size: usize,
    page: usize,
}

impl Pages {
    /// Page `page` (1-based) of `total_items` items in pages of `page_size`. The page is clamped
    /// to the valid range and a zero page size counts as one.
    #[must_use]
    pub fn new(total_items: usize, page_size: usize, page: usize) -> Self {
        let size = page_size.max(1);
        let count = total_items.div_ceil(size).max(1);
        Self {
            total_items,
            page_size: size,
            page: page.clamp(1, count),
        }
    }

    /// The current 1-based page.
    #[must_use]
    pub fn page(&self) -> usize {
        self.page
    }

    /// The number of pages; an empty list still has one (empty) page.
    #[must_use]
    pub fn page_count(&self) -> usize {
        self.total_items.div_ceil(self.page_size).max(1)
    }

    /// The indices of the items on the current page.
    #[must_use]
    pub fn range(&self) -> Range<usize> {
        let start = (self.page - 1) * self.page_size;
        start.min(self.total_items)..(start + self.page_size).min(self.total_items)
    }

    /// Whether there is a page before the current one.
    #[must_use]
    pub fn has_prev(&self) -> bool {
        self.page > 1
    }

    /// Whether there is a page after the current one.
    #[must_use]
    pub fn has_next(&self) -> bool {
        self.page < self.page_count()
    }

    /// The page links to show: the first and last page, `siblings` pages around the current one,
    /// and a [`PageItem::Gap`] where two or more pages are skipped.
    #[must_use]
    pub fn items(&self, siblings: usize) -> Vec<PageItem> {
        let last = self.page_count();
        let low = self.page.saturating_sub(siblings).max(1);
        let high = (self.page + siblings).min(last);
        let mut shown: Vec<usize> = std::iter::once(1)
            .chain(low..=high)
            .chain(std::iter::once(last))
            .collect();
        shown.dedup();
        let mut items = Vec::with_capacity(shown.len() + 2);
        let mut previous = 0;
        for page in shown {
            match page - previous {
                0 | 1 => {}
                2 => items.push(PageItem::Page(previous + 1)),
                _ => items.push(PageItem::Gap),
            }
            items.push(PageItem::Page(page));
            previous = page;
        }
        items
    }
}

#[cfg(test)]
mod tests {
    use super::PageItem::{Gap, Page};
    use super::*;

    #[test]
    fn the_last_page_holds_the_remainder() {
        let p = Pages::new(95, 10, 10);
        assert_eq!(p.page_count(), 10);
        assert_eq!(p.range(), 90..95);
        assert!(p.has_prev());
        assert!(!p.has_next());
    }

    #[test]
    fn out_of_range_pages_are_clamped() {
        assert_eq!(Pages::new(95, 10, 0).page(), 1);
        assert_eq!(Pages::new(95, 10, 99).page(), 10);
        assert_eq!(Pages::new(95, 10, 1).range(), 0..10);
    }

    #[test]
    fn an_empty_list_has_one_empty_page() {
        let p = Pages::new(0, 10, 3);
        assert_eq!((p.page(), p.page_count(), p.range()), (1, 1, 0..0));
        assert!(!p.has_prev() && !p.has_next());
    }

    #[test]
    fn a_zero_page_size_counts_as_one() {
        assert_eq!(Pages::new(3, 0, 1).page_count(), 3);
    }

    #[test]
    fn links_elide_runs_of_two_or_more_skipped_pages() {
        let at = |page| Pages::new(100, 10, page).items(1);
        assert_eq!(
            at(5),
            [Page(1), Gap, Page(4), Page(5), Page(6), Gap, Page(10)]
        );
        assert_eq!(at(1), [Page(1), Page(2), Gap, Page(10)]);
        assert_eq!(at(3), [Page(1), Page(2), Page(3), Page(4), Gap, Page(10)]);
        // One skipped page is shown rather than replaced by a gap.
        assert_eq!(
            at(4),
            [Page(1), Page(2), Page(3), Page(4), Page(5), Gap, Page(10)]
        );
    }

    #[test]
    fn few_pages_show_every_link() {
        assert_eq!(Pages::new(25, 10, 2).items(1), [Page(1), Page(2), Page(3)]);
    }
}

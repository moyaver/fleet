// SPDX-License-Identifier: AGPL-3.0-only
//! Screenshot viewer view model: ordered shots per step, zoom levels, step strip
//! (PLAN §10.2 order 7). Images are plain `<img>` elements with URLs from this model.

/// A screenshot taken during a step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shot {
    /// The step that took it.
    pub step: String,
    /// Caption: the step's description.
    pub label: String,
    /// Artifact URL: same-origin path (`/...`) or `http(s)://`; anything else is dropped.
    pub url: String,
}

/// Zoom steps, in percent; `None` fits the image to the viewport.
const ZOOMS: [Option<u16>; 5] = [None, Some(50), Some(100), Some(200), Some(400)];

/// A zoom level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Zoom {
    /// Scale to the viewport.
    Fit,
    /// A fixed percentage of the natural size.
    Percent(u16),
}

/// One cell of the step strip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripItem {
    /// Index into the shots.
    pub index: usize,
    /// The step that took the shot.
    pub step: String,
    /// Thumbnail URL.
    pub url: String,
    /// Accessible name.
    pub label: String,
    /// Whether it is the selected shot (`aria-current`).
    pub selected: bool,
}

/// The viewer state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScreenshotViewer {
    shots: Vec<Shot>,
    selected: usize,
    zoom: usize,
}

impl ScreenshotViewer {
    /// Builds a viewer over shots in capture order, dropping those with an unsafe URL.
    #[must_use]
    pub fn new(shots: impl IntoIterator<Item = Shot>) -> Self {
        Self {
            shots: shots.into_iter().filter(|s| safe_url(&s.url)).collect(),
            selected: 0,
            zoom: 0,
        }
    }

    /// Swaps in the shots of a later fetch (renewed links, or one more capture), keeping the
    /// selection and the zoom. The selection stays at its position, or moves to the last shot if
    /// the list got shorter; a renewal keeps the order, so the same shot stays selected.
    pub fn replace(&mut self, shots: impl IntoIterator<Item = Shot>) {
        self.shots = shots.into_iter().filter(|s| safe_url(&s.url)).collect();
        self.selected = self.selected.min(self.shots.len().saturating_sub(1));
    }

    /// The number of shots.
    #[must_use]
    pub fn len(&self) -> usize {
        self.shots.len()
    }

    /// Whether there are no shots (show an empty state).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.shots.is_empty()
    }

    /// The selected shot.
    #[must_use]
    pub fn selected(&self) -> Option<&Shot> {
        self.shots.get(self.selected)
    }

    /// `3 of 12`, or `None` without shots.
    #[must_use]
    pub fn position(&self) -> Option<String> {
        (!self.is_empty()).then(|| format!("{} of {}", self.selected + 1, self.shots.len()))
    }

    /// Selects the next shot; stays on the last.
    pub fn next(&mut self) {
        self.selected = (self.selected + 1).min(self.shots.len().saturating_sub(1));
    }

    /// Selects the previous shot; stays on the first.
    pub fn prev(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Selects shot `index`; out-of-range is ignored.
    pub fn select(&mut self, index: usize) {
        if index < self.shots.len() {
            self.selected = index;
        }
    }

    /// Selects the first shot of `step`. Returns whether the step has one.
    pub fn select_step(&mut self, step: &str) -> bool {
        match self.shots.iter().position(|s| s.step == step) {
            Some(i) => {
                self.selected = i;
                true
            }
            None => false,
        }
    }

    /// The current zoom.
    #[must_use]
    pub fn zoom(&self) -> Zoom {
        match ZOOMS.get(self.zoom).copied().flatten() {
            Some(p) => Zoom::Percent(p),
            None => Zoom::Fit,
        }
    }

    /// Zooms in one level, up to 400 %.
    pub fn zoom_in(&mut self) {
        self.zoom = (self.zoom + 1).min(ZOOMS.len() - 1);
    }

    /// Zooms out one level, down to fit.
    pub fn zoom_out(&mut self) {
        self.zoom = self.zoom.saturating_sub(1);
    }

    /// Back to fit.
    pub fn reset_zoom(&mut self) {
        self.zoom = 0;
    }

    /// The step strip.
    #[must_use]
    pub fn strip(&self) -> Vec<StripItem> {
        self.shots
            .iter()
            .enumerate()
            .map(|(index, s)| StripItem {
                index,
                step: s.step.clone(),
                url: s.url.clone(),
                label: format!(
                    "Screenshot {} of {}: {}",
                    index + 1,
                    self.shots.len(),
                    s.label
                ),
                selected: index == self.selected,
            })
            .collect()
    }
}

fn safe_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    (url.starts_with('/') && !url.starts_with("//") && !url.starts_with("/\\"))
        || lower.starts_with("https://")
        || lower.starts_with("http://")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shot(step: &str, url: &str) -> Shot {
        Shot {
            step: step.into(),
            label: format!("tap {step}"),
            url: url.into(),
        }
    }

    fn viewer() -> ScreenshotViewer {
        ScreenshotViewer::new([
            shot("a", "/artifacts/1.png"),
            shot("b", "/artifacts/2.png"),
            shot("b", "/artifacts/3.png"),
        ])
    }

    #[test]
    fn navigation_follows_capture_order_and_clamps() {
        let mut v = viewer();
        assert_eq!(v.position().as_deref(), Some("1 of 3"));
        v.prev();
        assert_eq!(v.selected().unwrap().url, "/artifacts/1.png");
        v.next();
        v.next();
        v.next();
        assert_eq!(v.position().as_deref(), Some("3 of 3"));
        v.select(99);
        assert_eq!(v.position().as_deref(), Some("3 of 3"));
        v.select(1);
        assert_eq!(v.selected().unwrap().url, "/artifacts/2.png");
    }

    #[test]
    fn choosing_a_step_selects_its_first_shot() {
        let mut v = viewer();
        assert!(v.select_step("b"));
        assert_eq!(v.selected().unwrap().url, "/artifacts/2.png");
        assert!(!v.select_step("zzz"));
        assert_eq!(v.selected().unwrap().step, "b");
    }

    #[test]
    fn zoom_goes_fit_50_100_200_400_and_clamps_both_ways() {
        let mut v = viewer();
        assert_eq!(v.zoom(), Zoom::Fit);
        v.zoom_out();
        assert_eq!(v.zoom(), Zoom::Fit);
        let seen: Vec<Zoom> = (0..5)
            .map(|_| {
                v.zoom_in();
                v.zoom()
            })
            .collect();
        assert_eq!(
            seen,
            [
                Zoom::Percent(50),
                Zoom::Percent(100),
                Zoom::Percent(200),
                Zoom::Percent(400),
                Zoom::Percent(400)
            ]
        );
        v.zoom_out();
        assert_eq!(v.zoom(), Zoom::Percent(200));
        v.reset_zoom();
        assert_eq!(v.zoom(), Zoom::Fit);
    }

    #[test]
    fn the_strip_marks_the_selected_shot_with_an_accessible_name() {
        let mut v = viewer();
        v.select(2);
        let strip = v.strip();
        assert_eq!(
            strip.iter().map(|i| i.selected).collect::<Vec<_>>(),
            [false, false, true]
        );
        assert_eq!(strip[0].label, "Screenshot 1 of 3: tap a");
    }

    #[test]
    fn urls_that_could_run_script_or_leave_the_origin_oddly_are_dropped() {
        let v = ScreenshotViewer::new([
            shot("a", "javascript:alert(1)"),
            shot("b", "data:image/svg+xml,<svg onload=alert(1)>"),
            shot("c", "//evil.example/x.png"),
            shot("d", "/\\evil.example/x.png"),
            shot("e", "HTTPS://cdn.example/x.png"),
            shot("f", "/a.png"),
        ]);
        assert_eq!(
            v.strip()
                .iter()
                .map(|i| i.step.as_str())
                .collect::<Vec<_>>(),
            ["e", "f"]
        );
    }

    #[test]
    fn renewed_links_keep_the_selected_shot_and_the_zoom() {
        let mut v = viewer();
        v.select(1);
        v.zoom_in();
        v.replace([
            shot("a", "/artifacts/1.png?renewed"),
            shot("b", "/artifacts/2.png?renewed"),
            shot("b", "/artifacts/3.png?renewed"),
        ]);
        assert_eq!(v.position().as_deref(), Some("2 of 3"));
        assert_eq!(v.selected().unwrap().url, "/artifacts/2.png?renewed");
        assert_eq!(v.zoom(), Zoom::Percent(50));
    }

    #[test]
    fn a_shorter_list_moves_the_selection_to_its_last_shot() {
        let mut v = viewer();
        v.select(2);
        v.replace([shot("a", "/artifacts/1.png")]);
        assert_eq!(v.position().as_deref(), Some("1 of 1"));
    }

    #[test]
    fn an_empty_viewer_is_inert() {
        let mut v = ScreenshotViewer::new([]);
        v.next();
        v.prev();
        v.zoom_in();
        assert!(v.is_empty() && v.selected().is_none() && v.position().is_none());
    }
}
pub mod view;

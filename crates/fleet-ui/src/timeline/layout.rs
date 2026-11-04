// SPDX-License-Identifier: AGPL-3.0-only
//! Turns timeline data and a time window into rectangles, and answers pointer hit-tests.

use crate::charts::scale::{TimeScale, time_ticks};
use crate::charts::time_series::{XTick, px_count};
use crate::timeline::model::{BarState, TimelineData};
use crate::timeline::style::{BarStyle, bar_style};

/// How the timeline is laid out, in pixels.
#[derive(Debug, Clone, PartialEq)]
pub struct LayoutConfig {
    /// Total width, lane labels included.
    pub width: f64,
    /// Width of the lane-label column on the left.
    pub gutter: f64,
    /// Height of the time axis on top.
    pub axis_height: f64,
    /// Height of one lane.
    pub lane_height: f64,
    /// Space between a lane's edge and its bars, top and bottom.
    pub bar_inset: f64,
    /// Narrowest a bar is drawn, so an instant flow stays visible.
    pub min_bar_width: f64,
    /// The current time, unix ms: where running bars end.
    pub now_ms: i64,
    /// The viewer's offset from UTC, for axis labels.
    pub utc_offset_minutes: i32,
}

impl Default for LayoutConfig {
    fn default() -> Self {
        Self {
            width: 960.0,
            gutter: 140.0,
            axis_height: 24.0,
            lane_height: 36.0,
            bar_inset: 6.0,
            min_bar_width: 2.0,
            now_ms: 0,
            utc_offset_minutes: 0,
        }
    }
}

/// A rectangle in pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    /// Left edge.
    pub x: f64,
    /// Top edge.
    pub y: f64,
    /// Width.
    pub w: f64,
    /// Height.
    pub h: f64,
}

impl Rect {
    /// Whether the point is inside: left and top edges count, right and bottom don't.
    #[must_use]
    pub fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }
}

/// A lane's label and band.
#[derive(Debug, Clone, PartialEq)]
pub struct LaneBox {
    /// The lane id.
    pub id: String,
    /// The label in the gutter.
    pub label: String,
    /// Top of the band.
    pub y: f64,
    /// Height of the band.
    pub height: f64,
}

/// A bar to draw.
#[derive(Debug, Clone, PartialEq)]
pub struct BarBox {
    /// The flow execution id.
    pub id: String,
    /// Index into [`TimelineLayout::lanes`].
    pub lane: usize,
    /// The flow's name.
    pub label: String,
    /// Where it is, clipped to the plot.
    pub rect: Rect,
    /// Tone, pattern and outcome word.
    pub style: BarStyle,
    /// Open-ended: still running.
    pub running: bool,
    /// Started before the visible window.
    pub clipped_start: bool,
    /// Ends after the visible window.
    pub clipped_end: bool,
}

/// Everything a view needs to draw the timeline.
#[derive(Debug, Clone, PartialEq)]
pub struct TimelineLayout {
    /// The lane bands, top to bottom.
    pub lanes: Vec<LaneBox>,
    /// The bars that intersect the window, in drawing order (later is on top).
    pub bars: Vec<BarBox>,
    /// The time axis.
    pub x_ticks: Vec<XTick>,
    /// Total height.
    pub height: f64,
    x_scale: TimeScale,
    plot: (f64, f64),
}

impl TimelineLayout {
    /// Lays out the bars that intersect `window` (unix ms).
    #[must_use]
    pub fn build(data: &TimelineData, window: (i64, i64), config: &LayoutConfig) -> Self {
        let left = config.gutter;
        let right = config.width.max(left);
        let x_scale = TimeScale::new(window, (left, right));
        let lane_count = data.lanes().len();
        let lanes = data
            .lanes()
            .iter()
            .enumerate()
            .map(|(i, lane)| LaneBox {
                id: lane.id.clone(),
                label: lane.label.clone(),
                y: config.axis_height + lane_height_times(config.lane_height, i),
                height: config.lane_height,
            })
            .collect::<Vec<_>>();
        let bars = data
            .bars()
            .iter()
            .filter_map(|bar| {
                let (start, end) = (bar.start_ms, bar.end_ms(config.now_ms));
                if end < window.0 || start > window.1 {
                    return None;
                }
                let lane_index = data.lane_index(bar)?;
                let lane = lanes.get(lane_index)?;
                let x0 = x_scale.map(start).max(left);
                let x1 = x_scale.map(end).min(right);
                let w = (x1 - x0)
                    .max(config.min_bar_width)
                    .min((right - x0).max(0.0));
                Some(BarBox {
                    id: bar.id.clone(),
                    lane: lane_index,
                    label: bar.label.clone(),
                    rect: Rect {
                        x: x0,
                        y: lane.y + config.bar_inset,
                        w,
                        h: (config.lane_height - 2.0 * config.bar_inset).max(0.0),
                    },
                    style: bar_style(bar.state, bar.provenance),
                    running: matches!(bar.state, BarState::Running),
                    clipped_start: start < window.0,
                    clipped_end: end > window.1,
                })
            })
            .collect();
        let target = (px_count(right - left) / TICK_SPACING_PX).max(2);
        let x_ticks = time_ticks(window.0, window.1, target, config.utc_offset_minutes)
            .into_iter()
            .map(|t| XTick {
                at_ms: t.at_ms,
                x: x_scale.map(t.at_ms),
                label: t.label,
            })
            .collect();
        Self {
            lanes,
            bars,
            x_ticks,
            height: config.axis_height + lane_height_times(config.lane_height, lane_count),
            x_scale,
            plot: (left, right),
        }
    }

    /// The topmost bar at a pixel, if any.
    #[must_use]
    pub fn hit_test(&self, x: f64, y: f64) -> Option<&BarBox> {
        self.bars.iter().rev().find(|b| b.rect.contains(x, y))
    }

    /// The time under a pixel x; `None` in the gutter or past the plot.
    #[must_use]
    pub fn time_at_x(&self, x: f64) -> Option<i64> {
        (self.plot.0..=self.plot.1)
            .contains(&x)
            .then(|| self.x_scale.invert(x))
    }

    /// Pixel x of a time; `None` outside the window.
    #[must_use]
    pub fn x_for_time(&self, at_ms: i64) -> Option<f64> {
        let (from, to) = self.x_scale.domain();
        (from..=to)
            .contains(&at_ms)
            .then(|| self.x_scale.map(at_ms))
    }
}

/// About one axis label per this many pixels.
const TICK_SPACING_PX: usize = 100;

#[expect(
    clippy::cast_precision_loss,
    reason = "a lane count is tiny compared with 2^53"
)]
fn lane_height_times(lane_height: f64, count: usize) -> f64 {
    lane_height * count as f64
}

#[cfg(test)]
mod tests {
    use fleet_protocol::event::FlowOutcome;

    use super::*;
    use crate::charts::format::Unit;
    use crate::charts::series::{Metric, Point, Reading, Series};
    use crate::charts::time_series::{ChartConfig, Margin, TimeSeriesModel};
    use crate::timeline::model::fixtures::*;
    use crate::timeline::style::Pattern;
    use crate::tone::Tone;

    /// Plot x 100..700 (600 px); lanes at y 20..50 and 50..80; bars inset 4 px.
    fn config() -> LayoutConfig {
        LayoutConfig {
            width: 700.0,
            gutter: 100.0,
            axis_height: 20.0,
            lane_height: 30.0,
            bar_inset: 4.0,
            min_bar_width: 2.0,
            now_ms: 5000,
            utc_offset_minutes: 0,
        }
    }

    fn data() -> TimelineData {
        TimelineData::new(
            vec![lane("a"), lane("b")],
            vec![
                ended("a1", "a", 1000, 3000, FlowOutcome::Passed),
                running("b1", "b", 4000),
            ],
        )
    }

    fn rect(b: &BarBox) -> (f64, f64, f64, f64) {
        (b.rect.x, b.rect.y, b.rect.w, b.rect.h)
    }

    #[test]
    fn lanes_stack_below_the_axis_and_bars_sit_inside_their_lane() {
        // 600 px for 6000 ms: 0.1 px per ms.
        let l = TimelineLayout::build(&data(), (0, 6000), &config());
        let lanes: Vec<(&str, f64, f64)> = l
            .lanes
            .iter()
            .map(|b| (b.label.as_str(), b.y, b.height))
            .collect();
        assert_eq!(lanes, [("Device a", 20.0, 30.0), ("Device b", 50.0, 30.0)]);
        assert!((l.height - 80.0).abs() < 1e-9);
        let a1 = &l.bars[0];
        assert_eq!(rect(a1), (200.0, 24.0, 200.0, 22.0));
        assert_eq!(
            (a1.style.tone, a1.style.pattern),
            (Tone::Success, Pattern::Solid)
        );
        assert!(!a1.running);
        // A running bar reaches "now" (5000 ms) and is marked open-ended.
        let b1 = &l.bars[1];
        assert_eq!(rect(b1), (500.0, 54.0, 100.0, 22.0));
        assert!(b1.running);
        assert_eq!(b1.lane, 1);
    }

    #[test]
    fn bars_outside_the_window_are_dropped_and_straddling_bars_are_clipped_to_the_plot() {
        let d = TimelineData::new(
            vec![lane("a")],
            vec![
                ended("before", "a", 0, 1000, FlowOutcome::Passed),
                ended("left", "a", 1000, 3000, FlowOutcome::Passed),
                ended("right", "a", 5000, 9000, FlowOutcome::Failed),
                ended("after", "a", 7000, 8000, FlowOutcome::Passed),
            ],
        );
        // Window 2000..6000: 0.15 px per ms.
        let l = TimelineLayout::build(&d, (2000, 6000), &config());
        let ids: Vec<&str> = l.bars.iter().map(|b| b.id.as_str()).collect();
        assert_eq!(ids, ["left", "right"]);
        let left = &l.bars[0];
        assert_eq!((left.rect.x, left.rect.w), (100.0, 150.0));
        assert!(left.clipped_start && !left.clipped_end);
        let right = &l.bars[1];
        assert_eq!((right.rect.x, right.rect.w), (550.0, 150.0));
        assert!(right.clipped_end && !right.clipped_start);
    }

    #[test]
    fn an_instant_flow_keeps_a_visible_width() {
        let d = TimelineData::new(
            vec![lane("a")],
            vec![ended("i", "a", 2000, 2000, FlowOutcome::Passed)],
        );
        let l = TimelineLayout::build(&d, (0, 6000), &config());
        assert_eq!((l.bars[0].rect.x, l.bars[0].rect.w), (300.0, 2.0));
    }

    #[test]
    fn hit_testing_includes_the_left_edge_and_excludes_the_right_and_the_inset() {
        let l = TimelineLayout::build(&data(), (0, 6000), &config());
        let hit = |x, y| l.hit_test(x, y).map(|b| b.id.as_str());
        assert_eq!(hit(300.0, 30.0), Some("a1"));
        assert_eq!(hit(200.0, 24.0), Some("a1"));
        assert_eq!(hit(399.9, 45.9), Some("a1"));
        assert_eq!(hit(400.0, 30.0), None);
        assert_eq!(hit(300.0, 23.9), None); // lane inset above the bar
        assert_eq!(hit(300.0, 46.0), None); // and below it
        assert_eq!(hit(550.0, 60.0), Some("b1"));
        assert_eq!(hit(550.0, 30.0), None); // wrong lane
        assert_eq!(hit(50.0, 30.0), None); // the label gutter
    }

    #[test]
    fn overlapping_bars_hit_the_one_drawn_on_top() {
        let d = TimelineData::new(
            vec![lane("a")],
            vec![
                ended("early", "a", 1000, 4000, FlowOutcome::Passed),
                ended("late", "a", 2000, 5000, FlowOutcome::Failed),
            ],
        );
        let l = TimelineLayout::build(&d, (0, 6000), &config());
        assert_eq!(l.hit_test(350.0, 30.0).map(|b| b.id.as_str()), Some("late"));
        assert_eq!(l.hit_test(150.0, 30.0).map(|b| b.id.as_str()), None);
        assert_eq!(
            l.hit_test(250.0, 30.0).map(|b| b.id.as_str()),
            Some("early")
        );
    }

    #[test]
    fn the_axis_has_ticks_on_round_times_inside_the_plot() {
        let from = 1_700_000_000_000;
        let l = TimelineLayout::build(&data(), (from, from + 60_000), &config());
        let first = l.x_ticks.first().unwrap();
        assert_eq!((first.label.as_str(), first.x), ("22:13:20", 100.0));
        assert!(l.x_ticks.iter().all(|t| (100.0..=700.0).contains(&t.x)));
    }

    #[test]
    fn the_cursor_time_under_the_pointer_is_the_time_a_chart_reads() {
        let l = TimelineLayout::build(&data(), (0, 6000), &config());
        assert_eq!(l.time_at_x(300.0), Some(2000));
        assert_eq!(l.time_at_x(50.0), None);
        assert_eq!(l.time_at_x(701.0), None);
        assert_eq!(l.x_for_time(2000), Some(300.0));
        assert_eq!(l.x_for_time(6001), None);

        // The same time on a chart picks the reading nearest to it.
        let series = Series::new(
            Metric {
                name: "Host CPU".to_owned(),
                unit: Unit::Percent,
            },
            [(0, 10.0), (1000, 20.0), (2000, 30.0), (3000, 40.0)]
                .map(|(at_ms, v)| Point {
                    at_ms,
                    reading: Reading::Value(v),
                })
                .to_vec(),
        );
        let chart = TimeSeriesModel::build(
            series,
            ChartConfig {
                width: 400.0,
                height: 100.0,
                margin: Margin {
                    left: 0.0,
                    right: 0.0,
                    top: 0.0,
                    bottom: 0.0,
                },
                utc_offset_minutes: 0,
                domain_ms: Some((0, 6000)),
            },
        );
        let at = l.time_at_x(330.0).unwrap(); // 2300 ms
        assert_eq!(chart.crosshair_at_time(at).unwrap().value_label, "30%");
    }
}

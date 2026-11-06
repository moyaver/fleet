// SPDX-License-Identifier: AGPL-3.0-only
//! View-model of the time-series chart: scales, ticks, drawable segments, gaps for unavailable
//! readings, the crosshair and the accessible data table.

use std::ops::Range;

use fleet_core::downsample::lttb;

use crate::charts::format::{AxisUnit, format_timestamp};
use crate::charts::scale::{LinearScale, TimeScale, nice_ticks, time_ticks};
use crate::charts::series::{Point, Reading, Series};

const Y_TICK_TARGET: usize = 5;
const X_TICK_TARGET: usize = 6;
/// SVG draws up to this many points per horizontal pixel, a canvas four times as many.
const SVG_POINTS_PER_PX: usize = 2;
const CANVAS_POINTS_PER_PX: usize = 4;

/// Above this many readings the chart is drawn on a canvas instead of SVG (PLAN §5.4).
pub const CANVAS_THRESHOLD: usize = 5_000;

/// Space around the plot area, in pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Margin {
    /// Room for the y-axis labels.
    pub left: f64,
    /// Right padding.
    pub right: f64,
    /// Top padding.
    pub top: f64,
    /// Room for the x-axis labels.
    pub bottom: f64,
}

/// How a chart is laid out.
#[derive(Debug, Clone, PartialEq)]
pub struct ChartConfig {
    /// Total width in pixels.
    pub width: f64,
    /// Total height in pixels.
    pub height: f64,
    /// Space around the plot.
    pub margin: Margin,
    /// The viewer's offset from UTC, for time labels.
    pub utc_offset_minutes: i32,
    /// The visible time range (unix ms); the series' own range when `None`.
    pub domain_ms: Option<(i64, i64)>,
}

impl Default for ChartConfig {
    fn default() -> Self {
        Self {
            width: 640.0,
            height: 240.0,
            margin: Margin {
                left: 56.0,
                right: 12.0,
                top: 8.0,
                bottom: 24.0,
            },
            utc_offset_minutes: 0,
            domain_ms: None,
        }
    }
}

/// How the chart is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Renderer {
    /// SVG elements.
    Svg,
    /// A `<canvas>`, for dense series.
    Canvas,
}

/// A point to draw.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlotPoint {
    /// Pixel x.
    pub x: f64,
    /// Pixel y.
    pub y: f64,
    /// The reading's time, unix ms.
    pub at_ms: i64,
    /// The reading's value.
    pub value: f64,
}

/// A run of consecutive available readings: one polyline.
#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    /// The points to draw, ascending in x. A single point is drawn as a dot.
    pub points: Vec<PlotPoint>,
}

impl Segment {
    /// The SVG path data (`M x y L x y …`), coordinates rounded to a tenth of a pixel. A single
    /// point becomes a zero-length line that a round line cap draws as a dot.
    #[must_use]
    pub fn path_d(&self) -> String {
        let mut d = String::new();
        let mut points = self.points.iter().peekable();
        while let Some(p) = points.next() {
            d.push(if d.is_empty() { 'M' } else { 'L' });
            d.push_str(&coord(p.x));
            d.push(' ');
            d.push_str(&coord(p.y));
            if self.points.len() == 1 && points.peek().is_none() {
                d.push('L');
                d.push_str(&coord(p.x));
                d.push(' ');
                d.push_str(&coord(p.y));
            }
        }
        d
    }
}

/// One path coordinate: a tenth of a pixel is plenty, and shorter paths are cheaper to patch.
fn coord(v: f64) -> String {
    let text = format!("{v:.1}");
    let text = text.trim_end_matches('0').trim_end_matches('.');
    if text.is_empty() || text == "-" {
        "0".to_owned()
    } else {
        text.to_owned()
    }
}

/// A stretch with no reading: drawn as a band and annotated, never as a line or a zero.
#[derive(Debug, Clone, PartialEq)]
pub struct Gap {
    /// First unavailable reading, unix ms.
    pub from_ms: i64,
    /// The next available reading (or the end of the chart), unix ms.
    pub to_ms: i64,
    /// Pixel x of `from_ms`.
    pub x0: f64,
    /// Pixel x of `to_ms`.
    pub x1: f64,
    /// `unavailable (reason)`.
    pub annotation: String,
}

/// A y-axis tick.
#[derive(Debug, Clone, PartialEq)]
pub struct YTick {
    /// The value in the metric's own unit.
    pub value: f64,
    /// Pixel y.
    pub y: f64,
    /// The number, without the unit (the axis label has it).
    pub label: String,
}

/// The y axis.
#[derive(Debug, Clone, PartialEq)]
pub struct YAxis {
    /// The metric name and unit, for example `Host memory used (GiB)`.
    pub label: String,
    /// The ticks, ascending in value.
    pub ticks: Vec<YTick>,
}

/// An x-axis tick.
#[derive(Debug, Clone, PartialEq)]
pub struct XTick {
    /// Unix time, ms.
    pub at_ms: i64,
    /// Pixel x.
    pub x: f64,
    /// The time text.
    pub label: String,
}

/// What is under the pointer or the shared cursor.
#[derive(Debug, Clone, PartialEq)]
pub struct Crosshair {
    /// The nearest reading's time, unix ms.
    pub at_ms: i64,
    /// Pixel x of that reading.
    pub x: f64,
    /// Pixel y, or `None` when the reading is unavailable.
    pub y: Option<f64>,
    /// The time as text.
    pub time_label: String,
    /// The value with its unit, or `unavailable (reason)`.
    pub value_label: String,
}

/// A row of the accessible data table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRow {
    /// The time as text.
    pub time: String,
    /// The value with its unit, or `unavailable (reason)`.
    pub value: String,
}

/// Everything a view needs to draw one time-series chart.
#[derive(Debug, Clone, PartialEq)]
pub struct TimeSeriesModel {
    /// The y axis.
    pub y_axis: YAxis,
    /// The x ticks.
    pub x_ticks: Vec<XTick>,
    /// The lines to draw.
    pub segments: Vec<Segment>,
    /// The bands for unavailable stretches.
    pub gaps: Vec<Gap>,
    /// SVG or canvas.
    pub renderer: Renderer,
    /// The plot rectangle.
    pub plot: PlotArea,
    series: Series,
    config: ChartConfig,
    x_scale: TimeScale,
    y_scale: LinearScale,
    visible: Range<usize>,
}

/// The y-axis title: the metric's name, with its unit in brackets when it has one.
fn axis_label(name: &str, symbol: &str) -> String {
    if symbol.is_empty() {
        name.to_owned()
    } else {
        format!("{name} ({symbol})")
    }
}

impl TimeSeriesModel {
    /// Lays `series` out as `config` says.
    #[must_use]
    pub fn build(series: Series, config: ChartConfig) -> Self {
        let plot = PlotArea::new(&config);
        let points = series.points();
        let domain = config
            .domain_ms
            .or_else(|| Some((points.first()?.at_ms, points.last()?.at_ms)))
            .unwrap_or((0, 1));
        let x_scale = TimeScale::new(domain, (plot.left, plot.right));
        let first = points.partition_point(|p| p.at_ms < domain.0);
        let end = points.partition_point(|p| p.at_ms <= domain.1);
        let visible = first..end.max(first);
        let in_view = points.get(visible.clone()).unwrap_or_default();

        let values = in_view.iter().filter_map(|p| match p.reading {
            Reading::Value(v) => Some(v),
            Reading::Unavailable { .. } => None,
        });
        let (lo, hi) = values.fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), v| {
            (lo.min(v), hi.max(v))
        });
        let (lo, hi) = if lo > hi { (0.0, 1.0) } else { (lo, hi) };
        let lo = if series.metric.unit.zero_based() {
            lo.min(0.0)
        } else {
            lo
        };
        // Ticks are round in the axis unit (1, 2, 3 GiB), not in raw bytes.
        let axis: AxisUnit = series.metric.unit.axis(lo.abs().max(hi.abs()));
        let nice = nice_ticks(axis.scale(lo), axis.scale(hi), Y_TICK_TARGET);
        let y_scale = LinearScale::new(
            (axis.unscale(nice.min), axis.unscale(nice.max)),
            (plot.bottom, plot.top),
        );
        let y_axis = YAxis {
            label: axis_label(&series.metric.name, axis.symbol),
            ticks: nice
                .ticks
                .iter()
                .map(|&tick| axis.unscale(tick))
                .map(|value| YTick {
                    value,
                    y: y_scale.map(value),
                    label: axis.format_tick(value),
                })
                .collect(),
        };
        let x_ticks = time_ticks(domain.0, domain.1, X_TICK_TARGET, config.utc_offset_minutes)
            .into_iter()
            .map(|t| XTick {
                at_ms: t.at_ms,
                x: x_scale.map(t.at_ms),
                label: t.label,
            })
            .collect();

        let (runs, gaps) = runs_and_gaps(in_view, domain.1);
        let available: usize = runs.iter().map(Vec::len).sum();
        let renderer = if available > CANVAS_THRESHOLD {
            Renderer::Canvas
        } else {
            Renderer::Svg
        };
        let per_px = match renderer {
            Renderer::Svg => SVG_POINTS_PER_PX,
            Renderer::Canvas => CANVAS_POINTS_PER_PX,
        };
        let budget = per_px * px_count(plot.right - plot.left);
        let segments = runs
            .iter()
            .map(|run| Segment {
                points: thin(run, budget * run.len() / available.max(1))
                    .into_iter()
                    .map(|(at_ms, value)| PlotPoint {
                        x: x_scale.map(at_ms),
                        y: y_scale.map(value),
                        at_ms,
                        value,
                    })
                    .collect(),
            })
            .collect();
        let gaps = gaps
            .into_iter()
            .map(|g| Gap {
                x0: x_scale.map(g.from_ms),
                x1: x_scale.map(g.to_ms),
                ..g
            })
            .collect();

        Self {
            y_axis,
            x_ticks,
            segments,
            gaps,
            renderer,
            plot,
            series,
            config,
            x_scale,
            y_scale,
            visible,
        }
    }

    /// The reading nearest to a pixel x inside the plot (unavailable readings included).
    #[must_use]
    pub fn crosshair_at_x(&self, x: f64) -> Option<Crosshair> {
        let plot = self.plot;
        if x < plot.left || x > plot.right {
            return None;
        }
        self.crosshair(self.x_scale.invert(x))
    }

    /// The reading nearest to a time, for a cursor shared with another chart; `None` outside the
    /// visible range.
    #[must_use]
    pub fn crosshair_at_time(&self, at_ms: i64) -> Option<Crosshair> {
        self.x_for_time(at_ms)?;
        self.crosshair(at_ms)
    }

    fn crosshair(&self, at_ms: i64) -> Option<Crosshair> {
        let in_view = self.series.points().get(self.visible.clone())?;
        let after = in_view.partition_point(|p| p.at_ms < at_ms);
        let before = after.checked_sub(1).and_then(|i| in_view.get(i));
        let point = match (before, in_view.get(after)) {
            (Some(b), Some(a)) if at_ms - b.at_ms <= a.at_ms - at_ms => b,
            (_, Some(a)) => a,
            (Some(b), None) => b,
            (None, None) => return None,
        };
        let unit = self.series.metric.unit;
        Some(Crosshair {
            at_ms: point.at_ms,
            x: self.x_scale.map(point.at_ms),
            y: match point.reading {
                Reading::Value(v) => Some(self.y_scale.map(v)),
                Reading::Unavailable { .. } => None,
            },
            time_label: format_timestamp(point.at_ms, self.config.utc_offset_minutes),
            value_label: point.reading.label(unit),
        })
    }

    /// Pixel x of a time, `None` outside the visible range.
    #[must_use]
    pub fn x_for_time(&self, at_ms: i64) -> Option<f64> {
        let (from, to) = self.x_scale.domain();
        (from..=to)
            .contains(&at_ms)
            .then(|| self.x_scale.map(at_ms))
    }

    /// The time under a pixel x.
    #[must_use]
    pub fn time_for_x(&self, x: f64) -> i64 {
        self.x_scale.invert(x)
    }

    /// Every reading as a table row, ascending in time.
    #[must_use]
    pub fn table_rows(&self) -> Vec<TableRow> {
        let unit = self.series.metric.unit;
        self.series
            .points()
            .iter()
            .map(|p| TableRow {
                time: format_timestamp(p.at_ms, self.config.utc_offset_minutes),
                value: p.reading.label(unit),
            })
            .collect()
    }

    /// A one-sentence description for assistive technology.
    #[must_use]
    pub fn summary(&self) -> String {
        let unit = self.series.metric.unit;
        let points = self.series.points();
        let (Some(first), Some(last)) = (points.first(), points.last()) else {
            return format!("{}: no readings", self.y_axis.label);
        };
        let offset = self.config.utc_offset_minutes;
        let values: Vec<f64> = points
            .iter()
            .filter_map(|p| match p.reading {
                Reading::Value(v) => Some(v),
                Reading::Unavailable { .. } => None,
            })
            .collect();
        let mut parts = vec![format!(
            "{}: {} readings from {} to {}",
            self.y_axis.label,
            points.len(),
            format_timestamp(first.at_ms, offset),
            format_timestamp(last.at_ms, offset),
        )];
        let min = values.iter().copied().reduce(f64::min);
        let max = values.iter().copied().reduce(f64::max);
        if let (Some(min), Some(max)) = (min, max) {
            parts.push(format!(
                "min {}, max {}",
                unit.format(min),
                unit.format(max)
            ));
        }
        parts.push(format!("latest {}", last.reading.label(unit)));
        let missing = points.len() - values.len();
        if missing > 0 {
            parts.push(format!("{missing} unavailable"));
        }
        parts.join("; ")
    }
}

/// The plot rectangle's edges in pixels: where the lines are drawn, inside the margins.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlotArea {
    /// Left edge.
    pub left: f64,
    /// Right edge.
    pub right: f64,
    /// Top edge.
    pub top: f64,
    /// Bottom edge.
    pub bottom: f64,
}

impl PlotArea {
    fn new(c: &ChartConfig) -> Self {
        Self {
            left: c.margin.left,
            right: (c.width - c.margin.right).max(c.margin.left),
            top: c.margin.top,
            bottom: (c.height - c.margin.bottom).max(c.margin.top),
        }
    }
}

/// Splits readings into runs of available values and gaps of unavailable ones. A gap lasts until
/// the next available reading (or `end_ms`) and a change of reason starts a new one. Gap pixels
/// are filled in by the caller.
pub(crate) fn runs_and_gaps(points: &[Point], end_ms: i64) -> (Vec<Vec<(i64, f64)>>, Vec<Gap>) {
    let gap = |from_ms: i64, to_ms: i64, reason: Option<&str>| Gap {
        from_ms,
        to_ms,
        x0: 0.0,
        x1: 0.0,
        annotation: Reading::unavailable_label(reason),
    };
    let mut runs = Vec::new();
    let mut gaps = Vec::new();
    let mut current: Vec<(i64, f64)> = Vec::new();
    let mut open: Option<(i64, Option<&str>)> = None;
    for p in points {
        match &p.reading {
            Reading::Value(v) => {
                if let Some((from, reason)) = open.take() {
                    gaps.push(gap(from, p.at_ms, reason));
                }
                current.push((p.at_ms, *v));
            }
            Reading::Unavailable { reason } => {
                if !current.is_empty() {
                    runs.push(std::mem::take(&mut current));
                }
                let reason = reason.as_deref();
                match open {
                    Some((_, r)) if r == reason => {}
                    Some((from, r)) => {
                        gaps.push(gap(from, p.at_ms, r));
                        open = Some((p.at_ms, reason));
                    }
                    None => open = Some((p.at_ms, reason)),
                }
            }
        }
    }
    if !current.is_empty() {
        runs.push(current);
    }
    if let Some((from, reason)) = open {
        gaps.push(gap(from, end_ms.max(from), reason));
    }
    (runs, gaps)
}

/// At most `budget` of the readings, chosen by LTTB; all of them when they fit.
pub(crate) fn thin(run: &[(i64, f64)], budget: usize) -> Vec<(i64, f64)> {
    if run.len() <= budget.max(2) {
        return run.to_vec();
    }
    let xy: Vec<(f64, f64)> = run.iter().map(|&(t, v)| (ms_to_f64(t), v)).collect();
    lttb(&xy, budget.max(2))
        .into_iter()
        .filter_map(|i| run.get(i).copied())
        .collect()
}

#[expect(
    clippy::cast_precision_loss,
    reason = "unix milliseconds stay far below 2^53 until the year 287396"
)]
fn ms_to_f64(ms: i64) -> f64 {
    ms as f64
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "a pixel width is a small non-negative number"
)]
pub(crate) fn px_count(width: f64) -> usize {
    width.max(0.0).round() as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::charts::format::Unit;
    use crate::charts::series::Metric;

    const GIB: f64 = 1_073_741_824.0;

    /// 600 × 200 with margins 50 / 10 / 10 / 20: the plot is x 50..590, y 10..180.
    fn config() -> ChartConfig {
        ChartConfig {
            width: 600.0,
            height: 200.0,
            margin: Margin {
                left: 50.0,
                right: 10.0,
                top: 10.0,
                bottom: 20.0,
            },
            utc_offset_minutes: 0,
            domain_ms: None,
        }
    }

    fn value(at_ms: i64, v: f64) -> Point {
        Point {
            at_ms,
            reading: Reading::Value(v),
        }
    }

    fn missing(at_ms: i64, reason: Option<&str>) -> Point {
        Point {
            at_ms,
            reading: Reading::Unavailable {
                reason: reason.map(str::to_owned),
            },
        }
    }

    fn series(name: &str, unit: Unit, points: Vec<Point>) -> Series {
        Series::new(
            Metric {
                name: name.to_owned(),
                unit,
            },
            points,
        )
    }

    fn approx(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-6, "{a} != {b}");
    }

    #[test]
    fn the_axis_label_carries_the_metric_name_and_the_unit_of_its_ticks() {
        let m = TimeSeriesModel::build(
            series(
                "Host memory used",
                Unit::Bytes,
                vec![value(0, 0.0), value(60_000, 3.0 * GIB)],
            ),
            config(),
        );
        assert_eq!(m.y_axis.label, "Host memory used (GiB)");
        let labels: Vec<&str> = m.y_axis.ticks.iter().map(|t| t.label.as_str()).collect();
        assert_eq!(labels, ["0", "1", "2", "3"]);
        // Zero sits on the bottom of the plot, the top tick on its top.
        approx(m.y_axis.ticks.first().unwrap().y, 180.0);
        approx(m.y_axis.ticks.last().unwrap().y, 10.0);

        let cpu = TimeSeriesModel::build(
            series(
                "Host CPU",
                Unit::Percent,
                vec![value(0, 10.0), value(1000, 40.0)],
            ),
            config(),
        );
        assert_eq!(cpu.y_axis.label, "Host CPU (%)");
    }

    #[test]
    fn unavailable_readings_are_gaps_never_zeros_or_bridged_lines() {
        // x(t) = 50 + t / 40_000 * 540.
        let m = TimeSeriesModel::build(
            series(
                "Battery temperature",
                Unit::Celsius,
                vec![
                    value(0, 31.0),
                    value(10_000, 32.0),
                    missing(20_000, Some("sensor not present")),
                    missing(30_000, Some("sensor not present")),
                    value(40_000, 33.0),
                ],
            ),
            config(),
        );
        let drawn: Vec<Vec<i64>> = m
            .segments
            .iter()
            .map(|s| s.points.iter().map(|p| p.at_ms).collect())
            .collect();
        assert_eq!(drawn, [vec![0, 10_000], vec![40_000]]);
        assert_eq!(m.gaps.len(), 1);
        let gap = &m.gaps[0];
        assert_eq!((gap.from_ms, gap.to_ms), (20_000, 40_000));
        approx(gap.x0, 320.0);
        approx(gap.x1, 590.0);
        assert_eq!(gap.annotation, "unavailable (sensor not present)");
    }

    #[test]
    fn a_segment_is_an_svg_path_and_a_lone_point_a_dot() {
        let p = |x, y| PlotPoint {
            x,
            y,
            at_ms: 0,
            value: 0.0,
        };
        let line = Segment {
            points: vec![p(50.0, 180.0), p(60.26, 100.04), p(70.0, 10.0)],
        };
        assert_eq!(line.path_d(), "M50 180L60.3 100L70 10");
        let dot = Segment {
            points: vec![p(5.0, 6.0)],
        };
        assert_eq!(dot.path_d(), "M5 6L5 6");
        assert_eq!(Segment { points: vec![] }.path_d(), "");
    }

    #[test]
    fn a_real_zero_is_drawn_and_a_missing_value_without_a_reason_is_plainly_unavailable() {
        let m = TimeSeriesModel::build(
            series(
                "Host CPU",
                Unit::Percent,
                vec![value(0, 0.0), missing(1000, None)],
            ),
            config(),
        );
        assert_eq!(m.segments.len(), 1);
        assert_eq!(m.segments[0].points.len(), 1);
        approx(m.segments[0].points[0].value, 0.0);
        assert_eq!(m.gaps[0].annotation, "unavailable");
    }

    #[test]
    fn leading_and_trailing_unavailable_stretches_are_gaps_too() {
        let m = TimeSeriesModel::build(
            series(
                "Host CPU",
                Unit::Percent,
                vec![
                    missing(0, Some("warming up")),
                    value(1000, 5.0),
                    value(2000, 6.0),
                    missing(3000, Some("agent paused")),
                ],
            ),
            config(),
        );
        let spans: Vec<(i64, i64, &str)> = m
            .gaps
            .iter()
            .map(|g| (g.from_ms, g.to_ms, g.annotation.as_str()))
            .collect();
        assert_eq!(
            spans,
            [
                (0, 1000, "unavailable (warming up)"),
                (3000, 3000, "unavailable (agent paused)"),
            ]
        );
    }

    #[test]
    fn a_different_reason_starts_a_new_gap() {
        let m = TimeSeriesModel::build(
            series(
                "Host CPU",
                Unit::Percent,
                vec![
                    missing(0, Some("a")),
                    missing(1000, Some("b")),
                    value(2000, 1.0),
                ],
            ),
            config(),
        );
        let notes: Vec<&str> = m.gaps.iter().map(|g| g.annotation.as_str()).collect();
        assert_eq!(notes, ["unavailable (a)", "unavailable (b)"]);
    }

    #[test]
    fn dense_series_are_downsampled_to_real_points_and_switch_to_canvas_above_5k() {
        // A noisy line so LTTB has something to choose between.
        let noisy = |n: i64| -> Vec<Point> {
            (0..n)
                .map(|i| value(i * 100, f64::from(i32::try_from((i * 7919) % 101).unwrap())))
                .collect()
        };
        for (n, renderer, budget) in [
            (1_000, Renderer::Svg, 1080),
            (3_000, Renderer::Svg, 1080),
            (10_000, Renderer::Canvas, 2160),
        ] {
            let points = noisy(n);
            let m =
                TimeSeriesModel::build(series("Host CPU", Unit::Percent, points.clone()), config());
            assert_eq!(m.renderer, renderer, "{n} points");
            let drawn: Vec<&PlotPoint> = m.segments.iter().flat_map(|s| &s.points).collect();
            assert!(drawn.len() <= budget, "{} > {budget}", drawn.len());
            assert!(drawn.len() > 100);
            // Never invents a point: each one is an input reading.
            for p in &drawn {
                assert!(
                    points
                        .iter()
                        .any(|q| q.at_ms == p.at_ms && q.reading == Reading::Value(p.value))
                );
            }
            assert_eq!(drawn.first().map(|p| p.at_ms), Some(0));
            assert_eq!(drawn.last().map(|p| p.at_ms), Some((n - 1) * 100));
        }
        // Below the budget nothing is dropped.
        let m = TimeSeriesModel::build(series("Host CPU", Unit::Percent, noisy(1_000)), config());
        assert_eq!(m.segments[0].points.len(), 1_000);
    }

    #[test]
    fn the_crosshair_snaps_to_the_nearest_reading_and_reads_gaps_honestly() {
        let m = TimeSeriesModel::build(
            series(
                "Host memory used",
                Unit::Bytes,
                vec![
                    value(0, 1.5 * 1024.0),
                    missing(20_000, Some("sensor not present")),
                    value(40_000, 2.0 * GIB),
                ],
            ),
            config(),
        );
        // x = 50 is t = 0; x = 330 is t ≈ 20_740 and nearest to the unavailable reading.
        let first = m.crosshair_at_x(60.0).unwrap();
        assert_eq!(first.at_ms, 0);
        assert_eq!(first.value_label, "1.5 KiB");
        assert_eq!(first.time_label, "1970-01-01 00:00:00");
        assert!(first.y.is_some());
        let gap = m.crosshair_at_x(330.0).unwrap();
        assert_eq!(gap.at_ms, 20_000);
        assert_eq!(gap.value_label, "unavailable (sensor not present)");
        assert_eq!(gap.y, None);
        assert_eq!(m.crosshair_at_x(580.0).unwrap().value_label, "2 GiB");
        // Outside the plot there is nothing to read.
        assert_eq!(m.crosshair_at_x(10.0), None);
        assert_eq!(m.crosshair_at_x(595.0), None);
    }

    #[test]
    fn a_shared_cursor_time_maps_to_x_and_to_the_nearest_reading() {
        let m = TimeSeriesModel::build(
            series(
                "Host CPU",
                Unit::Percent,
                vec![value(0, 10.0), value(40_000, 30.0)],
            ),
            config(),
        );
        approx(m.x_for_time(20_000).unwrap(), 320.0);
        assert_eq!(m.x_for_time(-1), None);
        assert_eq!(m.x_for_time(40_001), None);
        assert_eq!(m.time_for_x(320.0), 20_000);
        assert_eq!(m.crosshair_at_time(30_000).unwrap().value_label, "30%");
        assert_eq!(m.crosshair_at_time(50_000), None);
    }

    #[test]
    fn the_data_table_lists_every_reading_with_unit_or_unavailable() {
        let m = TimeSeriesModel::build(
            series(
                "Battery temperature",
                Unit::Celsius,
                vec![
                    value(1_700_000_000_000, 31.5),
                    missing(1_700_000_005_000, Some("sensor not present")),
                ],
            ),
            config(),
        );
        assert_eq!(
            m.table_rows(),
            [
                TableRow {
                    time: "2023-11-14 22:13:20".to_owned(),
                    value: "31.5 °C".to_owned()
                },
                TableRow {
                    time: "2023-11-14 22:13:25".to_owned(),
                    value: "unavailable (sensor not present)".to_owned()
                },
            ]
        );
    }

    #[test]
    fn the_summary_names_the_metric_range_and_what_is_missing() {
        let m = TimeSeriesModel::build(
            series(
                "Host CPU",
                Unit::Percent,
                vec![
                    value(0, 10.0),
                    value(1000, 50.0),
                    missing(2000, Some("agent paused")),
                ],
            ),
            config(),
        );
        assert_eq!(
            m.summary(),
            "Host CPU (%): 3 readings from 1970-01-01 00:00:00 to 1970-01-01 00:00:02; \
             min 10%, max 50%; latest unavailable (agent paused); 1 unavailable"
        );
    }
}

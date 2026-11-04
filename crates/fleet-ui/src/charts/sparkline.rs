// SPDX-License-Identifier: AGPL-3.0-only
//! View-model of the sparkline: a small line with the minimum, maximum and latest reading.

use crate::charts::scale::{LinearScale, TimeScale};
use crate::charts::series::{Reading, Series};
use crate::charts::time_series::{Gap, PlotPoint, Segment, px_count, runs_and_gaps, thin};

/// Everything a view needs to draw one sparkline.
#[derive(Debug, Clone, PartialEq)]
pub struct SparklineModel {
    /// The lines to draw; unavailable stretches break them.
    pub segments: Vec<Segment>,
    /// The unavailable stretches, as bands.
    pub gaps: Vec<Gap>,
    /// Smallest available reading with its unit.
    pub min_label: Option<String>,
    /// Largest available reading with its unit.
    pub max_label: Option<String>,
    /// The latest reading with its unit, or `unavailable (reason)`: never the last good value.
    pub latest_label: String,
    /// A description for assistive technology.
    pub description: String,
}

impl SparklineModel {
    /// Lays `series` out in a `width` × `height` box.
    #[must_use]
    pub fn build(series: &Series, width: f64, height: f64) -> Self {
        let unit = series.metric.unit;
        let points = series.points();
        let domain = match (points.first(), points.last()) {
            (Some(f), Some(l)) => (f.at_ms, l.at_ms),
            _ => (0, 1),
        };
        let x_scale = TimeScale::new(domain, (0.0, width));
        let values = points.iter().filter_map(|p| match p.reading {
            Reading::Value(v) => Some(v),
            Reading::Unavailable { .. } => None,
        });
        let (min, max) = values.fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), v| {
            (lo.min(v), hi.max(v))
        });
        let has_values = min <= max;
        // The shape matters, not the baseline: the line fills the box.
        let y_scale = LinearScale::new(
            if has_values { (min, max) } else { (0.0, 1.0) },
            (height, 0.0),
        );

        let (runs, gaps) = runs_and_gaps(points, domain.1);
        let available: usize = runs.iter().map(Vec::len).sum();
        let budget = 2 * px_count(width);
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

        let min_label = has_values.then(|| unit.format(min));
        let max_label = has_values.then(|| unit.format(max));
        let latest_label = points.last().map_or_else(
            || Reading::unavailable_label(None),
            |p| p.reading.label(unit),
        );
        let description = if points.is_empty() {
            format!("{}: no readings", series.metric.name)
        } else {
            let mut parts = vec![format!("latest {latest_label}")];
            if let (Some(lo), Some(hi)) = (&min_label, &max_label) {
                parts.push(format!("min {lo}"));
                parts.push(format!("max {hi}"));
            }
            format!("{}: {}", series.metric.name, parts.join(", "))
        };
        Self {
            segments,
            gaps,
            min_label,
            max_label,
            latest_label,
            description,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::charts::format::Unit;
    use crate::charts::series::{Metric, Point};

    fn series(points: Vec<Point>) -> Series {
        Series::new(
            Metric {
                name: "Host CPU".to_owned(),
                unit: Unit::Percent,
            },
            points,
        )
    }

    fn value(at_ms: i64, v: f64) -> Point {
        Point {
            at_ms,
            reading: Reading::Value(v),
        }
    }

    #[test]
    fn min_max_and_latest_come_with_units() {
        let m = SparklineModel::build(
            &series(vec![value(0, 30.0), value(1000, 90.0), value(2000, 50.0)]),
            100.0,
            20.0,
        );
        assert_eq!(m.min_label.as_deref(), Some("30%"));
        assert_eq!(m.max_label.as_deref(), Some("90%"));
        assert_eq!(m.latest_label, "50%");
        assert_eq!(m.description, "Host CPU: latest 50%, min 30%, max 90%");
        // Smallest at the bottom of the box, largest at the top: y = 20 - (v - 30) / 60 * 20.
        let ys: Vec<f64> = m.segments[0].points.iter().map(|p| p.y).collect();
        for (y, want) in ys.iter().zip([20.0, 0.0, 13.333_333]) {
            assert!((y - want).abs() < 1e-4, "{ys:?}");
        }
        let xs: Vec<f64> = m.segments[0].points.iter().map(|p| p.x).collect();
        assert_eq!(xs, [0.0, 50.0, 100.0]);
    }

    #[test]
    fn an_unavailable_latest_reading_is_never_replaced_by_the_last_good_value() {
        let m = SparklineModel::build(
            &series(vec![
                value(0, 30.0),
                value(1000, 90.0),
                Point {
                    at_ms: 2000,
                    reading: Reading::Unavailable {
                        reason: Some("agent paused".to_owned()),
                    },
                },
            ]),
            100.0,
            20.0,
        );
        assert_eq!(m.latest_label, "unavailable (agent paused)");
        assert_eq!(m.min_label.as_deref(), Some("30%"));
        assert_eq!(m.gaps.len(), 1);
        assert_eq!(m.segments.len(), 1);
    }

    #[test]
    fn an_empty_or_all_unavailable_series_has_no_min_or_max() {
        let m = SparklineModel::build(&series(Vec::new()), 100.0, 20.0);
        assert_eq!((m.min_label, m.max_label), (None, None));
        assert_eq!(m.latest_label, "unavailable");
        assert_eq!(m.segments, []);
        assert_eq!(m.description, "Host CPU: no readings");
    }

    #[test]
    fn a_flat_line_sits_in_the_middle() {
        let m = SparklineModel::build(&series(vec![value(0, 7.0), value(1000, 7.0)]), 100.0, 20.0);
        assert!(
            m.segments[0]
                .points
                .iter()
                .all(|p| (p.y - 10.0).abs() < 1e-9)
        );
    }
}

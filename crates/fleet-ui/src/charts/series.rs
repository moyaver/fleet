// SPDX-License-Identifier: AGPL-3.0-only
//! A metric series: readings over time, some of which may be unavailable.
//!
//! A reading that is not there stays "unavailable" with its reason all the way to the screen. It
//! is never turned into a zero or filled in from its neighbours (spec acceptance criterion 5).

use fleet_protocol::frame::Sample;

use crate::charts::format::Unit;

/// What a series measures: its display name and unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Metric {
    /// The name shown on the axis and in the table.
    pub name: String,
    /// The unit its values are in.
    pub unit: Unit,
}

/// One reading: a value, or the fact that there is none.
#[derive(Debug, Clone, PartialEq)]
pub enum Reading {
    /// A measured value (always finite).
    Value(f64),
    /// No reading, and why, when known.
    Unavailable {
        /// For example `sensor not present`.
        reason: Option<String>,
    },
}

impl Reading {
    /// `1.5 GiB`, or `unavailable (reason)`: an unavailable reading never shows a number.
    #[must_use]
    pub fn label(&self, unit: Unit) -> String {
        match self {
            Self::Value(v) => unit.format(*v),
            Self::Unavailable { reason } => Self::unavailable_label(reason.as_deref()),
        }
    }

    pub(crate) fn unavailable_label(reason: Option<&str>) -> String {
        match reason {
            Some(r) => format!("unavailable ({r})"),
            None => "unavailable".to_owned(),
        }
    }
}

/// A reading at a time.
#[derive(Debug, Clone, PartialEq)]
pub struct Point {
    /// Unix time, milliseconds.
    pub at_ms: i64,
    /// The reading.
    pub reading: Reading,
}

/// Readings of one metric, ascending in time.
#[derive(Debug, Clone, PartialEq)]
pub struct Series {
    /// What is measured.
    pub metric: Metric,
    points: Vec<Point>,
}

impl Series {
    /// A series of `points`, sorted by time. A non-finite value becomes unavailable.
    #[must_use]
    pub fn new(metric: Metric, mut points: Vec<Point>) -> Self {
        for p in &mut points {
            if matches!(p.reading, Reading::Value(v) if !v.is_finite()) {
                p.reading = Reading::Unavailable {
                    reason: Some("not a finite number".to_owned()),
                };
            }
        }
        points.sort_by_key(|p| p.at_ms);
        Self { metric, points }
    }

    /// The readings, ascending in time.
    #[must_use]
    pub fn points(&self) -> &[Point] {
        &self.points
    }
}

/// The metrics an agent reports in a [`Sample`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleMetric {
    /// Host CPU across all cores.
    HostCpu,
    /// Host memory in use.
    HostMemUsed,
    /// Host memory installed.
    HostMemTotal,
    /// Maestro's JVM CPU.
    MaestroCpu,
    /// Maestro's JVM resident memory.
    MaestroRss,
    /// The agent's own CPU.
    AgentCpu,
    /// The agent's own resident memory.
    AgentRss,
}

impl SampleMetric {
    /// Every metric a sample carries.
    pub const ALL: [Self; 7] = [
        Self::HostCpu,
        Self::HostMemUsed,
        Self::HostMemTotal,
        Self::MaestroCpu,
        Self::MaestroRss,
        Self::AgentCpu,
        Self::AgentRss,
    ];

    /// The name and unit shown for this metric.
    #[must_use]
    pub fn metric(self) -> Metric {
        let (name, unit) = match self {
            Self::HostCpu => ("Host CPU", Unit::Percent),
            Self::HostMemUsed => ("Host memory used", Unit::Bytes),
            Self::HostMemTotal => ("Host memory total", Unit::Bytes),
            Self::MaestroCpu => ("Maestro CPU", Unit::PercentOfCore),
            Self::MaestroRss => ("Maestro memory (RSS)", Unit::Bytes),
            Self::AgentCpu => ("Agent CPU", Unit::PercentOfCore),
            Self::AgentRss => ("Agent memory (RSS)", Unit::Bytes),
        };
        Metric {
            name: name.to_owned(),
            unit,
        }
    }

    /// This metric over `samples`; a sample that doesn't carry it is an unavailable reading.
    #[must_use]
    pub fn series(self, samples: &[Sample]) -> Series {
        let points = samples
            .iter()
            .map(|s| Point {
                at_ms: i64::try_from(s.at.wall.unix_timestamp_nanos() / 1_000_000)
                    .unwrap_or(i64::MAX),
                reading: match self.value(s) {
                    Some(v) => Reading::Value(v),
                    None => Reading::Unavailable {
                        reason: Some("not reported".to_owned()),
                    },
                },
            })
            .collect();
        Series::new(self.metric(), points)
    }

    fn value(self, s: &Sample) -> Option<f64> {
        #[expect(
            clippy::cast_precision_loss,
            reason = "byte counts above 2^53 (8 PiB) do not occur on a runner"
        )]
        let bytes = |v: Option<u64>| v.map(|v| v as f64);
        let pct = |v: Option<f32>| v.map(f64::from);
        match self {
            Self::HostCpu => pct(s.host_cpu_pct),
            Self::HostMemUsed => bytes(s.host_mem_used_bytes),
            Self::HostMemTotal => bytes(s.host_mem_total_bytes),
            Self::MaestroCpu => pct(s.maestro_cpu_pct),
            Self::MaestroRss => bytes(s.maestro_rss_bytes),
            Self::AgentCpu => pct(s.agent_cpu_pct),
            Self::AgentRss => bytes(s.agent_rss_bytes),
        }
    }
}

#[cfg(test)]
mod tests {
    use fleet_protocol::journal::ClockSample;
    use time::OffsetDateTime;

    use super::*;

    fn sample(secs: i64, cpu: Option<f32>, mem: Option<u64>) -> Sample {
        Sample {
            at: ClockSample {
                wall: OffsetDateTime::from_unix_timestamp(secs).unwrap(),
                mono_ms: 0,
            },
            host_cpu_pct: cpu,
            host_mem_used_bytes: mem,
            host_mem_total_bytes: None,
            maestro_cpu_pct: None,
            maestro_rss_bytes: None,
            agent_cpu_pct: None,
            agent_rss_bytes: None,
        }
    }

    #[test]
    fn samples_become_readings_and_a_missing_field_is_unavailable_not_zero() {
        let samples = [
            sample(10, Some(12.5), Some(2048)),
            sample(15, None, Some(4096)),
        ];
        let cpu = SampleMetric::HostCpu.series(&samples);
        assert_eq!(cpu.metric.name, "Host CPU");
        assert_eq!(cpu.metric.unit, Unit::Percent);
        assert_eq!(
            cpu.points(),
            [
                Point {
                    at_ms: 10_000,
                    reading: Reading::Value(12.5)
                },
                Point {
                    at_ms: 15_000,
                    reading: Reading::Unavailable {
                        reason: Some("not reported".to_owned())
                    }
                },
            ]
        );
        let mem = SampleMetric::HostMemUsed.series(&samples);
        assert_eq!(mem.metric.unit, Unit::Bytes);
        assert_eq!(
            mem.points().get(1).map(|p| p.reading.clone()),
            Some(Reading::Value(4096.0))
        );
    }

    #[test]
    fn a_series_is_ordered_by_time_and_never_holds_a_non_finite_value() {
        let metric = SampleMetric::HostCpu.metric();
        let s = Series::new(
            metric,
            vec![
                Point {
                    at_ms: 2000,
                    reading: Reading::Value(f64::NAN),
                },
                Point {
                    at_ms: 1000,
                    reading: Reading::Value(1.0),
                },
            ],
        );
        let times: Vec<i64> = s.points().iter().map(|p| p.at_ms).collect();
        assert_eq!(times, [1000, 2000]);
        assert!(matches!(
            s.points().get(1).map(|p| &p.reading),
            Some(Reading::Unavailable { reason: Some(_) })
        ));
    }
}

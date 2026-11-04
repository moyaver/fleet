// SPDX-License-Identifier: AGPL-3.0-only
//! Unit-aware number, duration and time formatting.

/// What a metric is measured in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    /// Bytes, shown with binary prefixes (KiB, MiB, GiB, TiB).
    Bytes,
    /// A percentage of the whole machine.
    Percent,
    /// A percentage of one CPU core: a busy process can exceed 100.
    PercentOfCore,
    /// Degrees Celsius.
    Celsius,
    /// Milliseconds, shown as seconds from one second up.
    Millis,
}

impl Unit {
    /// Formats one value with its unit, for tooltips and table cells.
    #[must_use]
    pub fn format(self, value: f64) -> String {
        match self {
            Self::Bytes => with_symbol(AxisUnit::for_bytes(value, true), value),
            Self::Percent => format!("{}%", number(value)),
            Self::PercentOfCore => format!("{}% of one core", number(value)),
            Self::Celsius => format!("{} °C", number(value)),
            Self::Millis => with_symbol(AxisUnit::for_millis(value), value),
        }
    }

    /// Whether a chart of this unit starts at zero: sizes, loads and durations can't be negative,
    /// and a baseline above zero would exaggerate small changes.
    #[must_use]
    pub fn zero_based(self) -> bool {
        !matches!(self, Self::Celsius)
    }

    /// The unit prefix for an axis whose largest value is `max_abs`.
    #[must_use]
    pub fn axis(self, max_abs: f64) -> AxisUnit {
        match self {
            Self::Bytes => AxisUnit::for_bytes(max_abs, false),
            Self::Percent => AxisUnit::plain("%"),
            Self::PercentOfCore => AxisUnit::plain("% of one core"),
            Self::Celsius => AxisUnit::plain("°C"),
            Self::Millis => AxisUnit::for_millis(max_abs),
        }
    }
}

/// Formats a duration in milliseconds: `850 ms`, `4.2 s`, `1 min 5 s`, `1 h 2 min`.
#[must_use]
pub fn format_duration(ms: i64) -> String {
    let ms = ms.max(0);
    if ms < 1000 {
        return format!("{ms} ms");
    }
    if ms < 10_000 {
        let tenths = ms / 100;
        let (whole, frac) = (tenths / 10, tenths % 10);
        return if frac == 0 {
            format!("{whole} s")
        } else {
            format!("{whole}.{frac} s")
        };
    }
    let secs = ms / 1000;
    if secs < 60 {
        return format!("{secs} s");
    }
    let mins = secs / 60;
    if mins < 60 {
        return match secs % 60 {
            0 => format!("{mins} min"),
            s => format!("{mins} min {s} s"),
        };
    }
    match mins % 60 {
        0 => format!("{} h", mins / 60),
        m => format!("{} h {m} min", mins / 60),
    }
}

/// Formats a Unix time in milliseconds as `YYYY-MM-DD HH:MM:SS` at a UTC offset in minutes.
#[must_use]
pub fn format_timestamp(unix_ms: i64, utc_offset_minutes: i32) -> String {
    let secs = unix_ms
        .div_euclid(1000)
        .saturating_add(i64::from(utc_offset_minutes) * 60);
    let Ok(t) = time::OffsetDateTime::from_unix_timestamp(secs) else {
        return "invalid time".to_owned();
    };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second()
    )
}

/// A unit prefix chosen once for a whole axis, so every tick reads in the same scale.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AxisUnit {
    /// What the scaled numbers are in: `MiB`, `s`, `%`.
    pub symbol: &'static str,
    divisor: f64,
}

const BYTE_SYMBOLS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];

impl AxisUnit {
    fn plain(symbol: &'static str) -> Self {
        Self {
            symbol,
            divisor: 1.0,
        }
    }

    fn for_millis(magnitude: f64) -> Self {
        if magnitude.abs() >= 1000.0 {
            Self {
                symbol: "s",
                divisor: 1000.0,
            }
        } else {
            Self::plain("ms")
        }
    }

    fn for_bytes(magnitude: f64, rounded: bool) -> Self {
        let mut k = 0;
        let mut divisor = 1.0;
        while k + 1 < BYTE_SYMBOLS.len() && magnitude.abs() / divisor >= 1024.0 {
            k += 1;
            divisor *= 1024.0;
        }
        // 1_048_575 B is "1024 KiB" once rounded: show it as 1 MiB instead.
        if rounded
            && k + 1 < BYTE_SYMBOLS.len()
            && number(magnitude.abs() / divisor)
                .parse::<f64>()
                .is_ok_and(|v| v >= 1024.0)
        {
            k += 1;
            divisor *= 1024.0;
        }
        Self {
            symbol: BYTE_SYMBOLS.get(k).copied().unwrap_or("B"),
            divisor,
        }
    }

    /// The value in this axis unit.
    #[must_use]
    pub fn scale(&self, value: f64) -> f64 {
        value / self.divisor
    }

    /// The inverse of [`AxisUnit::scale`].
    #[must_use]
    pub fn unscale(&self, scaled: f64) -> f64 {
        scaled * self.divisor
    }

    /// A tick label: the scaled number without its unit, which the axis label carries.
    #[must_use]
    pub fn format_tick(&self, value: f64) -> String {
        number(self.scale(value))
    }
}

/// `value` in the prefix that suits `value` itself, with its symbol (bytes and milliseconds).
fn with_symbol(axis: AxisUnit, value: f64) -> String {
    format!("{} {}", number(axis.scale(value)), axis.symbol)
}

/// Three significant digits at most, no trailing zeros: `1.5`, `12.3`, `123`.
fn number(value: f64) -> String {
    let decimals = if value.abs() >= 100.0 {
        0
    } else if value.abs() >= 10.0 {
        1
    } else {
        2
    };
    let text = format!("{value:.decimals$}");
    let text = if text.contains('.') {
        text.trim_end_matches('0').trim_end_matches('.')
    } else {
        &text
    };
    if text == "-0" {
        "0".to_owned()
    } else {
        text.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_use_binary_prefixes() {
        assert_eq!(Unit::Bytes.format(0.0), "0 B");
        assert_eq!(Unit::Bytes.format(512.0), "512 B");
        assert_eq!(Unit::Bytes.format(1536.0), "1.5 KiB");
        assert_eq!(Unit::Bytes.format(1_048_576.0), "1 MiB");
        assert_eq!(Unit::Bytes.format(5_368_709_120.0), "5 GiB");
    }

    #[test]
    fn bytes_never_show_1024_of_a_unit() {
        assert_eq!(Unit::Bytes.format(1_048_575.0), "1 MiB");
    }

    #[test]
    fn percent_celsius_and_millis_carry_their_unit() {
        assert_eq!(Unit::Percent.format(0.0), "0%");
        assert_eq!(Unit::Percent.format(42.0), "42%");
        assert_eq!(Unit::Percent.format(42.46), "42.5%");
        assert_eq!(Unit::Percent.format(250.0), "250%");
        assert_eq!(Unit::PercentOfCore.format(180.0), "180% of one core");
        assert_eq!(Unit::Celsius.format(36.5), "36.5 °C");
        assert_eq!(Unit::Celsius.format(-3.0), "-3 °C");
        assert_eq!(Unit::Millis.format(250.0), "250 ms");
        assert_eq!(Unit::Millis.format(1500.0), "1.5 s");
        assert_eq!(Unit::Millis.format(90_000.0), "90 s");
    }

    #[test]
    fn a_whole_axis_shares_one_unit() {
        let gib = Unit::Bytes.axis(3.0 * 1_073_741_824.0);
        assert_eq!(gib.symbol, "GiB");
        assert_eq!(gib.format_tick(1.5 * 1_073_741_824.0), "1.5");
        assert_eq!(Unit::Bytes.axis(900.0).symbol, "B");
        assert_eq!(Unit::Millis.axis(5000.0).symbol, "s");
        assert_eq!(Unit::Millis.axis(800.0).symbol, "ms");
        assert_eq!(Unit::Percent.axis(100.0).symbol, "%");
        assert_eq!(Unit::Celsius.axis(40.0).symbol, "°C");
    }

    #[test]
    fn durations_read_naturally() {
        assert_eq!(format_duration(0), "0 ms");
        assert_eq!(format_duration(850), "850 ms");
        assert_eq!(format_duration(4_200), "4.2 s");
        assert_eq!(format_duration(45_000), "45 s");
        assert_eq!(format_duration(60_000), "1 min");
        assert_eq!(format_duration(65_000), "1 min 5 s");
        assert_eq!(format_duration(3_720_000), "1 h 2 min");
        assert_eq!(format_duration(-5), "0 ms");
    }

    #[test]
    fn timestamps_are_utc_unless_an_offset_is_given() {
        // 1_700_000_000 s is 2023-11-14T22:13:20Z.
        assert_eq!(
            format_timestamp(1_700_000_000_000, 0),
            "2023-11-14 22:13:20"
        );
        assert_eq!(
            format_timestamp(1_700_000_000_000, 120),
            "2023-11-15 00:13:20"
        );
        assert_eq!(
            format_timestamp(1_700_000_000_000, -60),
            "2023-11-14 21:13:20"
        );
    }
}

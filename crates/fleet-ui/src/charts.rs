// SPDX-License-Identifier: AGPL-3.0-only
//! Charts (PLAN §10.2 order 5): sparkline and time-series chart.
//!
//! The view-models are pure. A missing reading is a gap, never a zero and never interpolated
//! (spec acceptance criterion 5); axis labels carry the metric name and unit (criterion 4).

pub mod format;
pub mod scale;
pub mod series;
pub mod sparkline;
pub mod time_series;
pub mod view;

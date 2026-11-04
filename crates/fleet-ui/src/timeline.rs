// SPDX-License-Identifier: AGPL-3.0-only
//! Swimlane timeline (PLAN §10.2 order 6): devices as lanes, flows as bars.
//!
//! The view-models are pure: [`model`] holds the data, [`style`] maps outcome and provenance to a
//! look, [`viewport`] zooms and pans, [`layout`] turns it all into rectangles and answers
//! hit-tests, [`nav`] orders keyboard focus and [`table`] is the accessible alternative. The
//! Leptos view is in [`view`].

pub mod layout;
pub mod model;
pub mod nav;
pub mod style;
pub mod table;
pub mod view;
pub mod viewport;

// SPDX-License-Identifier: AGPL-3.0-only
//! Small helpers shared by the canvas-drawing views. No logic worth testing natively: drawing
//! is covered by the browser end-to-end tests.

use leptos::wasm_bindgen::JsCast;
use web_sys::{CanvasRenderingContext2d, HtmlCanvasElement};

/// Sizes the canvas backing store for the device pixel ratio and returns a context whose
/// coordinates are CSS pixels, cleared.
pub(crate) fn prepare(
    canvas: &HtmlCanvasElement,
    width: f64,
    height: f64,
) -> Option<CanvasRenderingContext2d> {
    let dpr = web_sys::window()?.device_pixel_ratio().max(1.0);
    canvas.set_width(device_px(width * dpr));
    canvas.set_height(device_px(height * dpr));
    let ctx = canvas
        .get_context("2d")
        .ok()??
        .dyn_into::<CanvasRenderingContext2d>()
        .ok()?;
    ctx.set_transform(dpr, 0.0, 0.0, dpr, 0.0, 0.0).ok()?;
    ctx.clear_rect(0.0, 0.0, width, height);
    Some(ctx)
}

/// The computed value of a CSS custom property on the canvas, such as `--tone-info-fg`, or
/// `fallback` when it isn't set. Reading the theme's tokens keeps canvas and SVG in step.
pub(crate) fn css_var(canvas: &HtmlCanvasElement, name: &str, fallback: &str) -> String {
    web_sys::window()
        .and_then(|w| w.get_computed_style(canvas).ok().flatten())
        .and_then(|style| style.get_property_value(name).ok())
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| fallback.to_owned())
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "a canvas size is a small positive number of device pixels"
)]
fn device_px(v: f64) -> u32 {
    v.max(1.0).round() as u32
}

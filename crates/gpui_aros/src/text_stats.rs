// SPDX-License-Identifier: MIT
// Copyright (c) 2026 John Knipper

//! A timing proxy around the text system: every call is counted and timed
//! per method, and the totals go out with the frame statistics. GPUI's own
//! draw time is layout, text and paint together; this says how much of it
//! is text shaping and glyph rasterization.

use std::borrow::Cow;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use gpui::{
    Bounds, DevicePixels, Font, FontId, FontMetrics, FontRun, GlyphId, Hsla, LineLayout,
    MissingGlyphSink, Pixels, PlatformTextSystem, RenderGlyphParams, Size, TextRenderingMode,
};

pub(crate) const METHODS: [&str; 6] = [
    "layout_line",
    "rasterize_glyph",
    "glyph_raster_bounds",
    "advance",
    "typographic_bounds",
    "font_metrics",
];

#[derive(Default, Clone, Copy)]
pub(crate) struct Counter {
    pub count: u32,
    pub total: Duration,
}

static TOTALS: Mutex<[Counter; 6]> = Mutex::new([Counter { count: 0, total: Duration::ZERO }; 6]);

fn note(method: usize, d: Duration) {
    if let Ok(mut t) = TOTALS.lock() {
        t[method].count += 1;
        t[method].total += d;
    }
}

/// The counters since the last call, which resets them.
pub(crate) fn take() -> [Counter; 6] {
    match TOTALS.lock() {
        Ok(mut t) => std::mem::take(&mut *t),
        Err(_) => [Counter::default(); 6],
    }
}

pub(crate) struct TimedTextSystem(pub Arc<dyn PlatformTextSystem>);

impl PlatformTextSystem for TimedTextSystem {
    fn add_fonts(&self, fonts: Vec<Cow<'static, [u8]>>) -> Result<()> {
        self.0.add_fonts(fonts)
    }
    fn set_missing_glyph_sink(&self, sink: Option<Arc<dyn MissingGlyphSink>>) {
        self.0.set_missing_glyph_sink(sink)
    }
    fn all_font_names(&self) -> Vec<String> {
        self.0.all_font_names()
    }
    fn font_id(&self, descriptor: &Font) -> Result<FontId> {
        self.0.font_id(descriptor)
    }
    fn prewarm_fonts(&self, font_ids: &[FontId]) {
        self.0.prewarm_fonts(font_ids)
    }
    fn font_metrics(&self, font_id: FontId) -> FontMetrics {
        let t = Instant::now();
        let r = self.0.font_metrics(font_id);
        note(5, t.elapsed());
        r
    }
    fn typographic_bounds(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Bounds<f32>> {
        let t = Instant::now();
        let r = self.0.typographic_bounds(font_id, glyph_id);
        note(4, t.elapsed());
        r
    }
    fn advance(&self, font_id: FontId, glyph_id: GlyphId) -> Result<Size<f32>> {
        let t = Instant::now();
        let r = self.0.advance(font_id, glyph_id);
        note(3, t.elapsed());
        r
    }
    fn glyph_for_char(&self, font_id: FontId, ch: char) -> Option<GlyphId> {
        self.0.glyph_for_char(font_id, ch)
    }
    fn glyph_raster_bounds(&self, params: &RenderGlyphParams) -> Result<Bounds<DevicePixels>> {
        let t = Instant::now();
        let r = self.0.glyph_raster_bounds(params);
        note(2, t.elapsed());
        r
    }
    fn rasterize_glyph(
        &self,
        params: &RenderGlyphParams,
        raster_bounds: Bounds<DevicePixels>,
    ) -> Result<(Size<DevicePixels>, Vec<u8>)> {
        let t = Instant::now();
        let r = self.0.rasterize_glyph(params, raster_bounds);
        note(1, t.elapsed());
        r
    }
    fn layout_line(&self, text: &str, font_size: Pixels, runs: &[FontRun]) -> LineLayout {
        let t = Instant::now();
        let r = self.0.layout_line(text, font_size, runs);
        note(0, t.elapsed());
        r
    }
    fn recommended_rendering_mode(&self, font_id: FontId, font_size: Pixels) -> TextRenderingMode {
        self.0.recommended_rendering_mode(font_id, font_size)
    }
    fn glyph_dilation_for_color(&self, color: Hsla) -> u8 {
        self.0.glyph_dilation_for_color(color)
    }
}

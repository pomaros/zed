// SPDX-License-Identifier: MIT
// Copyright (c) 2026 John Knipper

//! Frame timing, summarized through `log` every few seconds: how long
//! GPUI's own draw takes (layout, prepaint, paint), how long the CPU
//! raster of the damaged region takes, how long the blit into the window
//! takes, and how large the damaged region is. One line answers "where
//! does the time go" on a target where no profiler runs.

use std::cell::RefCell;

use crate::damage::DamageRect;
use std::time::{Duration, Instant};

#[derive(Default)]
struct Sample {
    count: u32,
    total: Duration,
    max: Duration,
}

impl Sample {
    fn add(&mut self, d: Duration) {
        self.count += 1;
        self.total += d;
        if d > self.max {
            self.max = d;
        }
    }

    fn avg_ms(&self) -> f64 {
        if self.count == 0 {
            0.0
        } else {
            self.total.as_secs_f64() * 1000.0 / self.count as f64
        }
    }
}

struct Stats {
    frame: Sample,
    raster: Sample,
    blit: Sample,
    damage_px: u64,
    skipped: u32,
    events: u32,
    kinds: [(&'static str, Sample); 6],
    widest: DamageRect,
    /// Primitives in the last scene drawn: (quads, monochrome sprites, all).
    scene: (usize, usize, usize),
    last_report: Option<Instant>,
}

impl Default for Stats {
    fn default() -> Self {
        Self {
            frame: Sample::default(),
            raster: Sample::default(),
            blit: Sample::default(),
            damage_px: 0,
            skipped: 0,
            events: 0,
            kinds: [
                ("Shadows", Sample::default()),
                ("Quads", Sample::default()),
                ("Paths", Sample::default()),
                ("Underlines", Sample::default()),
                ("MonochromeSprites", Sample::default()),
                ("PolychromeSprites", Sample::default()),
            ],
            widest: DamageRect::EMPTY,
            scene: (0, 0, 0),
            last_report: None,
        }
    }
}

thread_local! {
    static STATS: RefCell<Stats> = RefCell::new(Stats::default());
}

const REPORT_EVERY: Duration = Duration::from_secs(2);

/// GPUI's request-frame callback ran for `d` (it draws only when dirty).
pub(crate) fn frame(d: Duration) {
    STATS.with(|s| s.borrow_mut().frame.add(d));
    report();
}

/// The scene was identical to the previous one: nothing rasterized.
pub(crate) fn skipped() {
    STATS.with(|s| s.borrow_mut().skipped += 1);
}

/// One damaged region rasterized (`raster`) and blitted (`blit`).
pub(crate) fn painted(raster: Duration, blit: Duration, damage: DamageRect) {
    STATS.with(|s| {
        let mut s = s.borrow_mut();
        s.raster.add(raster);
        s.blit.add(blit);
        let px = ((damage.x1 - damage.x0) as u64) * ((damage.y1 - damage.y0) as u64);
        s.damage_px += px;
        let widest = &s.widest;
        if widest.is_empty()
            || px > ((widest.x1 - widest.x0) as u64) * ((widest.y1 - widest.y0) as u64)
        {
            s.widest = damage;
        }
    });
}

/// The size of the scene GPUI handed over (whether or not it changed).
pub(crate) fn scene(quads: usize, mono: usize, all: usize) {
    STATS.with(|s| s.borrow_mut().scene = (quads, mono, all));
}

/// One primitive of `kind` rasterized in `d`.
pub(crate) fn kind(kind: &'static str, d: Duration) {
    STATS.with(|s| {
        let mut s = s.borrow_mut();
        if let Some(entry) = s.kinds.iter_mut().find(|(k, _)| *k == kind) {
            entry.1.add(d);
        }
    });
}

/// One input event dispatched to GPUI.
pub(crate) fn event() {
    STATS.with(|s| s.borrow_mut().events += 1);
}

fn report() {
    STATS.with(|s| {
        let mut s = s.borrow_mut();
        let now = Instant::now();
        let due = match s.last_report {
            None => {
                s.last_report = Some(now);
                false
            }
            Some(t) => now.duration_since(t) >= REPORT_EVERY,
        };
        if !due || s.frame.count == 0 {
            return;
        }
        let kinds: Vec<String> = s
            .kinds
            .iter()
            .filter(|(_, v)| v.count > 0)
            .map(|(k, v)| format!("{k}={}x{:.1}ms", v.count, v.total.as_secs_f64() * 1000.0))
            .collect();
        let text = crate::text_stats::take();
        let text: Vec<String> = crate::text_stats::METHODS
            .iter()
            .zip(text.iter())
            .filter(|(_, c)| c.count > 0)
            .map(|(m, c)| format!("{m}={}x{:.1}ms", c.count, c.total.as_secs_f64() * 1000.0))
            .collect();
        log::info!(
            "gpui_aros frames={} draw avg={:.1}ms max={:.1}ms | painted={} raster avg={:.1}ms max={:.1}ms blit avg={:.1}ms max={:.1}ms px/paint={} widest={:?} kinds[{}] | scene quads={} glyphs={} all={} | text[{}] | identical={} events={}",
            s.frame.count,
            s.frame.avg_ms(),
            s.frame.max.as_secs_f64() * 1000.0,
            s.raster.count,
            s.raster.avg_ms(),
            s.raster.max.as_secs_f64() * 1000.0,
            s.blit.avg_ms(),
            s.blit.max.as_secs_f64() * 1000.0,
            if s.raster.count == 0 { 0 } else { s.damage_px / s.raster.count as u64 },
            s.widest,
            kinds.join(" "),
            s.scene.0,
            s.scene.1,
            s.scene.2,
            text.join(" "),
            s.skipped,
            s.events,
        );
        *s = Stats { last_report: Some(now), ..Default::default() };
    });
}

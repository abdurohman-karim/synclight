//! Maps screen regions along the border to runs of LEDs on the strip.
//!
//! Two modes:
//! * grid (as in SyncLight): the screen is split into a grid (16x9 for 16:9) and each
//!   border cell drives a run of 1-2 LEDs;
//! * per-LED: every LED gets its own strip of screen along the edge (same depth as a
//!   grid cell), so colour changes move along the strip LED by LED.
//!
//! The strip runs either clockwise from the bottom-left corner (`reverse = false`) or
//! counter-clockwise from the bottom-right corner (`reverse = true`, SyncLight
//! "installationMethod = 1").

use anyhow::{bail, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    Left,
    Top,
    Right,
    Bottom,
}

/// Screen region in normalised coordinates (0..1, origin top-left).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
}

/// Screen region in pixels of the downscaled frame (end-exclusive, never empty).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PixRect {
    pub x0: usize,
    pub y0: usize,
    pub x1: usize,
    pub y1: usize,
}

impl Rect {
    pub fn to_pixels(&self, w: usize, h: usize) -> PixRect {
        let span = |a: f32, b: f32, n: usize| {
            let lo = ((a * n as f32).round() as usize).min(n - 1);
            let hi = ((b * n as f32).round() as usize).clamp(lo + 1, n);
            (lo, hi)
        };
        let (x0, x1) = span(self.x0, self.x1, w);
        let (y0, y1) = span(self.y0, self.y1, h);
        PixRect { x0, y0, x1, y1 }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Segment {
    /// First and last LED (1-based, inclusive).
    pub start: u8,
    pub end: u8,
    pub edge: Edge,
    pub rect: Rect,
    /// Next band inward, used when the outer band is black (letterboxed video).
    pub alt: Option<Rect>,
}

pub struct LayoutConfig {
    pub cols: usize,
    pub rows: usize,
    pub horizontal: usize,
    pub vertical: usize,
    pub edges: u8,
    pub reverse: bool,
    pub per_led: bool,
}

pub struct Layout {
    pub edges: u8,
    pub per_led: bool,
    pub segments: Vec<Segment>,
    pub total_leds: usize,
}

/// Grid used by SyncLight (`getAspectRatio` + `getColsAndRows`).
pub fn grid(device_id: &str, lamps: usize, width: u32, height: u32) -> (usize, usize) {
    let ratio = ((width as f64 / height as f64) * 10.0).floor() / 10.0;
    let f = |x: f64| (x * 10.0).floor() / 10.0;
    let (cols, rows) = if ratio == f(16.0 / 9.0) {
        (16, 9)
    } else if ratio == 1.6 {
        (8, 5)
    } else if ratio == f(1512.0 / 982.0) {
        (8, 5)
    } else if ratio == f(4.0 / 3.0) {
        (4, 3)
    } else if ratio == 1.5 {
        (6, 4)
    } else if ratio == 1.2 {
        (10, 8)
    } else if ratio == f(5.0 / 3.0) {
        (10, 6)
    } else if ratio == f(21.0 / 9.0) || ratio == f(32.0 / 9.0) {
        (16, 9)
    } else {
        (6, 4)
    };
    let small_models = ["000100", "000101", "000102", "000103", "001c00"];
    if device_id.len() == 8 || device_id == "888888" || device_id == "000608" {
        (cols, rows)
    } else if device_id == "001102" {
        (16, 9)
    } else if small_models.contains(&device_id) {
        (6, 5)
    } else if lamps >= 2 * (cols + rows) {
        (cols, rows)
    } else if lamps >= 22 {
        (6, 5)
    } else {
        (3, 2)
    }
}

/// LEDs on a horizontal edge and on each vertical edge.
/// Uses SyncLight's per-model table where known, otherwise splits by aspect ratio.
pub fn edge_lamps(device_id: &str, display_size: u8, lamps: usize, edges: u8, w: u32, h: u32) -> (usize, usize) {
    if device_id == "000501" || device_id == "000503" {
        let table = [(24u8, (26, 14)), (27, (31, 17)), (32, (37, 20)), (34, (41, 15))];
        let hit = table.iter().find(|(s, _)| *s == display_size).or(table.first());
        return hit.unwrap().1;
    }
    let x = w as f64 / h as f64;
    let n = lamps as f64;
    let hz = if edges == 3 { n * x / (x + 2.0) } else { n * x / (2.0 * (1.0 + x)) };
    let vt = (hz / x).floor() as usize;
    let hz = if edges == 3 { lamps - 2 * vt } else { (lamps - 2 * vt) / 2 };
    (hz, vt)
}

impl Layout {
    pub fn new(cfg: &LayoutConfig) -> Result<Layout> {
        let (h, v) = (cfg.horizontal, cfg.vertical);
        // (edge, runs in increasing coordinate?, LEDs on the edge, grid cells on the edge)
        // "increasing" = left->right for top/bottom, top->bottom for left/right.
        let mut path = if !cfg.reverse {
            vec![(Edge::Left, false, v, cfg.rows), (Edge::Top, true, h, cfg.cols), (Edge::Right, true, v, cfg.rows)]
        } else {
            vec![(Edge::Right, false, v, cfg.rows), (Edge::Top, false, h, cfg.cols), (Edge::Left, true, v, cfg.rows)]
        };
        if cfg.edges == 4 {
            path.push((Edge::Bottom, cfg.reverse, h, cfg.cols));
        }

        // Band depth: one grid cell, as in the original.
        let dh = 1.0 / cfg.rows as f32;
        let dv = 1.0 / cfg.cols as f32;
        let inner = cfg.rows > 2;

        let mut segments = Vec::new();
        let mut led = 1usize;
        for (edge, forward, n, cells) in path {
            let k = if cfg.per_led { n } else { cells };
            for j in 0..k {
                // Grid mode: even split of n LEDs over k cells; every LED covered once.
                let count = if cfg.per_led { 1 } else { (j + 1) * n / k - j * n / k };
                if count == 0 {
                    continue;
                }
                let a = if forward { j } else { k - 1 - j };
                let (t0, t1) = (a as f32 / k as f32, (a + 1) as f32 / k as f32);
                let (rect, alt) = match edge {
                    Edge::Top => (
                        Rect { x0: t0, y0: 0.0, x1: t1, y1: dh },
                        inner.then_some(Rect { x0: t0, y0: dh, x1: t1, y1: 2.0 * dh }),
                    ),
                    Edge::Bottom => (
                        Rect { x0: t0, y0: 1.0 - dh, x1: t1, y1: 1.0 },
                        inner.then_some(Rect { x0: t0, y0: 1.0 - 2.0 * dh, x1: t1, y1: 1.0 - dh }),
                    ),
                    Edge::Left => (Rect { x0: 0.0, y0: t0, x1: dv, y1: t1 }, None),
                    Edge::Right => (Rect { x0: 1.0 - dv, y0: t0, x1: 1.0, y1: t1 }, None),
                };
                let end = led + count - 1;
                if end > 254 {
                    bail!("слишком много светодиодов ({end}), максимум 254");
                }
                segments.push(Segment { start: led as u8, end: end as u8, edge, rect, alt });
                led += count;
            }
        }
        Ok(Layout { edges: cfg.edges, per_led: cfg.per_led, segments, total_leds: led - 1 })
    }
}

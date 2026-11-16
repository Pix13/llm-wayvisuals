//! Effects: giant gradient digits with mirror reflection, matrix rain, waveform.

use ab_glyph::{point, Font, FontRef, PxScale, ScaleFont};
use super::canvas::{Canvas, Cov, CovRow};
use super::draw::{self, rgb255, Color, PathBuilder};
use crate::theme::{alpha, mix, Theme};

/// Coverage bitmap of a line of digits. Rows `0..base` are above the baseline.
#[derive(Clone)]
pub struct Mask {
    pub w: usize,
    pub h: usize,
    pub base: usize,
    pub data: Vec<u8>,
}

impl Mask {
    pub fn cov(&self) -> Cov<'_> {
        Cov { w: self.w, h: self.h, data: &self.data }
    }
}

/// Metrics of the digit font, relative to the font size.
#[derive(Clone, Copy)]
pub struct DigitMetrics {
    /// Height of "0" / size.
    pub cap: f32,
    /// Widest digit advance / size.
    pub cell: f32,
}

pub fn digit_metrics(font: &FontRef) -> DigitMetrics {
    let sc = font.as_scaled(PxScale::from(100.0));
    let mut cap = 0.0f32;
    let mut cell = 0.0f32;
    for ch in '0'..='9' {
        let id = sc.glyph_id(ch);
        cell = cell.max(sc.h_advance(id));
        if let Some(og) = font.outline_glyph(id.with_scale_and_position(PxScale::from(100.0), point(0.0, 0.0))) {
            cap = cap.max(og.px_bounds().height());
        }
    }
    DigitMetrics {
        cap: cap / 100.0,
        cell: cell / 100.0,
    }
}

/// Lays `s` out in fixed cells (digits and '-' share `cell`, '.' gets `dot`),
/// the first `ghost` characters at 26 % intensity.
pub fn digits_mask(font: &FontRef, s: &str, size: f32, cap_px: f32, cell: f32, dot: f32, ghost: usize) -> Mask {
    let scale = PxScale::from(size);
    let sc = font.as_scaled(scale);
    let base = cap_px.ceil() as usize + 2;
    let h = base + (size * 0.12).ceil() as usize;
    let total: f32 = s.chars().map(|c| if c == '.' { dot } else { cell }).sum();
    let w = total.ceil() as usize + 2;
    let mut data = vec![0u8; w * h];
    let mut x = 1.0f32;
    for (i, ch) in s.chars().enumerate() {
        let cw = if ch == '.' { dot } else { cell };
        let id = sc.glyph_id(ch);
        let adv = sc.h_advance(id);
        let gx = x + (cw - adv) / 2.0;
        let g = id.with_scale_and_position(scale, point(gx, base as f32));
        let k = if i < ghost { 0.26 } else { 1.0 };
        if let Some(og) = font.outline_glyph(g) {
            let b = og.px_bounds();
            og.draw(|px, py, cov| {
                let (xx, yy) = (b.min.x as i32 + px as i32, b.min.y as i32 + py as i32);
                if xx >= 0 && yy >= 0 && (xx as usize) < w && (yy as usize) < h {
                    let o = &mut data[yy as usize * w + xx as usize];
                    *o = (*o).max((cov * k * 255.0) as u8);
                }
            });
        }
        x += cw;
    }
    Mask { w, h, base, data }
}

/// Draws the mask (top-left at x,y) with a vertical gradient, a cheap
/// 4-tap glow and a squashed mirror reflection below the baseline.
#[allow(clippy::too_many_arguments)]
pub fn draw_digits(
    pm: &mut dyn Canvas,
    m: &Mask,
    x: i32,
    y: i32,
    top: Color,
    bottom: Color,
    glow: Color,
    refl_h: usize,
    k: f32,
) {
    let cov = m.cov();
    let grad = |row: usize| {
        let t = (row as f32 / m.base as f32).clamp(0.0, 1.0);
        rgb255(mix(top, bottom, t))
    };
    // glow
    let g = rgb255(glow);
    let o = (3.0 * pm.view().0).round().max(1.0) as i32;
    let o2 = (o * 2 / 3).max(1);
    for (dx, dy) in [(-o, 0), (o, 0), (0, -o), (0, o), (-o2, o2), (o2, o2)] {
        pm.blit_cov(&cov, x + dx, y + dy, g, 0.07 * k, None);
    }
    let rows: Vec<CovRow> = (0..m.h).map(|row| CovRow { src_row: row, dst_y: y + row as i32, rgb: grad(row), a: k }).collect();
    pm.blit_cov_rows(&cov, x, &rows);

    // reflection: row r below the baseline samples the mask at base - r/squash
    let squash = 0.42f32;
    let by = y + m.base as i32;
    let mut rows = Vec::with_capacity(refl_h);
    for r in 0..refl_h {
        let py = by + 1 + r as i32;
        let src = m.base as f32 - 1.0 - r as f32 / squash;
        if src < 0.0 {
            break;
        }
        let srow = src as usize;
        let fade = (1.0 - r as f32 / refl_h as f32).powf(1.6);
        let scan = if r % 3 == 2 { 0.45 } else { 1.0 };
        let a = 0.5 * fade * scan * k;
        let t = (srow as f32 / m.base as f32).clamp(0.0, 1.0);
        rows.push(CovRow { src_row: srow, dst_y: py, rgb: rgb255(mix(top, bottom, t)), a });
    }
    pm.blit_cov_rows(&cov, x, &rows);
}

// ---- matrix rain ---------------------------------------------------------

struct RainGlyph {
    w: usize,
    h: usize,
    ox: i32,
    oy: i32,
    cov: Vec<u8>,
}

pub struct Rain {
    font: FontRef<'static>,
    scale: f32,
    glyphs: Vec<RainGlyph>,
}

const RAIN_CHARS: &str = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";
pub const RAIN_CELL: f32 = 17.0;
const RAIN_COL: f32 = 22.0;

fn hash(a: u32, b: u32) -> u32 {
    let mut h = a.wrapping_mul(0x9E37_79B1) ^ b.wrapping_mul(0x85EB_CA6B);
    h ^= h >> 15;
    h = h.wrapping_mul(0x2C1B_3C6D);
    h ^= h >> 12;
    h = h.wrapping_mul(0x297A_2D39);
    h ^ (h >> 15)
}

fn unit(h: u32) -> f32 {
    (h & 0xFFFF) as f32 / 65535.0
}

impl Rain {
    pub fn new(font: &FontRef<'static>) -> Self {
        let mut r = Rain { font: font.clone(), scale: 0.0, glyphs: Vec::new() };
        r.ensure(1.0);
        r
    }

    /// Rebuilds the glyph cache for pixel scale `s`.
    fn ensure(&mut self, s: f32) {
        if (self.scale - s).abs() < 1e-4 {
            return;
        }
        self.scale = s;
        let font = &self.font.clone();
        let size = 14.0 * s;
        let scale = PxScale::from(size);
        let sc = font.as_scaled(scale);
        self.glyphs = RAIN_CHARS
            .chars()
            .map(|ch| {
                let id = sc.glyph_id(ch);
                let g = id.with_scale_and_position(scale, point(0.0, 0.0));
                if let Some(og) = font.outline_glyph(g) {
                    let b = og.px_bounds();
                    let (w, h) = (b.width().ceil() as usize + 1, b.height().ceil() as usize + 1);
                    let mut cov = vec![0u8; w * h];
                    og.draw(|x, y, c| {
                        let i = y as usize * w + x as usize;
                        if i < cov.len() {
                            cov[i] = (c * 255.0) as u8;
                        }
                    });
                    RainGlyph { w, h, ox: b.min.x as i32, oy: b.min.y as i32, cov }
                } else {
                    RainGlyph { w: 0, h: 0, ox: 0, oy: 0, cov: vec![] }
                }
            })
            .collect();
    }

    fn put(&self, pm: &mut dyn Canvas, idx: usize, cx: f32, base_y: f32, clip: (i32, i32, i32, i32), rgb: [f32; 3], a: f32) {
        let g = &self.glyphs[idx % self.glyphs.len()];
        let (cx, base_y) = (draw::tx(pm, cx), draw::ty(pm, base_y));
        let gx = cx.round() as i32 + g.ox - (g.w as i32) / 2 - g.ox;
        let gy = base_y.round() as i32 + g.oy;
        pm.blit_cov(&Cov { w: g.w, h: g.h, data: &g.cov }, gx, gy, rgb, a, Some(clip));
    }

    /// Deterministic in `t`: falling columns of characters, brighter heads.
    pub fn draw(&mut self, pm: &mut dyn Canvas, th: &Theme, x: f32, y: f32, w: f32, h: f32, t: f32, k: f32) {
        self.ensure(pm.view().0);
        let clip = (draw::tx(pm, x) as i32, draw::ty(pm, y) as i32, draw::tx(pm, x + w) as i32, draw::ty(pm, y + h) as i32);
        let body = rgb255(th.rain);
        let head = rgb255(mix(th.rain, Color::WHITE, 0.6));
        let cols = ((w - 12.0) / RAIN_COL) as u32;
        for c in 0..cols {
            let hh = hash(c, 77);
            // sparse: about 2 out of 3 columns are active
            if hh % 3 == 0 {
                continue;
            }
            let speed = 26.0 + 60.0 * unit(hh >> 3);
            let len = 5 + (unit(hh >> 7) * 13.0) as u32;
            let travel = h + len as f32 * RAIN_CELL;
            let off = unit(hash(c, 5)) * travel;
            let pos = t * speed + off;
            let head_y = pos % travel;
            let row0 = (pos / RAIN_CELL) as i64;
            let cx = x + 12.0 + c as f32 * RAIN_COL;
            for i in 0..len {
                let yy = y + head_y - i as f32 * RAIN_CELL;
                if yy < y - 2.0 || yy > y + h + RAIN_CELL {
                    continue;
                }
                let k_row = (row0 - i as i64) as u32;
                let idx = hash(c, k_row.wrapping_add(((t * 1.5) as u32) * (i == 0) as u32)) as usize;
                let (rgb, a) = if i == 0 {
                    (head, 0.50)
                } else {
                    let f = 1.0 - i as f32 / len as f32;
                    (body, 0.10 + 0.32 * f.powf(1.3))
                };
                self.put(pm, idx, cx, yy, clip, rgb, a * k);
            }
        }
    }
}

// ---- waveform -----------------------------------------------------------

/// `wave`: (age 0..1, 0 = now at the right edge; value), None = gap.
#[allow(clippy::too_many_arguments)]
pub fn draw_wave(pm: &mut dyn Canvas, th: &Theme, wave: &[(f32, Option<f32>)], max: Option<f32>, x: f32, y: f32, w: f32, h: f32, k: f32) {
    // baseline
    draw::line(pm, x, y + h, x + w, y + h, alpha(th.accent, 0.25 * k), 1.0);
    draw::line(pm, x, y, x + w, y, alpha(th.accent, 0.08 * k), 1.0);
    let vmax = max
        .or_else(|| wave.iter().filter_map(|p| p.1).fold(None, |a: Option<f32>, v| Some(a.map_or(v, |a| a.max(v)))))
        .filter(|m| *m > 0.0)
        .unwrap_or(1.0);
    let mut pts: Vec<(f32, Option<f32>)> = wave
        .iter()
        .map(|&(age, v)| (age.clamp(0.0, 1.0), v))
        .collect();
    pts.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal)); // oldest first
    let mut pb = PathBuilder::new();
    let mut fill = PathBuilder::new();
    let mut pen = false;
    let mut last: Option<(f32, f32)> = None;
    let mut first_x = 0.0;
    let close_fill = |fill: &mut PathBuilder, last: Option<(f32, f32)>, fx: f32| {
        if let Some((lx, _)) = last {
            fill.line_to(lx, y + h);
            fill.line_to(fx, y + h);
            fill.close();
        }
    };
    for (age, v) in pts {
        match v {
            Some(v) => {
                let px = x + w * (1.0 - age);
                let py = y + h - (v / vmax).clamp(0.0, 1.0) * h;
                if pen {
                    pb.line_to(px, py);
                    fill.line_to(px, py);
                } else {
                    pb.move_to(px, py);
                    fill.move_to(px, y + h);
                    fill.line_to(px, py);
                    first_x = px;
                    pen = true;
                }
                last = Some((px, py));
            }
            None => {
                if pen {
                    close_fill(&mut fill, last, first_x);
                }
                pen = false;
                last = None;
            }
        }
    }
    if pen {
        close_fill(&mut fill, last, first_x);
    }
    if let Some(p) = fill.finish() {
        draw::fill(pm, &p, alpha(th.accent, 0.10 * k));
    }
    if let Some(p) = pb.finish() {
        draw::glow_stroke(pm, &p, alpha(th.bright, 0.95 * k), 1.5, 5.0, 0.07 * k);
    }
    if let Some((lx, ly)) = last {
        draw::fill(pm, &draw::circle_path(lx, ly, 2.6), alpha(th.value, k));
    }
}

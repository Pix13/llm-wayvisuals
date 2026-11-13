//! Layout, static layer (frames, labels, gauge ticks, icons) and the
//! dynamic cards. Everything is laid out for a design height `hd`
//! (360..=540); horizontal positions stay at the 1920 px design X, vertical
//! positions and all sizes scale with the card height factor `u`, so that
//! circles and glyphs are never squashed.

use tiny_skia::{Color, PathBuilder, Pixmap};

use super::draw::*;
use super::fmt;
use crate::model::{Gpu, Model, Phase};
use crate::theme::{alpha, mix, Theme};

pub const MX: f32 = 12.0;
pub const GAP: f32 = 18.0;
pub const TOP_W: f32 = 620.0;
pub const BOT_W: f32 = 301.0;
const VGAP: f32 = 16.0;

#[derive(Clone, Copy)]
pub struct R {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Vertical layout for a design height.
#[derive(Clone, Copy)]
pub struct Lay {
    /// Design height.
    pub hd: f32,
    pub top_h: f32,
    pub bot_y: f32,
    pub bot_h: f32,
    /// Scale factor of top-row content (1.0 at hd = 480).
    pub ut: f32,
    /// Scale factor of bottom-row content.
    pub ub: f32,
}

impl Lay {
    pub fn new(hd: f32) -> Self {
        let avail = hd - 2.0 * MX - VGAP;
        let top_h = (avail * 280.0 / 440.0).round();
        let bot_h = avail - top_h;
        Lay { hd, top_h, bot_y: MX + top_h + VGAP, bot_h, ut: top_h / 280.0, ub: bot_h / 160.0 }
    }
    pub fn top(&self, i: usize) -> R {
        R { x: MX + i as f32 * (TOP_W + GAP), y: MX, w: TOP_W, h: self.top_h }
    }
    pub fn bot(&self, i: usize) -> R {
        R { x: MX + i as f32 * (BOT_W + GAP), y: self.bot_y, w: BOT_W, h: self.bot_h }
    }
}

/// Scaled font size with a legibility floor.
pub fn px(size: f32, u: f32, min: f32) -> f32 {
    (size * u).max(min)
}

const GAUGE_N: usize = 36;
const GAUGE_A0: f32 = 135.0;
const GAUGE_SWEEP: f32 = 270.0;
const RING_N: usize = 24;
const BAND_R0: f32 = 85.0;
const BAND_R1: f32 = 106.0;

fn band_seg(i: usize) -> (f32, f32) {
    let step = GAUGE_SWEEP / GAUGE_N as f32;
    let a = GAUGE_A0 + i as f32 * step;
    (a + 0.9, a + step - 0.9)
}

#[derive(Clone, Copy)]
pub struct Gauge {
    pub cx: f32,
    pub cy: f32,
    /// Uniform radius scale.
    pub k: f32,
    pub card_x: f32,
}

pub fn gauge_of(card: R, right: bool, u: f32) -> Gauge {
    let cx = if right { 330.0f32.max(218.0 + 108.0 * u) } else { 300.0 };
    Gauge { cx: card.x + cx, cy: card.y + 160.0 * u, k: u, card_x: card.x }
}

/// Side value boxes: `right` places them at the right edge of the card.
pub fn box_rect(card: R, right: bool, i: usize, u: f32) -> R {
    R {
        x: card.x + if right { 498.0 } else { 14.0 },
        y: card.y + (60.0 + i as f32 * 72.0) * u,
        w: 108.0,
        h: 56.0 * u,
    }
}

pub fn bubble_rect(g: Gauge, i: usize) -> R {
    let u = g.k;
    R { x: g.cx - 108.0 * u - 80.0, y: g.cy + (-66.0 + i as f32 * 66.0 - 21.0) * u, w: 80.0, h: 42.0 * u }
}

fn usage_rect(g: Gauge, right: bool) -> R {
    let u = g.k;
    R {
        // right card: never run into the side value boxes (card.x + 498)
        x: if right { (g.cx + 98.0 * u).min(g.card_x + 498.0 - 62.0) } else { g.cx - 98.0 * u - 56.0 },
        y: g.cy - 100.0 * u,
        w: 56.0,
        h: (24.0 * u).max(18.0),
    }
}

/// Info column of the right card (x, width).
fn right_info(card: R, g: Gauge) -> (f32, f32) {
    let x = card.x + 14.0;
    let bx = bubble_rect(g, 0).x;
    (x, (bx - 12.0 - x).clamp(100.0, 130.0))
}

// ---- cards ----------------------------------------------------------------

fn card_frame(pm: &mut Pixmap, th: &Theme, r: R, u: f32) {
    let cut = 14.0 * u.clamp(0.8, 1.1);
    let body = chamfer_path(r.x, r.y, r.w, r.h, cut);
    fill(pm, &body, alpha(th.accent, 0.035));
    stroke(pm, &body, alpha(th.accent, 0.42), 1.3);
    let inner = chamfer_path(r.x + 4.0, r.y + 4.0, r.w - 8.0, r.h - 8.0, cut - 3.0);
    stroke(pm, &inner, alpha(th.accent, 0.12), 1.0);
    // bright corner accents
    let a = 24.0;
    let (x0, y0, x1, y1) = (r.x, r.y, r.x + r.w, r.y + r.h);
    let mut pb = PathBuilder::new();
    let corners = [
        [(x0, y0 + cut + a), (x0, y0 + cut), (x0 + cut, y0), (x0 + cut + a, y0)],
        [(x1 - cut - a, y0), (x1 - cut, y0), (x1, y0 + cut), (x1, y0 + cut + a)],
        [(x1, y1 - cut - a), (x1, y1 - cut), (x1 - cut, y1), (x1 - cut - a, y1)],
        [(x0 + cut + a, y1), (x0 + cut, y1), (x0, y1 - cut), (x0, y1 - cut - a)],
    ];
    for c in corners {
        pb.move_to(c[0].0, c[0].1);
        for p in &c[1..] {
            pb.line_to(p.0, p.1);
        }
    }
    let p = pb.finish().unwrap();
    glow_stroke(pm, &p, alpha(th.bright, 0.9), 2.0, 5.0, 0.07);
}

fn title_line(pm: &mut Pixmap, th: &Theme, r: R, u: f32) {
    line(pm, r.x + 10.0, r.y + 41.0 * u, r.x + r.w - 10.0, r.y + 41.0 * u, alpha(th.accent, 0.45), 1.0);
}

fn card_title(pm: &mut Pixmap, th: &Theme, f: &Fonts, r: R, t: &str, u: f32) {
    text(pm, &f.semi, px(23.0, u, 15.0), r.x + 18.0, r.y + 31.0 * u, t, th.text, 0.8, Align::Left);
    title_line(pm, th, r, u);
}

pub fn draw_centre_title(pm: &mut Pixmap, th: &Theme, f: &Fonts, lay: &Lay, title: &str, right: Option<&str>) {
    let r = lay.top(1);
    let sz = px(23.0, lay.ut, 15.0);
    let mut avail = r.w - 40.0;
    if let Some(rt) = right {
        let rs = px(13.0, lay.ut, 10.0);
        let w = text_width(&f.label, rs, rt, 1.0);
        text(pm, &f.label, rs, r.x + r.w - 18.0, r.y + 31.0 * lay.ut, rt, th.dim, 1.0, Align::Right);
        avail -= w + 24.0;
    }
    let t = clip_text(&f.semi, sz, &cap(title).to_uppercase(), avail, 0.8);
    text(pm, &f.semi, sz, r.x + 18.0, r.y + 31.0 * lay.ut, &t, th.text, 0.8, Align::Left);
}

// ---- icons ------------------------------------------------------------------

fn icon(pm: &mut Pixmap, kind: usize, cx: f32, cy: f32, c: Color) {
    let w = 1.7;
    match kind {
        0 => {
            stroke(pm, &rrect_path(cx - 7.0, cy - 7.0, 14.0, 14.0, 2.0), c, w);
            rect_aa(pm, cx - 3.0, cy - 3.0, 6.0, 6.0, c);
            for i in 0..3 {
                let o = -4.0 + i as f32 * 4.0;
                line(pm, cx + o, cy - 11.0, cx + o, cy - 7.0, c, 1.4);
                line(pm, cx + o, cy + 7.0, cx + o, cy + 11.0, c, 1.4);
                line(pm, cx - 11.0, cy + o, cx - 7.0, cy + o, c, 1.4);
                line(pm, cx + 7.0, cy + o, cx + 11.0, cy + o, c, 1.4);
            }
        }
        1 => {
            stroke(pm, &rrect_path(cx - 11.0, cy - 7.0, 22.0, 14.0, 2.5), c, w);
            line(pm, cx - 11.0, cy + 1.0, cx + 11.0, cy + 1.0, c, 1.4);
            fill(pm, &circle_path(cx + 7.0, cy + 4.5, 1.3), c);
        }
        2 => fan(pm, cx, cy, 10.0, c),
        3 => {
            stroke(pm, &rrect_path(cx - 3.0, cy - 11.0, 6.0, 16.0, 3.0), c, w);
            fill(pm, &circle_path(cx, cy + 7.0, 5.0), c);
            line(pm, cx, cy - 4.0, cx, cy + 6.0, c, 2.0);
        }
        4 => {
            let mut pb = PathBuilder::new();
            pb.move_to(cx + 3.0, cy - 11.0);
            pb.line_to(cx - 7.0, cy + 2.0);
            pb.line_to(cx - 1.0, cy + 2.0);
            pb.line_to(cx - 4.0, cy + 11.0);
            pb.line_to(cx + 7.0, cy - 3.0);
            pb.line_to(cx + 1.0, cy - 3.0);
            pb.close();
            fill(pm, &pb.finish().unwrap(), c);
        }
        _ => {
            for r in [5.0, 9.0, 13.0] {
                stroke(pm, &arc_path(cx, cy + 7.0, r, 225.0, 315.0), c, w);
            }
            fill(pm, &circle_path(cx, cy + 7.0, 1.8), c);
        }
    }
}

fn fan(pm: &mut Pixmap, cx: f32, cy: f32, r: f32, c: Color) {
    fan_rot(pm, cx, cy, r, c, 0.0);
}

pub fn fan_rot(pm: &mut Pixmap, cx: f32, cy: f32, r: f32, c: Color, rot: f32) {
    stroke(pm, &circle_path(cx, cy, r), c, (r * 0.12).max(1.4));
    for k in 0..4 {
        let a = k as f32 * 90.0 + 20.0 + rot;
        let (x1, y1) = polar(cx, cy, r * 0.92, a);
        let (x2, y2) = polar(cx, cy, r * 0.55, a + 38.0);
        let mut pb = PathBuilder::new();
        pb.move_to(cx, cy);
        pb.quad_to(x1, y1, x2, y2);
        pb.close();
        fill(pm, &pb.finish().unwrap(), alpha(c, 0.85));
    }
    fill(pm, &circle_path(cx, cy, r * 0.18), c);
}

// ---- gauge ------------------------------------------------------------------

fn gauge_static(pm: &mut Pixmap, th: &Theme, f: &Fonts, g: Gauge, label: &str) {
    let (cx, cy, k) = (g.cx, g.cy, g.k);
    for i in 0..GAUGE_N {
        let (a0, a1) = band_seg(i);
        fill(pm, &sector_path(cx, cy, BAND_R0 * k, BAND_R1 * k, a0, a1), alpha(th.accent, 0.16));
    }
    stroke(pm, &arc_path(cx, cy, 109.5 * k, GAUGE_A0, GAUGE_A0 + GAUGE_SWEEP), alpha(th.accent, 0.35), 1.0);
    for (v, a) in [("0", GAUGE_A0), ("50", GAUGE_A0 + 135.0), ("100", GAUGE_A0 + 270.0)] {
        let (x, y) = polar(cx, cy, 78.0 * k, a);
        text(pm, &f.label, 10.0, x, y + 3.5, v, th.dim, 0.0, Align::Center);
    }
    for i in 0..RING_N {
        let a = i as f32 * 360.0 / RING_N as f32;
        let (x0, y0) = polar(cx, cy, 60.0 * k, a);
        let (x1, y1) = polar(cx, cy, 71.0 * k, a);
        line(pm, x0, y0, x1, y1, alpha(th.accent, 0.20), 7.0 * k);
    }
    fill(pm, &circle_path(cx, cy, 54.0 * k), alpha(th.accent, 0.07));
    stroke(pm, &circle_path(cx, cy, 54.0 * k), th.border, 1.5);
    stroke(pm, &circle_path(cx, cy, 43.0 * k), alpha(th.accent, 0.28), 1.0);
    for q in 0..4 {
        let a = q as f32 * 90.0;
        let (x, y) = polar(cx, cy, 49.0 * k, a);
        let (nx, ny) = polar(0.0, 0.0, 1.0, a);
        let (tx, ty) = (-ny, nx);
        let s = k.max(0.8);
        let mut pb = PathBuilder::new();
        pb.move_to(x + nx * 3.0 * s, y + ny * 3.0 * s);
        pb.line_to(x - nx * 2.0 * s + tx * 3.0 * s, y - ny * 2.0 * s + ty * 3.0 * s);
        pb.line_to(x - nx * 2.0 * s - tx * 3.0 * s, y - ny * 2.0 * s - ty * 3.0 * s);
        pb.close();
        fill(pm, &pb.finish().unwrap(), alpha(th.bright, 0.8));
    }
    let sz = px(38.0, k, 24.0);
    let by = cy + 4.0 * k;
    for (dx, dy) in [(-1.2, 0.0), (1.2, 0.0), (0.0, -1.2), (0.0, 1.2)] {
        text(pm, &f.semi, sz, cx + dx, by + dy, label, alpha(th.accent, 0.30), 1.0, Align::Center);
    }
    text(pm, &f.semi, sz, cx, by, label, th.value, 1.0, Align::Center);
}

fn gauge_dynamic(pm: &mut Pixmap, th: &Theme, f: &Fonts, g: Gauge, frac: Option<f32>, sub: &str, t: f32, live: bool) {
    let (cx, cy, k) = (g.cx, g.cy, g.k);
    if let Some(fr) = frac {
        let n = (fr.clamp(0.0, 1.0) * GAUGE_N as f32).round() as usize;
        let n = if fr > 0.0 { n.max(1) } else { 0 };
        for i in 0..n {
            let (a0, a1) = band_seg(i);
            let p = sector_path(cx, cy, BAND_R0 * k, BAND_R1 * k, a0, a1);
            let kk = i as f32 / GAUGE_N as f32;
            let c = mix(th.accent, th.bright, if i + 1 == n { 0.8 } else { 0.05 + 0.3 * kk });
            stroke(pm, &p, alpha(th.accent, 0.22), 6.0 * k.max(0.8));
            fill(pm, &p, c);
        }
    }
    if live {
        let head = (t * 3.0) as usize % RING_N;
        for (j, a) in [1.0f32, 0.55, 0.28].iter().enumerate() {
            let i = (head + RING_N - j) % RING_N;
            let ang = i as f32 * 360.0 / RING_N as f32;
            let (x0, y0) = polar(cx, cy, 60.0 * k, ang);
            let (x1, y1) = polar(cx, cy, 71.0 * k, ang);
            line(pm, x0, y0, x1, y1, alpha(th.bright, 0.8 * a), 7.0 * k);
        }
    }
    text(pm, &f.label, px(13.0, k, 10.0), cx, cy + 26.0 * k, sub, th.dim, 0.5, Align::Center);
}

fn value_box_static(pm: &mut Pixmap, th: &Theme, f: &Fonts, r: R, label: &str, u: f32) {
    let p = chamfer_path(r.x, r.y, r.w, r.h, 6.0 * u.clamp(0.7, 1.0));
    fill(pm, &p, alpha(th.accent, 0.04));
    stroke(pm, &p, alpha(th.accent, 0.50), 1.2);
    text(pm, &f.label, px(12.0, u, 10.0), r.x + 10.0, r.y + r.h * 0.33, label, th.dim, 1.0, Align::Left);
}

fn value_box_dynamic(pm: &mut Pixmap, th: &Theme, f: &Fonts, r: R, v: &str, u: f32) {
    let s = fit_size(&f.semi, px(23.0, u, 15.0), 12.0, v, r.w - 18.0, 0.3);
    text(pm, &f.semi, s, r.x + 10.0, r.y + r.h * 0.80, v, th.value, 0.3, Align::Left);
}

fn info_header(pm: &mut Pixmap, th: &Theme, f: &Fonts, x: f32, y: f32, w: f32, title: &str, u: f32) {
    text(pm, &f.semi, px(14.0, u, 12.0), x + w / 2.0, y + 66.0 * u, title, th.text, 1.2, Align::Center);
    line(pm, x, y + 73.0 * u, x + w, y + 73.0 * u, alpha(th.accent, 0.45), 1.0);
}

#[allow(clippy::too_many_arguments)]
fn info_pair(pm: &mut Pixmap, th: &Theme, f: &Fonts, x: f32, w: f32, y: f32, label: &str, v: &str, c: Color, u: f32) {
    text(pm, &f.label, px(11.0, u, 10.0), x, y, label, th.dim, 1.2, Align::Left);
    let s = fit_size(&f.semi, px(17.0, u, 14.0), 10.0, v, w, 0.2);
    text(pm, &f.semi, s, x, y + 19.0 * u, v, c, 0.2, Align::Left);
}

// ---- static layer -------------------------------------------------------------

pub fn draw_static(pm: &mut Pixmap, th: &Theme, f: &Fonts, lay: &Lay) {
    pm.fill(Color::BLACK);
    let u = lay.ut;
    let (l, c, r) = (lay.top(0), lay.top(1), lay.top(2));
    card_frame(pm, th, l, u);
    card_title(pm, th, f, l, "DATA PATH", u);
    card_frame(pm, th, c, u);
    title_line(pm, th, c, u);
    card_frame(pm, th, r, u);
    card_title(pm, th, f, r, "GPU / GRAPHICS", u);

    let gr = gauge_of(r, true, u);
    gauge_static(pm, th, f, gr, "GPU");
    let ur = usage_rect(gr, true);
    stroke(pm, &rrect_path(ur.x, ur.y, ur.w, ur.h, 3.0), alpha(th.accent, 0.5), 1.2);
    text(pm, &f.label, 10.0, ur.x + ur.w / 2.0, ur.y + ur.h + 11.0, "UTIL", th.dim, 1.0, Align::Center);
    for (i, lb) in ["CLOCK", "POWER", "VRAM"].iter().enumerate() {
        let b = bubble_rect(gr, i);
        stroke(pm, &rrect_path(b.x, b.y, b.w, b.h, (14.0 * u).min(b.h / 2.0)), alpha(th.accent, 0.55), 1.2);
        let my = b.y + b.h / 2.0;
        let dy = my - gr.cy;
        let rr = 106.0 * u;
        let ex = gr.cx - (rr * rr - dy * dy).max(0.0).sqrt();
        line(pm, b.x + b.w, my, ex, my, alpha(th.accent, 0.45), 1.0);
        text(pm, &f.label, 10.0, b.x + 14.0, b.y + b.h * 0.36, lb, th.dim, 1.0, Align::Left);
    }
    for (i, lb) in ["VRAM", "GPU TEMP", "MEM TEMP"].iter().enumerate() {
        value_box_static(pm, th, f, box_rect(r, true, i, u), lb, u);
    }
    let (ix, iw) = right_info(r, gr);
    info_header(pm, th, f, ix, r.y, iw, "GPU INFO", u);

    // bottom row
    let ub = lay.ub;
    let titles = ["MEMORY / VRAM", "CONTEXT", "CPU", "TEMPERATURE", "POWER", "REQUESTS"];
    for (i, t) in titles.iter().enumerate() {
        let b = lay.bot(i);
        card_frame(pm, th, b, ub);
        icon(pm, if i == 2 { 0 } else { i }, b.x + 28.0, b.y + 22.0 * ub.max(0.8), th.bright);
        text(pm, &f.semi, px(21.0, ub, 14.0), b.x + 50.0, b.y + 31.0 * ub, t, th.text, 0.8, Align::Left);
        title_line(pm, th, b, ub);
    }
}

// ---- dynamic ------------------------------------------------------------------

pub struct Data<'a> {
    pub model: Option<&'a Model>,
    pub gpus: Vec<&'a Gpu>,
}

#[allow(clippy::too_many_arguments)]
pub fn seg_bar(pm: &mut Pixmap, th: &Theme, x: f32, y: f32, w: f32, h: f32, n: usize, frac: Option<f32>) {
    let gap = 3.0;
    let bw = (w - gap * (n as f32 - 1.0)) / n as f32;
    let lit = frac.map_or(0, |f| {
        let f = f.clamp(0.0, 1.0);
        if f > 0.0 { ((f * n as f32).round() as usize).max(1) } else { 0 }
    });
    for i in 0..n {
        let bx = x + i as f32 * (bw + gap);
        if i < lit {
            let c = mix(th.accent, th.bright, 0.35 * (i as f32 / n as f32));
            rect(pm, bx, y, bw, h, c);
            rect(pm, bx, y, bw, h * 0.35, alpha(Color::WHITE, 0.10));
        } else {
            rect(pm, bx, y, bw, h, alpha(th.accent, 0.13));
        }
    }
    if lit > 0 {
        let glow_w = (bw + gap) * lit as f32;
        rect(pm, x, y + h, glow_w - gap, 1.0, alpha(th.accent, 0.25));
    }
}

fn frac(v: Option<f64>, max: f64) -> Option<f32> {
    v.filter(|v| v.is_finite()).map(|v| (v / max) as f32)
}

fn sum_opt<I: Iterator<Item = Option<f64>>>(it: I) -> Option<f64> {
    let mut any = false;
    let mut s = 0.0;
    for v in it.flatten() {
        any = true;
        s += v;
    }
    any.then_some(s)
}

pub fn draw_top_dynamic(pm: &mut Pixmap, th: &Theme, f: &Fonts, lay: &Lay, d: &Data, t: f32, live: bool) {
    let u = lay.ut;
    // ---- right: GPU. First GPU of the model prominent, the others as
    // compact lines in the info column.
    let r = lay.top(2);
    let gr = gauge_of(r, true, u);
    let g0 = d.gpus.first().copied();
    let util = g0.and_then(|g| g.util_pct);
    let gname = g0.and_then(|g| g.name.as_deref()).map(fmt::short_gpu_name).unwrap_or_else(|| fmt::NONE.into());
    gauge_dynamic(pm, th, f, gr, util.map(|u| (u / 100.0) as f32), &gname, t, live);
    let ur = usage_rect(gr, true);
    text(pm, &f.semi, px(16.0, u, 14.0), ur.x + ur.w / 2.0, ur.y + ur.h * 0.72, &fmt::pct(util), th.value, 0.2, Align::Center);
    let vram_pct = g0.and_then(|g| match (g.mem_used_mb, g.mem_total_mb) {
        (Some(u), Some(t)) if t > 0 => Some(u as f64 / t as f64 * 100.0),
        _ => None,
    });
    let bub = [
        fmt::mhz(g0.and_then(|g| g.clock_sm_mhz)),
        fmt::watts(g0.and_then(|g| g.power_w)),
        fmt::gb_from_mb(g0.and_then(|g| g.mem_used_mb)),
    ];
    for (i, v) in bub.iter().enumerate() {
        let b = bubble_rect(gr, i);
        let s = fit_size(&f.semi, px(17.0, u, 14.0), 11.0, v, b.w - 22.0, 0.0);
        text(pm, &f.semi, s, b.x + 14.0, b.y + b.h * 0.81, v, th.value, 0.0, Align::Left);
    }
    let vals = [fmt::pct(vram_pct), fmt::temp(g0.and_then(|g| g.temp_c)), fmt::temp(g0.and_then(|g| g.mem_temp_c))];
    for (i, v) in vals.iter().enumerate() {
        value_box_dynamic(pm, th, f, box_rect(r, true, i, u), v, u);
    }
    let (ix, iw) = right_info(r, gr);
    info_pair(pm, th, f, ix, iw, r.y + 97.0 * u, "GPU 0", &gname, th.value, u);
    for (k, g) in d.gpus.iter().skip(1).take(3).enumerate() {
        let y = r.y + (139.0 + k as f32 * 44.0) * u;
        if y + 19.0 * u > r.y + r.h - 12.0 {
            break; // no room: drop the least important compact GPU lines
        }
        let lb = format!("GPU {}", g.index.map_or("?".into(), |i| i.to_string()));
        let v = format!("{} {} {}", fmt::pct(g.util_pct), fmt::watts(g.power_w), fmt::temp(g.temp_c));
        info_pair(pm, th, f, ix, iw, y, &lb, &v, th.text, u);
    }
}

#[allow(clippy::too_many_arguments)]
fn bottom_label_value(pm: &mut Pixmap, th: &Theme, f: &Fonts, b: R, y: f32, label: &str, v: &str, size: f32, u: f32) {
    text(pm, &f.label, px(13.0, u, 10.0), b.x + 14.0, y, label, th.dim, 1.0, Align::Left);
    text(pm, &f.semi, px(size, u, 14.0), b.x + b.w - 14.0, y, v, th.value, 0.2, Align::Right);
}

/// Row baselines of the small-row cards: (first, pitch), bounded below.
fn rows(b: R, u: f32) -> (f32, f32) {
    (b.y + 64.0 * u, (25.0 * u).max(17.5))
}

/// A row fits when its baseline leaves room above the card bottom.
fn row_fits(b: R, y: f32) -> bool {
    y <= b.y + b.h - 9.0
}

#[allow(clippy::too_many_arguments)]
fn bar_row(pm: &mut Pixmap, th: &Theme, f: &Fonts, b: R, y: f32, label: &str, fr: Option<f32>, v: &str, u: f32) {
    text(pm, &f.label, px(14.0, u, 10.0), b.x + 14.0, y, label, th.text, 0.8, Align::Left);
    let bh = (11.0 * u).max(8.0);
    seg_bar(pm, th, b.x + 66.0, y - bh, 142.0, bh, 12, fr);
    text(pm, &f.semi, px(17.0, u, 14.0), b.x + b.w - 14.0, y, v, th.value, 0.2, Align::Right);
}

fn big_bar_card(pm: &mut Pixmap, th: &Theme, f: &Fonts, b: R, label: &str, pct: Option<f64>, fr: Option<f32>, u: f32) {
    text(pm, &f.label, px(14.0, u, 10.0), b.x + 14.0, b.y + 70.0 * u, label, th.text, 1.0, Align::Left);
    text(pm, &f.semi, px(34.0, u, 22.0), b.x + b.w - 14.0, b.y + 74.0 * u, &fmt::pct(pct), th.value, 0.2, Align::Right);
    seg_bar(pm, th, b.x + 14.0, b.y + 82.0 * u, b.w - 28.0, (15.0 * u).max(9.0), 22, fr);
}

pub fn draw_bottom_dynamic(pm: &mut Pixmap, th: &Theme, f: &Fonts, lay: &Lay, d: &Data) {
    let u = lay.ub;
    let m = d.model;
    // Below ub = 0.8 the secondary line of MEMORY (FREE) and CONTEXT
    // (PROMPT / CACHED) is dropped.
    let second = u >= 0.8;
    // MEMORY: summed over the model's GPUs
    let b = lay.bot(0);
    let used = sum_opt(d.gpus.iter().map(|g| g.mem_used_mb.map(|v| v as f64)));
    let total = sum_opt(d.gpus.iter().map(|g| g.mem_total_mb.map(|v| v as f64)));
    let pctv = match (used, total) {
        (Some(u), Some(t)) if t > 0.0 => Some(u / t * 100.0),
        _ => None,
    };
    big_bar_card(pm, th, f, b, "VRAM LOAD", pctv, pctv.map(|p| (p / 100.0) as f32), u);
    let gb = |v: Option<f64>| fmt::gb_from_mb(v.map(|v| v as u64));
    let y1 = if second { 122.0 } else { 130.0 };
    bottom_label_value(pm, th, f, b, b.y + y1 * u, "USED / TOTAL", &format!("{} / {}", gb(used), gb(total)), 19.0, u);
    if second {
        let free = match (used, total) {
            (Some(u), Some(t)) => Some((t - u).max(0.0)),
            _ => None,
        };
        bottom_label_value(pm, th, f, b, b.y + 144.0 * u, "FREE", &gb(free), 17.0, u);
    }

    // CONTEXT
    let b = lay.bot(1);
    let fillc = m.and_then(|m| m.ctx_fill());
    big_bar_card(pm, th, f, b, "CTX FILL", fillc.map(|v| v * 100.0), fillc.map(|v| v as f32), u);
    let ctx = format!("{} / {}", fmt::count(m.and_then(|m| m.ctx_used)), fmt::count(m.and_then(|m| m.ctx_max)));
    bottom_label_value(pm, th, f, b, b.y + y1 * u, "TOKENS", &ctx, 19.0, u);
    if second {
        let req = m
            .and_then(|m| m.current_request.as_ref().filter(|_| m.phase() != Phase::Idle).map(|r| (r.prompt_tokens, r.cached_tokens)))
            .or_else(|| m.and_then(|m| m.last_request.as_ref().map(|r| (r.prompt_tokens, r.cached_tokens))));
        let pc = match req {
            Some((p, c)) => format!("{} / {}", fmt::count(p), fmt::count(c)),
            None => fmt::NONE.into(),
        };
        bottom_label_value(pm, th, f, b, b.y + 144.0 * u, "PROMPT / CACHED", &pc, 17.0, u);
    }

    // TEMPERATURE: GPU temps, then memory temps (rows that do not fit are dropped)
    let b = lay.bot(3);
    let (y0, pitch) = rows(b, u);
    let mut trows: Vec<(String, Option<f64>, f64)> = Vec::new();
    for g in d.gpus.iter().take(2) {
        let i = g.index.map_or("?".into(), |i| i.to_string());
        trows.push((format!("GPU{i}"), g.temp_c, g.temp_limit_c.unwrap_or(100.0)));
    }
    for g in d.gpus.iter().take(2) {
        let i = g.index.map_or("?".into(), |i| i.to_string());
        trows.push((format!("MEM{i}"), g.mem_temp_c, 110.0));
    }
    if trows.is_empty() {
        trows.push(("GPU".into(), None, 100.0));
        trows.push(("MEM".into(), None, 110.0));
    }
    for (i, (lb, v, max)) in trows.iter().take(4).enumerate() {
        let y = y0 + i as f32 * pitch;
        if !row_fits(b, y) {
            break;
        }
        bar_row(pm, th, f, b, y, lb, frac(*v, *max), &fmt::temp(*v), u);
    }

    // POWER: per GPU (max 3), then TOTAL, then LIMIT
    let b = lay.bot(4);
    let (y0, pitch) = rows(b, u);
    let tp = sum_opt(d.gpus.iter().map(|g| g.power_w));
    let tm = sum_opt(d.gpus.iter().map(|g| g.power_max_w)).unwrap_or(0.0);
    let i = std::cell::Cell::new(0usize);
    let next = |b: R| {
        let y = y0 + i.get() as f32 * pitch;
        i.set(i.get() + 1);
        row_fits(b, y).then_some(y)
    };
    for g in d.gpus.iter().take(3) {
        let idx = g.index.map_or("?".into(), |i| i.to_string());
        let max = g.power_max_w.unwrap_or(350.0).max(1.0);
        if let Some(y) = next(b) {
            bar_row(pm, th, f, b, y, &format!("GPU{idx}"), frac(g.power_w, max), &fmt::watts(g.power_w), u);
        }
    }
    if d.gpus.is_empty() {
        if let Some(y) = next(b) {
            bar_row(pm, th, f, b, y, "GPU", None, fmt::NONE, u);
        }
    }
    if i.get() < 4 {
        if let Some(y) = next(b) {
            bar_row(pm, th, f, b, y, "TOTAL", if tm > 0.0 { frac(tp, tm) } else { None }, &fmt::watts(tp), u);
        }
    }
    if i.get() < 4 && tm > 0.0 {
        if let Some(y) = next(b) {
            bottom_label_value(pm, th, f, b, y, "LIMIT", &fmt::watts(Some(tm)), 17.0, u);
        }
    }

    // REQUESTS
    let b = lay.bot(5);
    let (y0, pitch) = rows(b, u);
    let lr = m.and_then(|m| m.last_request.as_ref());
    let ttft = lr
        .and_then(|r| r.ttft_s)
        .or_else(|| m.and_then(|m| m.current_request.as_ref()).and_then(|r| r.ttft_s));
    let peak = m.and_then(|m| m.peak_decode_tps).or_else(|| lr.and_then(|r| r.peak_decode_tps));
    let rws = [
        ("LAST TTFT", fmt::secs(ttft)),
        ("AVG DECODE", fmt::tps(lr.and_then(|r| r.avg_decode_tps))),
        ("PEAK DECODE", fmt::tps(peak)),
        ("FINISHED", fmt::count(m.and_then(|m| m.requests_finished))),
    ];
    for (i, (lb, v)) in rws.iter().enumerate() {
        let y = y0 + i as f32 * pitch;
        if !row_fits(b, y) {
            break;
        }
        bottom_label_value(pm, th, f, b, y, lb, v, 19.0, u);
        if i < 3 && row_fits(b, y + pitch) {
            line(pm, b.x + 14.0, y + 6.0, b.x + b.w - 14.0, y + 6.0, alpha(th.accent, 0.16), 1.0);
        }
    }
}

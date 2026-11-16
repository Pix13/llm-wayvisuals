//! DATA PATH diagram (left card) and CPU card, driven by the v2 export
//! fields (`Snapshot.system`, `Model.bandwidth`, `process_cpu_pct`, ...).

use super::canvas::Canvas;

use super::draw::*;
use super::fmt;
use super::scene::{fan_rot, px, seg_bar, Lay, R};
use crate::model::{Model, Snapshot};
use crate::theme::{alpha, mix, Theme};

pub const RED: (f32, f32, f32) = (1.0, 0.19, 0.19);

fn red() -> Color {
    Color::from_rgba(RED.0, RED.1, RED.2, 1.0).unwrap()
}

/// One physical GPU of the diagram.
#[derive(Debug, Clone, PartialEq)]
pub struct GpuNode {
    pub index: u32,
    /// Short display name (`RTX 3090`).
    pub name: Option<String>,
    /// Memory-controller busy % (`bandwidth.vram[gpu].busy_pct`).
    pub busy_pct: Option<f64>,
    /// Host -> device PCIe MB/s and the link capacity.
    pub rx_mb_s: Option<f64>,
    pub cap_mb_s: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Limit {
    None,
    Disk,
    Ram,
    /// Link CPU -> GPU with this index.
    Pcie(u32),
    /// The GPU box itself (VRAM bandwidth or compute bound).
    Gpu(u32, bool),
}

/// Everything the data path diagram needs, extracted from the snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct PathView {
    /// First 4 physical GPUs.
    pub gpus: Vec<GpuNode>,
    /// GPUs beyond the first 4.
    pub extra: usize,
    /// SSD -> CPU: system-wide block reads (falls back to the process's own).
    pub disk_mb_s: Option<f64>,
    pub ram_gb_s: Option<f64>,
    pub limit: Limit,
    pub headline: Option<String>,
    pub cpu_pct: Option<f64>,
    /// A model is selected (else the verdict reads `--`).
    pub has_model: bool,
}

impl PathView {
    pub fn from(snap: Option<&Snapshot>, model: Option<&Model>) -> Self {
        let bw = model.and_then(|m| m.bandwidth.as_ref());
        let all = snap.map(|s| s.gpus.as_slice()).unwrap_or(&[]);
        let gpus = all
            .iter()
            .take(4)
            .enumerate()
            .map(|(k, g)| {
                let index = g.index.unwrap_or(k as u32);
                let link = bw.and_then(|b| b.pcie.iter().find(|p| p.gpu == Some(index)));
                let vram = bw.and_then(|b| b.vram.iter().find(|v| v.gpu == Some(index)));
                GpuNode {
                    index,
                    name: g.name.as_deref().map(fmt::short_gpu_name).filter(|n| !n.is_empty()),
                    busy_pct: vram.and_then(|v| v.busy_pct),
                    rx_mb_s: link.and_then(|l| l.rx_mb_s),
                    cap_mb_s: link.and_then(|l| l.cap_mb_s),
                }
            })
            .collect();
        let (limit, headline) = match model.and_then(|m| m.active_bottleneck()) {
            Some(b) => {
                let g = b.gpu.unwrap_or(0);
                let l = match b.stage.as_deref() {
                    Some("disk") => Limit::Disk,
                    Some("ram") => Limit::Ram,
                    Some("pcie") => Limit::Pcie(g),
                    Some("vram") => Limit::Gpu(g, false),
                    Some("compute") => Limit::Gpu(g, true),
                    _ => Limit::None,
                };
                (l, b.headline.clone())
            }
            None => (Limit::None, None),
        };
        let disk = match (bw.and_then(|b| b.disk_mb_s), bw.and_then(|b| b.proc_disk_mb_s)) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        PathView {
            gpus,
            extra: all.len().saturating_sub(4),
            disk_mb_s: disk,
            ram_gb_s: bw.and_then(|b| b.ram_gb_s),
            limit,
            headline,
            cpu_pct: snap.and_then(|s| s.system.as_ref()).and_then(|s| s.cpu_pct),
            has_model: model.is_some(),
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Link {
    /// No data: outline only, label `--`.
    Unknown,
    Idle,
    Flow(f32),
    Limit(f32),
}

/// `frac`: share of the link capacity (or a log-scaled figure when the
/// capacity is unknown), 0..1.
fn link_state(rate: Option<f64>, eps: f64, frac: f32, limited: bool) -> Link {
    match rate {
        _ if limited => Link::Limit(frac.clamp(0.0, 1.0).max(0.6)),
        None => Link::Unknown,
        Some(r) if r > eps => Link::Flow(frac.clamp(0.0, 1.0).max(0.08)),
        Some(_) => Link::Idle,
    }
}

/// Log-scaled 0..1 against a reference rate (capacity unknown).
fn log_frac(v: f64, reference: f64) -> f32 {
    ((1.0 + v.max(0.0)).ln() / (1.0 + reference).ln()) as f32
}

fn rate_mb(mbps: f64) -> String {
    if mbps >= 1000.0 {
        format!("{:.1} GB/s", mbps / 1000.0)
    } else {
        format!("{:.0} MB/s", mbps)
    }
}

// ---- geometry helpers ------------------------------------------------------

fn dedupe(pts: &[(f32, f32)]) -> Vec<(f32, f32)> {
    let mut v: Vec<(f32, f32)> = Vec::new();
    for p in pts {
        if v.last().is_none_or(|l| (l.0 - p.0).abs() > 0.1 || (l.1 - p.1).abs() > 0.1) {
            v.push(*p);
        }
    }
    v
}

fn path_len(p: &[(f32, f32)]) -> f32 {
    p.windows(2).map(|w| ((w[1].0 - w[0].0).powi(2) + (w[1].1 - w[0].1).powi(2)).sqrt()).sum()
}

fn along(p: &[(f32, f32)], mut d: f32) -> (f32, f32, f32, f32) {
    for w in p.windows(2) {
        let l = ((w[1].0 - w[0].0).powi(2) + (w[1].1 - w[0].1).powi(2)).sqrt();
        if d <= l || std::ptr::eq(w, &p[p.len() - 2..]) {
            let k = if l > 0.0 { (d / l).clamp(0.0, 1.0) } else { 0.0 };
            return (w[0].0 + (w[1].0 - w[0].0) * k, w[0].1 + (w[1].1 - w[0].1) * k, (w[1].0 - w[0].0) / l.max(1e-3), (w[1].1 - w[0].1) / l.max(1e-3));
        }
        d -= l;
    }
    (p[0].0, p[0].1, 0.0, -1.0)
}

/// A pipe (hollow tube) along `pts` in flow direction, with an arrow head.
fn pipe(pm: &mut dyn Canvas, th: &Theme, pts: &[(f32, f32)], pw: f32, st: Link, t: f32) {
    let pts = dedupe(pts);
    if pts.len() < 2 {
        return;
    }
    let mut pb = PathBuilder::new();
    pb.move_to(pts[0].0, pts[0].1);
    for p in &pts[1..] {
        pb.line_to(p.0, p.1);
    }
    let path = pb.finish().unwrap();
    let (col, outline, inner) = match st {
        Link::Idle | Link::Unknown => (alpha(th.accent, 0.3), alpha(th.accent, 0.30), 0.0),
        Link::Flow(_) => (th.accent, alpha(th.accent, 0.80), 0.30),
        Link::Limit(_) => (red(), alpha(red(), 0.95), 0.40),
    };
    if matches!(st, Link::Limit(_)) {
        let pulse = 0.75 + 0.25 * (t * 4.0).sin();
        stroke(pm, &path, alpha(red(), 0.10 * pulse), pw + 12.0);
        stroke(pm, &path, alpha(red(), 0.20 * pulse), pw + 6.0);
    }
    stroke(pm, &path, outline, pw);
    stroke(pm, &path, Color::BLACK, pw - 2.4);
    if inner > 0.0 {
        stroke(pm, &path, alpha(col, inner), pw - 2.4);
    }
    // particles
    if let Link::Flow(f) | Link::Limit(f) = st {
        let l = path_len(&pts);
        let spacing = 28.0 - 17.0 * f;
        let speed = 22.0 + 60.0 * f.sqrt();
        let n = (l / spacing).ceil() as usize + 1;
        let pc = if matches!(st, Link::Limit(_)) { Color::from_rgba(1.0, 0.75, 0.6, 1.0).unwrap() } else { th.bright };
        for k in 0..n {
            let d = (t * speed + k as f32 * spacing) % (n as f32 * spacing);
            if d > l {
                continue;
            }
            let (x, y, dx, dy) = along(&pts, d);
            let (tx2, ty2) = (x - dx * spacing * 0.35, y - dy * spacing * 0.35);
            stroke(pm, &line_path(tx2, ty2, x, y), alpha(pc, 0.55), (pw - 4.0).max(1.5));
            fill(pm, &circle_path(x, y, ((pw - 3.0) * 0.5).max(1.2)), pc);
        }
    }
    // arrow head at the end
    let n = pts.len();
    let (x1, y1) = pts[n - 1];
    let (x0, y0) = pts[n - 2];
    let (dx, dy) = ((x1 - x0), (y1 - y0));
    let l = (dx * dx + dy * dy).sqrt().max(1e-3);
    let (dx, dy) = (dx / l, dy / l);
    let a = pw * 0.95;
    let (nx, ny) = (-dy, dx);
    let mut pb = PathBuilder::new();
    pb.move_to(x1 + dx * a * 0.2, y1 + dy * a * 0.2);
    pb.line_to(x1 - dx * a * 0.9 + nx * a * 0.8, y1 - dy * a * 0.9 + ny * a * 0.8);
    pb.line_to(x1 - dx * a * 0.9 - nx * a * 0.8, y1 - dy * a * 0.9 - ny * a * 0.8);
    pb.close();
    fill(pm, &pb.finish().unwrap(), match st {
        Link::Idle | Link::Unknown => alpha(th.accent, 0.35),
        Link::Flow(_) => th.bright,
        Link::Limit(_) => red(),
    });
}

fn tag(pm: &mut dyn Canvas, f: &Fonts, x: f32, y: f32, align: Align, t: f32) {
    let s = 11.0;
    let w = text_width(&f.semi, s, "BOTTLENECK", 0.8) + 8.0;
    let x0 = match align {
        Align::Left => x,
        Align::Center => x - w / 2.0,
        Align::Right => x - w,
    };
    let pulse = 0.8 + 0.2 * (t * 4.0).sin();
    fill(pm, &rrect_path(x0, y - 10.0, w, 13.5, 2.5), alpha(red(), 0.92 * pulse));
    text(pm, &f.semi, s, x0 + 4.0, y, "BOTTLENECK", Color::BLACK, 0.8, Align::Left);
}

fn rate_text(pm: &mut dyn Canvas, th: &Theme, f: &Fonts, x: f32, y: f32, align: Align, s: &str, st: Link, u: f32) {
    let c = match st {
        Link::Idle | Link::Unknown => alpha(th.dim, 0.55),
        Link::Flow(_) => th.value,
        Link::Limit(_) => Color::from_rgba(1.0, 0.55, 0.5, 1.0).unwrap(),
    };
    text(pm, &f.semi, px(14.0, u, 11.5), x, y, s, c, 0.2, align);
}

// ---- blocks -----------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn gpu_box(pm: &mut dyn Canvas, th: &Theme, f: &Fonts, x: f32, y: f32, w: f32, h: f32, i: u32, g: Option<&GpuNode>, bound: bool, t: f32, u: f32) {
    let busy = g.and_then(|g| g.busy_pct);
    let border = if bound { red() } else { alpha(th.accent, 0.65) };
    let body = rrect_path(x, y, w, h, 5.0 * u.max(0.7));
    if bound {
        let pulse = 0.75 + 0.25 * (t * 4.0).sin();
        stroke(pm, &body, alpha(red(), 0.10 * pulse), 12.0);
        stroke(pm, &body, alpha(red(), 0.22 * pulse), 6.0);
        fill(pm, &body, alpha(red(), 0.10));
    } else {
        fill(pm, &body, alpha(th.accent, 0.05));
    }
    stroke(pm, &body, border, 1.6);
    let lx = x + 8.0;
    let fr = (h * 0.28).min(15.0 * u.max(0.7));
    let ls = px(16.0, u, 12.0);
    let by_l = y + 22.0 * u.max(0.6);
    let lb = format!("GPU{i}");
    let lw = text(pm, &f.semi, ls, lx, by_l, &lb, th.value, 0.6, Align::Left);
    // short GPU name next to the label, clipped to the room left of the fan
    if let Some(n) = g.and_then(|g| g.name.as_deref()) {
        let avail = x + w - fr * 2.0 - 10.0 - (lx + lw + 6.0);
        let ns = px(11.0, u, 10.0);
        let mut nm = clip_text(&f.label, ns, &format!("· {n}"), avail, 0.3);
        if nm.ends_with('…') {
            let alt = clip_text(&f.label, ns, n, avail, 0.3);
            if !alt.ends_with('…') {
                nm = alt; // drop the dot rather than the name
            }
        }
        if avail > 14.0 && nm != "…" && !nm.is_empty() {
            text(pm, &f.label, ns, lx + lw + 6.0, by_l, &nm, th.dim, 0.3, Align::Left);
        }
    }
    // fan, spinning with the memory/compute activity
    let (fcx, fcy) = (x + w - fr - 8.0, y + h * 0.40);
    fan_rot(pm, fcx, fcy, fr, alpha(if bound { red() } else { th.bright }, 0.85), t * (40.0 + 600.0 * busy.unwrap_or(0.0) as f32 / 100.0));
    // VRAM bar
    let bw = (w - fr * 2.0 - 26.0).max(30.0);
    let bh = (7.0 * u).max(5.0);
    let by = y + h * 0.54;
    seg_bar(pm, th, lx, by, bw, bh, 10, busy.map(|b| (b / 100.0) as f32));
    text(pm, &f.label, px(11.0, u, 10.0), lx, y + h - 5.0, &format!("VRAM BUSY {}", fmt::pct(busy)), th.dim, 0.5, Align::Left);
}

fn chip_pins(pm: &mut dyn Canvas, c: Color, x: f32, y: f32, w: f32, h: f32, n: usize, len: f32) {
    for k in 0..n {
        let fx = x + w * (k as f32 + 1.0) / (n as f32 + 1.0);
        let fy = y + h * (k as f32 + 1.0) / (n as f32 + 1.0);
        line(pm, fx, y - len, fx, y, c, 1.4);
        line(pm, fx, y + h, fx, y + h + len, c, 1.4);
        line(pm, x - len, fy, x, fy, c, 1.4);
        line(pm, x + w, fy, x + w + len, fy, c, 1.4);
    }
}

// ---- data path card -----------------------------------------------------------

pub fn draw_data_path(pm: &mut dyn Canvas, th: &Theme, f: &Fonts, lay: &Lay, v: &PathView, t: f32) {
    let card = lay.top(0);
    let u = lay.ut;
    let ox = card.x + 14.0;
    let cw = card.w - 28.0;
    let y0 = card.y + 50.0 * u;
    let n = v.gpus.len().clamp(1, 4);
    let lim = v.limit;
    let pw = px(11.0, u, 7.0);

    // GPU row
    let slot = (cw / n as f32).min(200.0);
    let x_start = ox + (cw - slot * n as f32) / 2.0;
    let gw = (slot - 14.0).min([190.0, 170.0, 150.0, 132.0][n - 1]);
    let (gy, gh) = (y0, 60.0 * u);
    let gcx: Vec<f32> = (0..n).map(|i| x_start + slot * (i as f32 + 0.5)).collect();

    // CPU, RAM, SSD
    let cx = card.x + 310.0;
    let (cpu_w, cpu_h) = ((112.0 * u).max(72.0), 48.0 * u);
    let cpu = R { x: cx - cpu_w / 2.0, y: y0 + 108.0 * u, w: cpu_w, h: cpu_h };
    let cpu_cy = cpu.y + cpu.h / 2.0;
    let dimm = R { x: cpu.x + cpu.w + 92.0 * u, y: cpu_cy - 14.0 * u, w: 124.0 * u, h: 28.0 * u };
    let ssd_w = (160.0 * u).max(100.0);
    let ssd = R { x: cx - ssd_w / 2.0, y: y0 + 182.0 * u, w: ssd_w, h: 32.0 * u };

    // ---- links
    let zone_top = gy + gh;
    let zh = cpu.y - zone_top;
    let dmax = (0..n).map(|i| ((i as f32) - (n as f32 - 1.0) / 2.0).abs()).fold(0.0f32, f32::max).max(0.5);
    for i in 0..n {
        let g = v.gpus.get(i);
        let idx = g.map_or(i as u32, |g| g.index);
        let rx = g.and_then(|g| g.rx_mb_s);
        let cap = g.and_then(|g| g.cap_mb_s).filter(|c| *c > 0.0);
        let frac = match (rx, cap) {
            (Some(r), Some(c)) => (r / c) as f32,
            (Some(r), None) => log_frac(r, 31500.0),
            _ => 0.0,
        };
        let st = link_state(rx, 10.0, frac, lim == Limit::Pcie(idx));
        let d = ((i as f32) - (n as f32 - 1.0) / 2.0).abs();
        let xs = cpu.x + cpu.w * (i as f32 + 1.0) / (n as f32 + 1.0);
        let ym = zone_top + zh * (0.22 + 0.5 * d / dmax);
        pipe(pm, th, &[(xs, cpu.y), (xs, ym), (gcx[i], ym), (gcx[i], zone_top)], pw, st, t);
        // labels on the outer side of the final vertical
        let right = (i as f32) >= (n as f32 - 1.0) / 2.0 && !(n > 1 && i == 0);
        let (lx, al) = if right { (gcx[i] + pw + 4.0, Align::Left) } else { (gcx[i] - pw - 4.0, Align::Right) };
        let label = match (rx, cap) {
            (None, _) => fmt::NONE.to_string(),
            (Some(r), Some(c)) => format!("{:.1}/{:.1} GB/s", r / 1000.0, c / 1000.0),
            (Some(r), None) => rate_mb(r),
        };
        rate_text(pm, th, f, lx, zone_top + 11.0, al, &label, st, u);
        if matches!(st, Link::Limit(_)) || matches!(lim, Limit::Gpu(gi, _) if gi == idx) {
            tag(pm, f, lx, zone_top + 24.0, al, t);
        }
    }
    // RAM -> CPU (flows leftwards)
    {
        let st = link_state(v.ram_gb_s, 0.05, v.ram_gb_s.map_or(0.0, |r| log_frac(r, 85.0)), lim == Limit::Ram);
        let (xa, xb) = (dimm.x, cpu.x + cpu.w);
        pipe(pm, th, &[(xa, cpu_cy), (xb, cpu_cy)], pw, st, t);
        let mid = (xa + xb) / 2.0;
        let label = v.ram_gb_s.map_or(fmt::NONE.to_string(), |r| format!("{:.1} GB/s", r));
        rate_text(pm, th, f, mid, cpu_cy - pw / 2.0 - 4.0, Align::Center, &label, st, u);
        if v.ram_gb_s.is_none() {
            text(pm, &f.label, 10.0, mid, cpu_cy + pw / 2.0 + 12.0, "n/a", alpha(th.dim, 0.6), 0.8, Align::Center);
        }
        if matches!(st, Link::Limit(_)) {
            tag(pm, f, mid, cpu_cy + pw / 2.0 + 14.0, Align::Center, t);
        }
    }
    // SSD -> CPU (flows upwards)
    {
        let st = link_state(v.disk_mb_s, 1.0, v.disk_mb_s.map_or(0.0, |r| log_frac(r, 7000.0)), lim == Limit::Disk);
        pipe(pm, th, &[(cx, ssd.y), (cx, cpu.y + cpu.h)], pw, st, t);
        let my = (ssd.y + cpu.y + cpu.h) / 2.0 + 4.0;
        let label = v.disk_mb_s.map_or(fmt::NONE.to_string(), rate_mb);
        rate_text(pm, th, f, cx + pw + 5.0, my, Align::Left, &label, st, u);
        if matches!(st, Link::Limit(_)) {
            tag(pm, f, cx - pw - 5.0, my, Align::Right, t);
        }
    }

    // ---- blocks (drawn after pipes so their edges stay clean)
    for i in 0..n {
        let g = v.gpus.get(i);
        let idx = g.map_or(i as u32, |g| g.index);
        gpu_box(pm, th, f, gcx[i] - gw / 2.0, gy, gw, gh, idx, g, matches!(lim, Limit::Gpu(gi, _) if gi == idx), t, u);
    }
    if v.extra > 0 {
        text(pm, &f.semi, px(14.0, u, 11.0), card.x + card.w - 16.0, gy - 2.0 * u, &format!("+{}", v.extra), th.dim, 0.5, Align::Right);
    }
    // CPU chip
    {
        let body = rrect_path(cpu.x, cpu.y, cpu.w, cpu.h, 4.0);
        fill(pm, &body, Color::BLACK);
        let load = (v.cpu_pct.unwrap_or(0.0) / 100.0).clamp(0.0, 1.0) as f32;
        fill(pm, &body, alpha(th.accent, 0.06 + 0.22 * load));
        stroke(pm, &body, alpha(th.accent, 0.85), 1.6);
        chip_pins(pm, alpha(th.accent, 0.7), cpu.x, cpu.y, cpu.w, cpu.h, 5, 5.0 * u.max(0.7));
        let lb = format!("CPU {}", v.cpu_pct.map_or(fmt::NONE.to_string(), |c| format!("{c:.0}%")));
        text(pm, &f.semi, px(18.0, u, 12.5), cx, cpu_cy + 6.0 * u.max(0.7), &lb, th.value, 0.6, Align::Center);
    }
    // DIMM
    {
        let body = rrect_path(dimm.x, dimm.y, dimm.w, dimm.h, 2.5);
        fill(pm, &body, alpha(th.accent, 0.06));
        stroke(pm, &body, alpha(th.accent, 0.75), 1.5);
        let chips = 6;
        let cwid = (dimm.w - 20.0) / chips as f32;
        for k in 0..chips {
            rect_aa(pm, dimm.x + 8.0 + k as f32 * cwid, dimm.y + dimm.h * 0.22, cwid - 3.0, dimm.h * 0.4, alpha(th.accent, 0.35));
        }
        for k in 0..16 {
            rect(pm, dimm.x + 5.0 + k as f32 * ((dimm.w - 10.0) / 16.0), dimm.y + dimm.h - 3.5, 2.2, 2.5, alpha(th.bright, 0.7));
        }
        text(pm, &f.label, px(12.0, u, 10.0), dimm.x + dimm.w / 2.0, dimm.y - 4.0, "RAM", th.dim, 1.0, Align::Center);
    }
    // SSD (M.2 stick)
    {
        let body = rrect_path(ssd.x, ssd.y, ssd.w, ssd.h, 3.0);
        fill(pm, &body, alpha(th.accent, 0.06));
        stroke(pm, &body, alpha(th.accent, 0.75), 1.5);
        for k in 0..6 {
            rect(pm, ssd.x + 3.0 + k as f32 * 3.4, ssd.y + 3.0, 2.0, ssd.h - 6.0, alpha(th.bright, 0.6));
        }
        for k in 0..2 {
            rect_aa(pm, ssd.x + ssd.w * (0.30 + 0.20 * k as f32), ssd.y + ssd.h * 0.2, ssd.w * 0.15, ssd.h * 0.6, alpha(th.accent, 0.35));
        }
        text(pm, &f.semi, px(16.0, u, 11.5), ssd.x + ssd.w - 8.0, ssd.y + ssd.h * 0.62, "SSD", th.value, 0.8, Align::Right);
    }

    // ---- verdict (left column, free area)
    let lx = card.x + 24.0;
    let any_flow = v.ram_gb_s.is_some_and(|r| r > 0.05)
        || v.disk_mb_s.is_some_and(|d| d > 1.0)
        || v.gpus.iter().any(|g| g.rx_mb_s.is_some_and(|r| r > 10.0));
    let (name, col) = match lim {
        Limit::None if !v.has_model => (fmt::NONE.to_string(), alpha(th.dim, 0.9)),
        Limit::None => (if any_flow { "BALANCED" } else { "IDLE" }.to_string(), alpha(th.dim, 0.9)),
        Limit::Disk => ("DISK READ".to_string(), red()),
        Limit::Ram => ("RAM BANDWIDTH".to_string(), red()),
        Limit::Pcie(i) => (format!("PCIE TO GPU{i}"), red()),
        Limit::Gpu(i, false) => (format!("GPU{i} VRAM BANDWIDTH"), red()),
        Limit::Gpu(i, true) => (format!("GPU{i} COMPUTE"), red()),
    };
    let vw = cpu.x - lx - 14.0;
    let vy = cpu_cy + 2.0 * u;
    text(pm, &f.label, px(12.0, u, 10.5), lx, vy - 12.0 * u.max(0.6), "LIMITED BY", th.dim, 1.4, Align::Left);
    let vs = fit_size(&f.semi, px(23.0, u, 14.0), 11.0, &name, vw, 0.6);
    text(pm, &f.semi, vs, lx, vy + 12.0 * u.max(0.7), &name, col, 0.6, Align::Left);
    if let Some(h) = v.headline.as_deref().filter(|_| lim != Limit::None) {
        let hs = px(12.0, u, 10.5);
        let h = clip_text(&f.label, hs, h, vw + 20.0, 0.3);
        text(pm, &f.label, hs, lx, vy + 12.0 * u.max(0.7) + hs + 3.0, &h, alpha(th.dim, 0.95), 0.3, Align::Left);
    }
    // legend (hidden on short bands)
    if lay.hd >= 400.0 {
        let ly = ssd.y + ssd.h * 0.62;
        let mut x = lx;
        for (lb, st) in [("IDLE", Link::Idle), ("FLOW", Link::Flow(0.5)), ("LIMIT", Link::Limit(0.5))] {
            let c = match st {
                Link::Flow(_) => th.accent,
                Link::Limit(_) => red(),
                _ => alpha(th.accent, 0.3),
            };
            let r = rrect_path(x, ly - 7.0, 16.0, 7.0, 2.0);
            stroke(pm, &r, c, 1.3);
            if st != Link::Idle {
                fill(pm, &r, alpha(c, 0.45));
            }
            x += 21.0;
            text(pm, &f.label, 11.0, x, ly, lb, th.dim, 0.8, Align::Left);
            x += text_width(&f.label, 11.0, lb, 0.8) + 14.0;
        }
    }
}

// ---- CPU card -------------------------------------------------------------------

pub struct CpuView<'a> {
    pub cpu_pct: Option<f64>,
    /// (age 0..1 over 15 s where 0 = now, 0..100 %), None = gap.
    pub wave: &'a [(f32, Option<f32>)],
    pub proc_pct: Option<f64>,
    pub threads: Option<u32>,
}

impl<'a> CpuView<'a> {
    pub fn from(snap: Option<&Snapshot>, model: Option<&Model>, wave: &'a [(f32, Option<f32>)]) -> Self {
        CpuView {
            cpu_pct: snap.and_then(|s| s.system.as_ref()).and_then(|s| s.cpu_pct),
            wave,
            proc_pct: model.and_then(|m| m.process_cpu_pct),
            threads: model.and_then(|m| m.engine_threads),
        }
    }
}

pub fn draw_cpu_card(pm: &mut dyn Canvas, th: &Theme, f: &Fonts, lay: &Lay, v: &CpuView) {
    let b = lay.bot(2);
    let u = lay.ub;
    text(pm, &f.label, px(14.0, u, 10.0), b.x + 14.0, b.y + 70.0 * u, "SYSTEM LOAD", th.text, 1.0, Align::Left);
    text(pm, &f.semi, px(34.0, u, 22.0), b.x + b.w - 14.0, b.y + 74.0 * u, &fmt::pct(v.cpu_pct), th.value, 0.2, Align::Right);
    // area chart, 0..100 %
    let (x0, x1) = (b.x + 14.0, b.x + b.w - 14.0);
    let (cy0, cy1) = (b.y + 82.0 * u, b.y + 128.0 * u);
    for (k, a) in [(0.0, 0.22), (0.5, 0.10), (1.0, 0.10)] {
        let y = cy1 - (cy1 - cy0) * k;
        line(pm, x0, y, x1, y, alpha(th.accent, a), 1.0);
    }
    let mut pts: Vec<(f32, Option<f32>)> = v.wave.iter().map(|&(a, v)| (a.clamp(0.0, 1.0), v)).collect();
    pts.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal)); // oldest first
    let mut line_pb = PathBuilder::new();
    let mut fill_pb = PathBuilder::new();
    let mut pen: Option<(f32, f32)> = None; // (first x, last x)
    let close = |fb: &mut PathBuilder, pen: Option<(f32, f32)>| {
        if let Some((fx, lx)) = pen {
            fb.line_to(lx, cy1);
            fb.line_to(fx, cy1);
            fb.close();
        }
    };
    let mut last = None;
    for (age, val) in pts {
        match val {
            Some(val) => {
                let x = x0 + (x1 - x0) * (1.0 - age);
                let y = cy1 - (cy1 - cy0) * (val / 100.0).clamp(0.0, 1.0);
                match pen {
                    Some((fx, _)) => {
                        line_pb.line_to(x, y);
                        fill_pb.line_to(x, y);
                        pen = Some((fx, x));
                    }
                    None => {
                        line_pb.move_to(x, y);
                        fill_pb.move_to(x, cy1);
                        fill_pb.line_to(x, y);
                        pen = Some((x, x));
                    }
                }
                last = Some((x, y));
            }
            None => {
                close(&mut fill_pb, pen);
                pen = None;
                last = None;
            }
        }
    }
    close(&mut fill_pb, pen);
    if let Some(p) = fill_pb.finish() {
        fill(pm, &p, alpha(th.accent, 0.20));
    }
    if let Some(p) = line_pb.finish() {
        glow_stroke(pm, &p, th.bright, 1.6, 5.0, 0.08);
    }
    if let Some((ex, ey)) = last {
        fill(pm, &circle_path(ex, ey, 2.6), th.value);
    }
    let pc = v.proc_pct.map_or(fmt::NONE.to_string(), |p| format!("{p:.1}%"));
    let th_s = v.threads.map_or(format!("{}T", fmt::NONE), |t| format!("{t}T"));
    let proc = format!("PROC {pc} · {th_s}");
    text(pm, &f.semi, px(15.0, u, 11.0), b.x + 14.0, (b.y + 148.0 * u).min(b.y + b.h - 8.0), &proc, mix(th.text, th.accent, 0.2), 0.8, Align::Left);
}

//! Drawing helpers over the backend-neutral [`Canvas`]: path constructors,
//! strokes with glow, rectangles and aligned text. Text measuring lives in
//! `text.rs` (re-exported here).


use super::canvas::Canvas;
pub use super::canvas::{Color, FontFace, Path, PathBuilder};
pub use super::text::{cap, clip_text, fit_size, text_width, Fonts};
use crate::theme::alpha;

/// `pixel = design * s + (ox, oy)` as set on the canvas.
pub fn tx(c: &dyn Canvas, x: f32) -> f32 {
    let (s, ox, _) = c.view();
    x * s + ox
}

pub fn ty(c: &dyn Canvas, y: f32) -> f32 {
    let (s, _, oy) = c.view();
    y * s + oy
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Center,
    Right,
}

// ---- paths -------------------------------------------------------------

pub fn rrect_path(x: f32, y: f32, w: f32, h: f32, r: f32) -> Path {
    let r = r.min(w / 2.0).min(h / 2.0);
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.quad_to(x + w, y, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.quad_to(x + w, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.quad_to(x, y + h, x, y + h - r);
    pb.line_to(x, y + r);
    pb.quad_to(x, y, x + r, y);
    pb.close();
    pb.finish().unwrap()
}

/// Rectangle with 45-degree cut corners.
pub fn chamfer_path(x: f32, y: f32, w: f32, h: f32, c: f32) -> Path {
    let mut pb = PathBuilder::new();
    pb.move_to(x + c, y);
    pb.line_to(x + w - c, y);
    pb.line_to(x + w, y + c);
    pb.line_to(x + w, y + h - c);
    pb.line_to(x + w - c, y + h);
    pb.line_to(x + c, y + h);
    pb.line_to(x, y + h - c);
    pb.line_to(x, y + c);
    pb.close();
    pb.finish().unwrap()
}

pub fn circle_path(cx: f32, cy: f32, r: f32) -> Path {
    PathBuilder::from_circle(cx, cy, r).unwrap()
}

/// Polyline arc, angles in degrees clockwise from +x (screen space).
pub fn arc_path(cx: f32, cy: f32, r: f32, a0: f32, a1: f32) -> Path {
    let n = (((a1 - a0).abs() / 4.0).ceil() as usize).max(2);
    let mut pb = PathBuilder::new();
    for i in 0..=n {
        let a = (a0 + (a1 - a0) * i as f32 / n as f32).to_radians();
        let (x, y) = (cx + r * a.cos(), cy + r * a.sin());
        if i == 0 {
            pb.move_to(x, y)
        } else {
            pb.line_to(x, y)
        }
    }
    pb.finish().unwrap()
}

/// Annular sector between radii r0..r1 and angles a0..a1 (degrees).
pub fn sector_path(cx: f32, cy: f32, r0: f32, r1: f32, a0: f32, a1: f32) -> Path {
    let n = (((a1 - a0).abs() / 2.5).ceil() as usize).max(2);
    let mut pb = PathBuilder::new();
    for i in 0..=n {
        let (x, y) = polar(cx, cy, r1, a0 + (a1 - a0) * i as f32 / n as f32);
        if i == 0 { pb.move_to(x, y) } else { pb.line_to(x, y) }
    }
    for i in (0..=n).rev() {
        let (x, y) = polar(cx, cy, r0, a0 + (a1 - a0) * i as f32 / n as f32);
        pb.line_to(x, y);
    }
    pb.close();
    pb.finish().unwrap()
}

pub fn line_path(x0: f32, y0: f32, x1: f32, y1: f32) -> Path {
    let mut pb = PathBuilder::new();
    pb.move_to(x0, y0);
    pb.line_to(x1, y1);
    pb.finish().unwrap()
}

pub fn polar(cx: f32, cy: f32, r: f32, a_deg: f32) -> (f32, f32) {
    let a = a_deg.to_radians();
    (cx + r * a.cos(), cy + r * a.sin())
}

// ---- fill / stroke -----------------------------------------------------

pub fn fill(pm: &mut dyn Canvas, p: &Path, c: Color) {
    pm.fill_path(p, c);
}

pub fn stroke(pm: &mut dyn Canvas, p: &Path, c: Color, w: f32) {
    pm.stroke_path(p, c, w);
}

/// Cheap glow: a wide low-alpha stroke under the real one.
pub fn glow_stroke(pm: &mut dyn Canvas, p: &Path, c: Color, w: f32, glow_w: f32, glow_a: f32) {
    stroke(pm, p, alpha(c, glow_a), glow_w);
    stroke(pm, p, alpha(c, glow_a * 1.8), (glow_w + w) * 0.5);
    stroke(pm, p, c, w);
}

pub fn line(pm: &mut dyn Canvas, x0: f32, y0: f32, x1: f32, y1: f32, c: Color, w: f32) {
    stroke(pm, &line_path(x0, y0, x1, y1), c, w);
}

/// Rectangle, anti-aliased only when the view is not the identity (so that
/// 1:1 rendering stays pixel-exact).
pub fn rect(pm: &mut dyn Canvas, x: f32, y: f32, w: f32, h: f32, c: Color) {
    let aa = pm.view() != (1.0, 0.0, 0.0);
    pm.fill_rect(x, y, w, h, c, aa);
}

/// Blends `c` over the whole target (pixel space, ignores the view).
pub fn fill_all(pm: &mut dyn Canvas, c: Color) {
    pm.overlay(c);
}

pub fn rect_aa(pm: &mut dyn Canvas, x: f32, y: f32, w: f32, h: f32, c: Color) {
    pm.fill_rect(x, y, w, h, c, true);
}

// ---- text --------------------------------------------------------------

pub fn rgb255(c: Color) -> [f32; 3] {
    [c.red() * 255.0, c.green() * 255.0, c.blue() * 255.0]
}

/// Draws `s` with its baseline at `base_y`. Returns the drawn width.
#[allow(clippy::too_many_arguments)]
pub fn text(
    pm: &mut dyn Canvas,
    font: &FontFace,
    size: f32,
    x: f32,
    base_y: f32,
    s: &str,
    color: Color,
    tracking: f32,
    align: Align,
) -> f32 {
    let s = cap(s);
    let w = text_width(font, size, s, tracking);
    let x0 = match align {
        Align::Left => x,
        Align::Center => x - w / 2.0,
        Align::Right => x - w,
    };
    pm.text_run(font, size, x0, base_y, s, color, tracking);
    w
}

#[cfg(test)]
mod tests {
    use super::super::skia::SkiaStore;
    use ab_glyph::FontRef;
    use super::super::text::MAX_CHARS;
    use super::*;
    use tiny_skia::Pixmap;

    fn old_clip(font: &FontRef, size: f32, s: &str, max_w: f32, tracking: f32) -> String {
        if text_width(font, size, s, tracking) <= max_w {
            return s.to_string();
        }
        let mut t: String = s.to_string();
        while !t.is_empty() {
            t.pop();
            let c = format!("{t}…");
            if text_width(font, size, &c, tracking) <= max_w {
                return c;
            }
        }
        String::new()
    }

    #[test]
    fn clip_matches_reference_on_normal_strings() {
        let f = Fonts::load();
        for s in ["", "A", "DEEPSEEK-V4.1-FLASH / MAYA", "Qwen3-30B-A3B-Instruct-2507 · llama.cpp", "héllo wörld ünïcode"] {
            for w in [0.0, 5.0, 40.0, 120.0, 300.0, 1000.0] {
                for tr in [0.0, 0.8] {
                    assert_eq!(clip_text(&f.semi, 23.0, s, w, tr), old_clip(&f.semi, 23.0, s, w, tr), "{s:?} w={w} tr={tr}");
                }
            }
        }
    }

    #[test]
    fn huge_strings_are_capped_and_fast() {
        let f = Fonts::load();
        let big = "é".repeat(1_000_000);
        let t0 = std::time::Instant::now();
        let c = clip_text(&f.semi, 23.0, &big, 5000.0, 0.8);
        let w = text_width(&f.semi, 23.0, &big, 0.8);
        assert!(c.chars().count() <= MAX_CHARS + 1 && c.ends_with('…'));
        assert!(w > 0.0);
        let mut pm = Pixmap::new(64, 64).unwrap();
        let mut store = SkiaStore::new();
        let mut cv = store.canvas(&mut pm);
        text(&mut cv, &f.semi, 12.0, 0.0, 20.0, &big, Color::WHITE, 0.0, Align::Left);
        assert!(t0.elapsed().as_millis() < 500, "took {:?}", t0.elapsed());
        assert_eq!(cap(&big).chars().count(), MAX_CHARS);
    }
}

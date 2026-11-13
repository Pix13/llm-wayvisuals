//! Low-level drawing helpers over tiny-skia + ab_glyph.

use ab_glyph::{point, Font, FontRef, PxScale, ScaleFont, VariableFont};
use tiny_skia::{
    Color, FillRule, Paint, Path, PathBuilder, Pixmap, Rect, Stroke, Transform,
};

use crate::theme::alpha;

thread_local! {
    static VIEW: std::cell::Cell<(f32, f32, f32)> = const { std::cell::Cell::new((1.0, 0.0, 0.0)) };
}

/// Sets the design -> pixel mapping used by every primitive below:
/// `pixel = design * s + (ox, oy)`.
pub fn set_view(s: f32, ox: f32, oy: f32) {
    VIEW.with(|v| v.set((s, ox, oy)));
}

pub fn view() -> (f32, f32, f32) {
    VIEW.with(|v| v.get())
}

pub fn tr() -> Transform {
    let (s, ox, oy) = view();
    if s == 1.0 && ox == 0.0 && oy == 0.0 {
        Transform::identity()
    } else {
        Transform::from_row(s, 0.0, 0.0, s, ox, oy)
    }
}

pub fn tx(x: f32) -> f32 {
    let (s, ox, _) = view();
    x * s + ox
}

pub fn ty(y: f32) -> f32 {
    let (s, _, oy) = view();
    y * s + oy
}

pub struct Fonts {
    /// Rajdhani Medium: labels, small text.
    pub label: FontRef<'static>,
    /// Rajdhani SemiBold: titles, values.
    pub semi: FontRef<'static>,
    /// Oxanium (variable, wght 700): giant digits.
    pub digits: FontRef<'static>,
}

impl Fonts {
    pub fn load() -> Self {
        let label = FontRef::try_from_slice(include_bytes!(
            "../../assets/fonts/rajdhani/Rajdhani-Medium.ttf"
        ))
        .expect("Rajdhani-Medium");
        let semi = FontRef::try_from_slice(include_bytes!(
            "../../assets/fonts/rajdhani/Rajdhani-SemiBold.ttf"
        ))
        .expect("Rajdhani-SemiBold");
        let mut digits = FontRef::try_from_slice(include_bytes!(
            "../../assets/fonts/oxanium/Oxanium-VF.ttf"
        ))
        .expect("Oxanium");
        digits.set_variation(b"wght", 700.0);
        Fonts { label, semi, digits }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Center,
    Right,
}

pub fn paint(c: Color) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color(c);
    p.anti_alias = true;
    p
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

pub fn fill(pm: &mut Pixmap, p: &Path, c: Color) {
    pm.fill_path(p, &paint(c), FillRule::Winding, tr(), None);
}

pub fn stroke(pm: &mut Pixmap, p: &Path, c: Color, w: f32) {
    let s = Stroke {
        width: w,
        ..Stroke::default()
    };
    pm.stroke_path(p, &paint(c), &s, tr(), None);
}

/// Cheap glow: a wide low-alpha stroke under the real one.
pub fn glow_stroke(pm: &mut Pixmap, p: &Path, c: Color, w: f32, glow_w: f32, glow_a: f32) {
    stroke(pm, p, alpha(c, glow_a), glow_w);
    stroke(pm, p, alpha(c, glow_a * 1.8), (glow_w + w) * 0.5);
    stroke(pm, p, c, w);
}

pub fn line(pm: &mut Pixmap, x0: f32, y0: f32, x1: f32, y1: f32, c: Color, w: f32) {
    stroke(pm, &line_path(x0, y0, x1, y1), c, w);
}

pub fn rect(pm: &mut Pixmap, x: f32, y: f32, w: f32, h: f32, c: Color) {
    if let Some(r) = Rect::from_xywh(x, y, w, h) {
        let mut p = paint(c);
        let t = tr();
        p.anti_alias = !t.is_identity();
        pm.fill_rect(r, &p, t, None);
    }
}

/// Fills the whole pixmap (pixel space, ignores the view).
pub fn fill_all(pm: &mut Pixmap, c: Color) {
    let r = Rect::from_xywh(0.0, 0.0, pm.width() as f32, pm.height() as f32).unwrap();
    let mut p = paint(c);
    p.anti_alias = false;
    pm.fill_rect(r, &p, Transform::identity(), None);
}

pub fn rect_aa(pm: &mut Pixmap, x: f32, y: f32, w: f32, h: f32, c: Color) {
    if let Some(r) = Rect::from_xywh(x, y, w, h) {
        pm.fill_rect(r, &paint(c), tr(), None);
    }
}

// ---- text --------------------------------------------------------------

pub fn text_width(font: &FontRef, size: f32, s: &str, tracking: f32) -> f32 {
    let sc = font.as_scaled(PxScale::from(size));
    let mut w = 0.0;
    let mut prev = None;
    for ch in s.chars() {
        let g = sc.glyph_id(ch);
        if let Some(p) = prev {
            w += sc.kern(p, g);
        }
        w += sc.h_advance(g) + tracking;
        prev = Some(g);
    }
    (w - tracking).max(0.0)
}

#[inline]
pub fn blend_px(d: &mut [u8], c: [f32; 3], a: f32) {
    if a <= 0.0 {
        return;
    }
    let ia = 1.0 - a;
    d[0] = (c[0] * a + d[0] as f32 * ia) as u8;
    d[1] = (c[1] * a + d[1] as f32 * ia) as u8;
    d[2] = (c[2] * a + d[2] as f32 * ia) as u8;
    d[3] = (255.0 * a + d[3] as f32 * ia) as u8;
}

pub fn rgb255(c: Color) -> [f32; 3] {
    [c.red() * 255.0, c.green() * 255.0, c.blue() * 255.0]
}

/// Draws `s` with its baseline at `base_y`. Returns the drawn width.
#[allow(clippy::too_many_arguments)]
pub fn text(
    pm: &mut Pixmap,
    font: &FontRef,
    size: f32,
    x: f32,
    base_y: f32,
    s: &str,
    color: Color,
    tracking: f32,
    align: Align,
) -> f32 {
    let w = text_width(font, size, s, tracking);
    let x0 = match align {
        Align::Left => x,
        Align::Center => x - w / 2.0,
        Align::Right => x - w,
    };
    let (vs, _, _) = view();
    let scale = PxScale::from(size * vs);
    let sc = font.as_scaled(scale);
    let (tracking, base_y) = (tracking * vs, ty(base_y));
    let (pw, ph) = (pm.width() as i32, pm.height() as i32);
    let rgb = rgb255(color);
    let ca = color.alpha();
    let data = pm.data_mut();
    let mut caret = tx(x0);
    let mut prev = None;
    for ch in s.chars() {
        let id = sc.glyph_id(ch);
        if let Some(p) = prev {
            caret += sc.kern(p, id);
        }
        let g = id.with_scale_and_position(scale, point(caret, base_y));
        if let Some(og) = font.outline_glyph(g) {
            let b = og.px_bounds();
            let (bx, by) = (b.min.x as i32, b.min.y as i32);
            og.draw(|gx, gy, cov| {
                let (px, py) = (bx + gx as i32, by + gy as i32);
                if px >= 0 && py >= 0 && px < pw && py < ph {
                    let i = ((py * pw + px) * 4) as usize;
                    blend_px(&mut data[i..i + 4], rgb, cov * ca);
                }
            });
        }
        caret += sc.h_advance(id) + tracking;
        prev = Some(id);
    }
    w
}

/// Largest size <= `size` (down to `min`) at which `s` fits in `max_w`.
pub fn fit_size(font: &FontRef, size: f32, min: f32, s: &str, max_w: f32, tracking: f32) -> f32 {
    let w = text_width(font, size, s, tracking);
    if w <= max_w || w <= 0.0 {
        size
    } else {
        (size * max_w / w).max(min)
    }
}

/// Truncates with an ellipsis so that it fits `max_w`.
pub fn clip_text(font: &FontRef, size: f32, s: &str, max_w: f32, tracking: f32) -> String {
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

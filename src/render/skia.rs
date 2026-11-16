//! CPU backend of [`Canvas`]: tiny-skia for vectors, ab_glyph for glyphs.
//!
//! A [`SkiaStore`] owns the offscreen layers and the view; a short-lived
//! [`SkiaCanvas`] pairs it with the target `Pixmap` of one frame.

use std::sync::atomic::{AtomicU64, Ordering};

use ab_glyph::{point, Font, PxScale, ScaleFont};
use tiny_skia as sk;

use super::canvas::{Canvas, Color, Cov, CovRow, FontFace, LayerFilter, LayerId, Path, Seg};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Persistent state of the CPU backend (layers, view).
pub struct SkiaStore {
    id: u64,
    view: (f32, f32, f32),
    layers: Vec<Option<sk::Pixmap>>,
}

impl Default for SkiaStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SkiaStore {
    pub fn new() -> Self {
        SkiaStore { id: NEXT_ID.fetch_add(1, Ordering::Relaxed), view: (1.0, 0.0, 0.0), layers: Vec::new() }
    }

    /// Canvas drawing into `target` (the main surface).
    pub fn canvas<'a>(&'a mut self, target: &'a mut sk::Pixmap) -> SkiaCanvas<'a> {
        SkiaCanvas { st: self, main: target, stack: Vec::new() }
    }
}

pub struct SkiaCanvas<'a> {
    st: &'a mut SkiaStore,
    main: &'a mut sk::Pixmap,
    /// Active layer targets (innermost last); empty = the main surface.
    stack: Vec<usize>,
}

fn sk_color(c: Color) -> sk::Color {
    sk::Color::from_rgba(c.red(), c.green(), c.blue(), c.alpha()).unwrap_or(sk::Color::BLACK)
}

fn sk_paint(c: Color) -> sk::Paint<'static> {
    let mut p = sk::Paint::default();
    p.set_color(sk_color(c));
    p.anti_alias = true;
    p
}

fn sk_path(p: &Path) -> Option<sk::Path> {
    let n = p.segments().len();
    let mut pb = sk::PathBuilder::with_capacity(n, n * 2);
    for s in p.segments() {
        match *s {
            Seg::MoveTo(x, y) => pb.move_to(x, y),
            Seg::LineTo(x, y) => pb.line_to(x, y),
            Seg::QuadTo(x1, y1, x, y) => pb.quad_to(x1, y1, x, y),
            Seg::Close => pb.close(),
            Seg::Circle(cx, cy, r) => pb.push_circle(cx, cy, r),
        }
    }
    pb.finish()
}

#[inline]
fn blend_px(d: &mut [u8], c: [f32; 3], a: f32) {
    if a <= 0.0 {
        return;
    }
    let ia = 1.0 - a;
    d[0] = (c[0] * a + d[0] as f32 * ia) as u8;
    d[1] = (c[1] * a + d[1] as f32 * ia) as u8;
    d[2] = (c[2] * a + d[2] as f32 * ia) as u8;
    d[3] = (255.0 * a + d[3] as f32 * ia) as u8;
}

impl SkiaCanvas<'_> {
    fn tr(&self) -> sk::Transform {
        let (s, ox, oy) = self.st.view;
        if s == 1.0 && ox == 0.0 && oy == 0.0 {
            sk::Transform::identity()
        } else {
            sk::Transform::from_row(s, 0.0, 0.0, s, ox, oy)
        }
    }

    fn target(&mut self) -> &mut sk::Pixmap {
        match self.stack.last() {
            Some(&i) => self.st.layers[i].as_mut().expect("live layer"),
            None => self.main,
        }
    }
}

impl Canvas for SkiaCanvas<'_> {
    fn instance_id(&self) -> u64 {
        self.st.id
    }

    fn size(&self) -> (u32, u32) {
        match self.stack.last() {
            Some(&i) => {
                let p = self.st.layers[i].as_ref().expect("live layer");
                (p.width(), p.height())
            }
            None => (self.main.width(), self.main.height()),
        }
    }

    fn view(&self) -> (f32, f32, f32) {
        self.st.view
    }

    fn set_view(&mut self, s: f32, ox: f32, oy: f32) {
        self.st.view = (s, ox, oy);
    }

    fn fill_path(&mut self, p: &Path, c: Color) {
        let Some(p) = sk_path(p) else { return };
        let t = self.tr();
        self.target().fill_path(&p, &sk_paint(c), sk::FillRule::Winding, t, None);
    }

    fn stroke_path(&mut self, p: &Path, c: Color, w: f32) {
        let Some(p) = sk_path(p) else { return };
        let s = sk::Stroke { width: w, ..sk::Stroke::default() };
        let t = self.tr();
        self.target().stroke_path(&p, &sk_paint(c), &s, t, None);
    }

    fn fill_rect(&mut self, x: f32, y: f32, w: f32, h: f32, c: Color, aa: bool) {
        if let Some(r) = sk::Rect::from_xywh(x, y, w, h) {
            let mut p = sk_paint(c);
            p.anti_alias = aa;
            let t = self.tr();
            self.target().fill_rect(r, &p, t, None);
        }
    }

    fn overlay(&mut self, c: Color) {
        let pm = self.target();
        let r = sk::Rect::from_xywh(0.0, 0.0, pm.width() as f32, pm.height() as f32).unwrap();
        let mut p = sk_paint(c);
        p.anti_alias = false;
        pm.fill_rect(r, &p, sk::Transform::identity(), None);
    }

    fn clear(&mut self, c: Color) {
        self.target().fill(sk_color(c));
    }

    fn text_run(&mut self, font: &FontFace, size: f32, x: f32, base_y: f32, s: &str, color: Color, tracking: f32) {
        let (vs, ox, oy) = self.st.view;
        let scale = PxScale::from(size * vs);
        let sc = font.as_scaled(scale);
        let (tracking, base_y) = (tracking * vs, base_y * vs + oy);
        let pm = self.target();
        let (pw, ph) = (pm.width() as i32, pm.height() as i32);
        let rgb = [color.red() * 255.0, color.green() * 255.0, color.blue() * 255.0];
        let ca = color.alpha();
        let data = pm.data_mut();
        let mut caret = x * vs + ox;
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
    }

    fn blit_cov(&mut self, cov: &Cov, x: i32, y: i32, rgb: [f32; 3], a: f32, clip: Option<(i32, i32, i32, i32)>) {
        let pm = self.target();
        let (pw, ph) = (pm.width() as i32, pm.height() as i32);
        let (cx0, cy0, cx1, cy1) = clip.unwrap_or((i32::MIN, i32::MIN, i32::MAX, i32::MAX));
        let data = pm.data_mut();
        for yy in 0..cov.h {
            let py = y + yy as i32;
            if py < cy0 || py >= cy1 || py < 0 || py >= ph {
                continue;
            }
            for xx in 0..cov.w {
                let px = x + xx as i32;
                if px < cx0 || px >= cx1 || px < 0 || px >= pw {
                    continue;
                }
                let c = cov.data[yy * cov.w + xx];
                if c == 0 {
                    continue;
                }
                let i = ((py * pw + px) * 4) as usize;
                blend_px(&mut data[i..i + 4], rgb, c as f32 / 255.0 * a);
            }
        }
    }

    fn blit_cov_rows(&mut self, cov: &Cov, x: i32, rows: &[CovRow]) {
        let pm = self.target();
        let (pw, ph) = (pm.width() as i32, pm.height() as i32);
        let data = pm.data_mut();
        for r in rows {
            if r.a <= 0.0 || r.dst_y < 0 || r.dst_y >= ph {
                continue;
            }
            for col in 0..cov.w {
                let c = cov.data[r.src_row * cov.w + col];
                if c == 0 {
                    continue;
                }
                let px = x + col as i32;
                if px < 0 || px >= pw {
                    continue;
                }
                let i = ((r.dst_y * pw + px) * 4) as usize;
                blend_px(&mut data[i..i + 4], r.rgb, c as f32 / 255.0 * r.a);
            }
        }
    }

    fn create_layer(&mut self, w: u32, h: u32) -> LayerId {
        let pm = sk::Pixmap::new(w, h).expect("layer size");
        match self.st.layers.iter().position(Option::is_none) {
            Some(i) => {
                self.st.layers[i] = Some(pm);
                LayerId(i as u32)
            }
            None => {
                self.st.layers.push(Some(pm));
                LayerId(self.st.layers.len() as u32 - 1)
            }
        }
    }

    fn drop_layer(&mut self, id: LayerId) {
        self.st.layers[id.0 as usize] = None;
    }

    fn begin_layer(&mut self, id: LayerId) {
        self.stack.push(id.0 as usize);
    }

    fn end_layer(&mut self) {
        self.stack.pop();
    }

    fn layer_copy_in(&mut self, id: LayerId) {
        let src = self.st.layers[id.0 as usize].take().expect("live layer");
        self.target().data_mut().copy_from_slice(src.data());
        self.st.layers[id.0 as usize] = Some(src);
    }

    fn layer_capture(&mut self, id: LayerId) {
        let mut dst = self.st.layers[id.0 as usize].take().expect("live layer");
        dst.data_mut().copy_from_slice(self.target().data());
        self.st.layers[id.0 as usize] = Some(dst);
    }

    fn layer_blit(&mut self, id: LayerId, x: i32, y: i32) {
        let src = self.st.layers[id.0 as usize].take().expect("live layer");
        self.target().draw_pixmap(x, y, src.as_ref(), &sk::PixmapPaint::default(), sk::Transform::identity(), None);
        self.st.layers[id.0 as usize] = Some(src);
    }

    fn layer_filter(&mut self, id: LayerId, f: LayerFilter) {
        let pm = self.st.layers[id.0 as usize].as_mut().expect("live layer");
        match f {
            LayerFilter::Dim { keep, greyw, gain } => {
                for px in pm.data_mut().chunks_exact_mut(4) {
                    let g = px[0] as f32 * 0.30 + px[1] as f32 * 0.59 + px[2] as f32 * 0.11;
                    for c in &mut px[..3] {
                        *c = ((*c as f32 * keep + g * greyw) * gain) as u8;
                    }
                    px[3] = (px[3] as f32 * gain) as u8;
                }
            }
        }
    }
}

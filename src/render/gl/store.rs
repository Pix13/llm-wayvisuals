//! [`Canvas`] on femtovg (OpenGL). Every target (the frame, the layers) is an
//! RGBA image used as a render target, so layers can be sampled and copied
//! with plain draws. Colours are premultiplied everywhere, like the CPU one.

use std::collections::HashMap;
use std::rc::Rc;

use femtovg::imgref::ImgRef;
use femtovg::renderer::OpenGl;
use femtovg::rgb::{FromSlice, RGBA8};
use femtovg::{
    CompositeOperation, DrawCommand, FillRule, GlyphDrawCommands, ImageFlags, ImageId, ImageSource, LineCap, LineJoin, Paint, PixelFormat,
    Quad, RenderTarget, Transform2D,
};
use glow::HasContext;

use crate::render::canvas::{Canvas, Color, Cov, CovRow, FontFace, FontId, LayerFilter, LayerId, Path, Seg};

use ab_glyph::{point, Font, PxScale, ScaleFont};

static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1 << 40);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Target {
    Frame,
    Layer(usize),
}

struct Layer {
    id: ImageId,
    w: u32,
    h: u32,
}

/// Sub-pixel positions of a glyph are quantised to 1/SUBPX px.
const SUBPX: f32 = 16.0;
const ATLAS_SIZE: u32 = 2048;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct GlyphKey {
    font: FontId,
    gid: u16,
    size_bits: u32,
    fx: u8,
    fy: u8,
}

/// A rasterised glyph in the atlas (None = nothing to draw, e.g. a space).
#[derive(Clone, Copy)]
struct GlyphSlot {
    /// Offset of the bitmap's top-left from the integer pen position.
    ox: i32,
    oy: i32,
    w: u32,
    h: u32,
    ax: u32,
    ay: u32,
}

/// Shelf-packed Gray8 atlas of glyph coverage bitmaps rasterised with
/// ab_glyph (same rasteriser and sub-pixel placement as the CPU backend).
struct GlyphAtlas {
    img: ImageId,
    map: HashMap<GlyphKey, Option<GlyphSlot>>,
    x: u32,
    y: u32,
    row_h: u32,
    full: bool,
}

struct CovEntry {
    id: ImageId,
    last_used: u64,
}

/// Max live coverage textures (glyph caches are small, digit masks are big).
const COV_CACHE_MAX: usize = 768;
/// Frames a coverage texture survives without being drawn.
const COV_CACHE_TTL: u64 = 240;

pub struct GlStore {
    id: u64,
    pub(super) gl: Rc<glow::Context>,
    pub(super) cv: femtovg::Canvas<OpenGl>,
    view: (f32, f32, f32),
    glyphs: Option<GlyphAtlas>,
    layers: Vec<Option<Layer>>,
    /// Frame images by band index; `cur_frame` is the one being drawn.
    frames: Vec<Option<Layer>>,
    cur_frame: usize,
    stack: Vec<Target>,
    cov: HashMap<(u32, u32, u64), CovEntry>,
    frame_no: u64,
}

fn fc(c: Color) -> femtovg::Color {
    femtovg::Color::rgbaf(c.red(), c.green(), c.blue(), c.alpha())
}

fn fc_rgb(rgb: [f32; 3], a: f32) -> femtovg::Color {
    femtovg::Color::rgbaf((rgb[0] / 255.0).clamp(0.0, 1.0), (rgb[1] / 255.0).clamp(0.0, 1.0), (rgb[2] / 255.0).clamp(0.0, 1.0), a.clamp(0.0, 1.0))
}

fn fnv(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut chunks = data.chunks_exact(8);
    for c in &mut chunks {
        h = (h ^ u64::from_le_bytes(c.try_into().unwrap())).wrapping_mul(0x0000_0100_0000_01b3);
        h ^= h >> 29;
    }
    for &b in chunks.remainder() {
        h = (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

impl GlStore {
    pub(super) fn new(gl: Rc<glow::Context>, renderer: OpenGl) -> Result<Self, String> {
        let cv = femtovg::Canvas::new(renderer).map_err(|e| format!("femtovg: {e}"))?;
        Ok(GlStore {
            id: NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            gl,
            cv,
            view: (1.0, 0.0, 0.0),
            glyphs: None,
            layers: Vec::new(),
            frames: Vec::new(),
            cur_frame: 0,
            stack: Vec::new(),
            cov: HashMap::new(),
            frame_no: 0,
        })
    }

    // ---- frame management (called by `GpuRenderer`) --------------------------------

    /// Starts (or continues, for another band of the same surface frame) a
    /// frame of `w` x `h` pixels for band `band`: (re)allocates its image and
    /// directs drawing to it.
    pub(super) fn begin_frame(&mut self, band: usize, w: u32, h: u32) {
        if band == 0 {
            self.frame_no += 1;
            if self.glyphs.as_ref().is_some_and(|a| a.full) {
                // The previous frame ran out of room: start over (all its draws are submitted).
                if let Some(a) = self.glyphs.take() {
                    self.cv.delete_image(a.img);
                }
            }
        }
        self.stack.clear();
        self.cv.set_size(w, h, 1.0);
        if self.frames.len() <= band {
            self.frames.resize_with(band + 1, || None);
        }
        if self.frames[band].as_ref().is_none_or(|f| (f.w, f.h) != (w, h)) {
            if let Some(f) = self.frames[band].take() {
                self.cv.delete_image(f.id);
            }
            self.frames[band] = Some(self.alloc_image(w, h));
        }
        self.cur_frame = band;
        self.select(Target::Frame);
        self.cv.reset_scissor();
        self.cv.reset_transform();
    }

    /// Submits the recorded commands to the GPU.
    pub(super) fn flush(&mut self) {
        self.evict_cov();
        self.cv.flush();
    }

    pub(super) fn frame_image(&self, band: usize) -> Option<(ImageId, u32, u32)> {
        self.frames.get(band)?.as_ref().map(|f| (f.id, f.w, f.h))
    }

    fn cur_frame_layer(&self) -> &Layer {
        self.frames[self.cur_frame].as_ref().expect("frame")
    }

    /// Top-down premultiplied RGBA of the frame (flushes first).
    pub(super) fn read_frame(&mut self, band: usize) -> Option<(u32, u32, Vec<u8>)> {
        let (id, w, h) = self.frame_image(band)?;
        self.flush();
        let mut px = self.read_image(id, w, h)?;
        flip_rows(&mut px, w as usize, h as usize);
        Some((w, h, px))
    }

    fn alloc_image(&mut self, w: u32, h: u32) -> Layer {
        let id = self
            .cv
            .create_image_empty(w as usize, h as usize, PixelFormat::Rgba8, ImageFlags::FLIP_Y | ImageFlags::PREMULTIPLIED | ImageFlags::NEAREST)
            .expect("image alloc");
        Layer { id, w, h }
    }

    /// Raw GL rows (bottom-up) of an image, via a scratch framebuffer.
    fn read_image(&mut self, id: ImageId, w: u32, h: u32) -> Option<Vec<u8>> {
        let tex = self.cv.get_native_texture(id).ok()?;
        let gl = &self.gl;
        let mut out = vec![0u8; (w * h * 4) as usize];
        unsafe {
            let fbo = gl.create_framebuffer().ok()?;
            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(fbo));
            gl.framebuffer_texture_2d(glow::READ_FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, Some(tex), 0);
            gl.pixel_store_i32(glow::PACK_ALIGNMENT, 1);
            gl.read_pixels(0, 0, w as i32, h as i32, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelPackData::Slice(Some(&mut out)));
            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, None);
            gl.delete_framebuffer(fbo);
        }
        Some(out)
    }

    /// Composes the band images on the window's default framebuffer of `sw` x
    /// `sh` pixels: black background, each `(band, x, y)` image copied at its
    /// position (top-left origin). Submits the commands.
    pub(super) fn present_to_screen(&mut self, bands: &[(usize, i32, i32)], sw: u32, sh: u32) {
        self.cv.set_size(sw, sh, 1.0);
        self.cv.set_render_target(RenderTarget::Screen);
        self.cv.reset_transform();
        self.cv.reset_scissor();
        self.cv.clear_rect(0, 0, sw, sh, femtovg::Color::rgba(0, 0, 0, 255));
        for &(band, x, y) in bands {
            if let Some((id, w, h)) = self.frame_image(band) {
                self.draw_image(id, x, y, w, h, CompositeOperation::Copy);
            }
        }
        self.flush();
    }

    // ---- internals ----------------------------------------------------------------------

    fn select(&mut self, t: Target) {
        let id = match t {
            Target::Frame => self.cur_frame_layer().id,
            Target::Layer(i) => self.layers[i].as_ref().expect("live layer").id,
        };
        self.cv.set_render_target(RenderTarget::Image(id));
    }

    fn cur(&self) -> Target {
        self.stack.last().copied().unwrap_or(Target::Frame)
    }

    fn cur_size(&self) -> (u32, u32) {
        match self.cur() {
            Target::Frame => self.frames.get(self.cur_frame).and_then(Option::as_ref).map_or((0, 0), |f| (f.w, f.h)),
            Target::Layer(i) => self.layers[i].as_ref().map_or((0, 0), |l| (l.w, l.h)),
        }
    }

    fn set_view_transform(&mut self) {
        let (s, ox, oy) = self.view;
        self.cv.set_transform(&Transform2D::new(s, 0.0, 0.0, s, ox, oy));
    }

    fn fe_path(p: &Path) -> femtovg::Path {
        let mut out = femtovg::Path::new();
        for s in p.segments() {
            match *s {
                Seg::MoveTo(x, y) => out.move_to(x, y),
                Seg::LineTo(x, y) => out.line_to(x, y),
                Seg::QuadTo(x1, y1, x, y) => out.quad_to(x1, y1, x, y),
                Seg::Close => out.close(),
                Seg::Circle(cx, cy, r) => out.circle(cx, cy, r),
            }
        }
        out
    }

    /// Slot of a glyph variant, rasterising it into the atlas on first use.
    /// `None` when the glyph has no outline or the atlas is full.
    fn glyph_slot(&mut self, font: &FontFace, key: GlyphKey, scale: PxScale) -> Option<GlyphSlot> {
        if self.glyphs.is_none() {
            let img = self
                .cv
                .create_image_empty(ATLAS_SIZE as usize, ATLAS_SIZE as usize, PixelFormat::Gray8, ImageFlags::NEAREST)
                .ok()?;
            self.glyphs = Some(GlyphAtlas { img, map: HashMap::new(), x: 0, y: 0, row_h: 0, full: false });
        }
        if let Some(s) = self.glyphs.as_ref()?.map.get(&key) {
            return *s;
        }
        let g = ab_glyph::GlyphId(key.gid).with_scale_and_position(scale, point(key.fx as f32 / SUBPX, key.fy as f32 / SUBPX));
        let slot = match font.outline_glyph(g) {
            None => None,
            Some(og) => {
                let b = og.px_bounds();
                let (bx, by) = (b.min.x as i32, b.min.y as i32);
                let (w, h) = (((b.max.x - b.min.x) as u32).max(1), ((b.max.y - b.min.y) as u32).max(1));
                let mut bmp = vec![0u8; (w * h) as usize];
                og.draw(|x, y, c| {
                    if x < w && y < h {
                        bmp[(y * w + x) as usize] = (c * 255.0 + 0.5) as u8;
                    }
                });
                let atlas = self.glyphs.as_mut()?;
                // shelf allocation (1 px gap against bleeding)
                if atlas.x + w + 1 > ATLAS_SIZE {
                    atlas.x = 0;
                    atlas.y += atlas.row_h + 1;
                    atlas.row_h = 0;
                }
                if atlas.y + h + 1 > ATLAS_SIZE || w + 1 > ATLAS_SIZE {
                    atlas.full = true;
                    return None;
                }
                let (ax, ay) = (atlas.x, atlas.y);
                atlas.x += w + 1;
                atlas.row_h = atlas.row_h.max(h);
                let img = atlas.img;
                let src = ImgRef::new(bmp.as_gray(), w as usize, h as usize);
                let _ = self.cv.update_image(img, ImageSource::Gray(src), ax as usize, ay as usize);
                Some(GlyphSlot { ox: bx, oy: by, w, h, ax, ay })
            }
        };
        self.glyphs.as_mut()?.map.insert(key, slot);
        slot
    }

    /// Texture of a coverage bitmap, uploaded once per distinct content.
    fn cov_image(&mut self, cov: &Cov) -> Option<ImageId> {
        if cov.w == 0 || cov.h == 0 || cov.data.len() < cov.w * cov.h {
            return None;
        }
        let data = &cov.data[..cov.w * cov.h];
        let key = (cov.w as u32, cov.h as u32, fnv(data));
        if let Some(e) = self.cov.get_mut(&key) {
            e.last_used = self.frame_no;
            return Some(e.id);
        }
        let img = ImgRef::new(data.as_gray(), cov.w, cov.h);
        let id = self.cv.create_image(ImageSource::Gray(img), ImageFlags::NEAREST).ok()?;
        self.cov.insert(key, CovEntry { id, last_used: self.frame_no });
        Some(id)
    }

    fn evict_cov(&mut self) {
        let now = self.frame_no;
        let over = self.cov.len() > COV_CACHE_MAX;
        let stale: Vec<_> = self
            .cov
            .iter()
            .filter(|(_, e)| now.saturating_sub(e.last_used) > if over { 2 } else { COV_CACHE_TTL })
            .map(|(k, _)| *k)
            .collect();
        for k in stale {
            if let Some(e) = self.cov.remove(&k) {
                self.cv.delete_image(e.id);
            }
        }
    }

    fn rect_path(x: f32, y: f32, w: f32, h: f32) -> femtovg::Path {
        let mut p = femtovg::Path::new();
        p.rect(x, y, w, h);
        p
    }

    /// Draws image `id` (target-sized rect at `(x, y)`) with `op`, pixel space.
    fn draw_image(&mut self, id: ImageId, x: i32, y: i32, w: u32, h: u32, op: CompositeOperation) {
        self.cv.save();
        self.cv.reset_transform();
        self.cv.global_composite_operation(op);
        let paint = Paint::image(id, x as f32, y as f32, w as f32, h as f32, 0.0, 1.0).with_anti_alias(false);
        self.cv.fill_path(&Self::rect_path(x as f32, y as f32, w as f32, h as f32), &paint);
        self.cv.restore();
    }

    fn layer_ref(&self, id: LayerId) -> (ImageId, u32, u32) {
        let l = self.layers[id.0 as usize].as_ref().expect("live layer");
        (l.id, l.w, l.h)
    }
}

fn flip_rows(px: &mut [u8], w: usize, h: usize) {
    let stride = w * 4;
    for y in 0..h / 2 {
        let (a, b) = px.split_at_mut((h - 1 - y) * stride);
        a[y * stride..(y + 1) * stride].swap_with_slice(&mut b[..stride]);
    }
}

impl Canvas for GlStore {
    fn instance_id(&self) -> u64 {
        self.id
    }

    fn size(&self) -> (u32, u32) {
        self.cur_size()
    }

    fn view(&self) -> (f32, f32, f32) {
        self.view
    }

    fn set_view(&mut self, s: f32, ox: f32, oy: f32) {
        self.view = (s, ox, oy);
    }

    fn fill_path(&mut self, p: &Path, c: Color) {
        self.set_view_transform();
        let mut paint = Paint::color(fc(c)).with_anti_alias(true);
        paint.set_fill_rule(FillRule::NonZero);
        self.cv.fill_path(&Self::fe_path(p), &paint);
    }

    fn stroke_path(&mut self, p: &Path, c: Color, w: f32) {
        self.set_view_transform();
        let mut paint = Paint::color(fc(c)).with_anti_alias(true);
        paint.set_line_width(w);
        paint.set_line_cap(LineCap::Butt);
        paint.set_line_join(LineJoin::Miter);
        paint.set_miter_limit(4.0);
        self.cv.stroke_path(&Self::fe_path(p), &paint);
    }

    fn fill_rect(&mut self, x: f32, y: f32, w: f32, h: f32, c: Color, aa: bool) {
        if !(w > 0.0 && h > 0.0 && x.is_finite() && y.is_finite() && w.is_finite() && h.is_finite()) {
            return;
        }
        if aa {
            self.set_view_transform();
            let paint = Paint::color(fc(c)).with_anti_alias(true);
            self.cv.fill_path(&Self::rect_path(x, y, w, h), &paint);
        } else {
            // Non-AA fill: tiny-skia's non-AA rectangle truncates its edges to
            // whole pixels (floor, both sides); the GPU would sample centres.
            let (s, ox, oy) = self.view;
            let r = |v: f32| v.floor();
            let (x0, x1) = (r(x * s + ox), r((x + w) * s + ox));
            let (y0, y1) = (r(y * s + oy), r((y + h) * s + oy));
            if x1 > x0 && y1 > y0 {
                self.cv.reset_transform();
                let paint = Paint::color(fc(c)).with_anti_alias(false);
                self.cv.fill_path(&Self::rect_path(x0, y0, x1 - x0, y1 - y0), &paint);
            }
        }
    }

    fn overlay(&mut self, c: Color) {
        let (w, h) = self.cur_size();
        self.cv.reset_transform();
        let paint = Paint::color(fc(c)).with_anti_alias(false);
        self.cv.fill_path(&Self::rect_path(0.0, 0.0, w as f32, h as f32), &paint);
    }

    fn clear(&mut self, c: Color) {
        let (w, h) = self.cur_size();
        self.cv.reset_scissor();
        self.cv.clear_rect(0, 0, w, h, fc(c));
    }

    fn text_run(&mut self, font: &FontFace, size: f32, x: f32, base_y: f32, s: &str, color: Color, tracking: f32) {
        let (vs, ox, oy) = self.view;
        let scale = PxScale::from(size * vs);
        let sc = font.as_scaled(scale);
        let (tracking, base_y) = (tracking * vs, base_y * vs + oy);
        let mut caret = x * vs + ox;
        let mut prev = None;
        let iy0 = base_y.floor();
        let mut qy = ((base_y - iy0) * SUBPX).round() as i32;
        let mut iy = iy0 as i32;
        if qy >= SUBPX as i32 {
            qy = 0;
            iy += 1;
        }
        let mut quads = Vec::with_capacity(s.len());
        let inv = 1.0 / ATLAS_SIZE as f32;
        for ch in s.chars() {
            let id = sc.glyph_id(ch);
            if let Some(p) = prev {
                caret += sc.kern(p, id);
            }
            if ch != ' ' && caret.is_finite() {
                let ic = caret.floor();
                let mut qx = ((caret - ic) * SUBPX).round() as i32;
                let mut ix = ic as i32;
                if qx >= SUBPX as i32 {
                    qx = 0;
                    ix += 1;
                }
                let key = GlyphKey { font: font.id, gid: id.0, size_bits: scale.y.to_bits(), fx: qx as u8, fy: qy as u8 };
                if let Some(g) = self.glyph_slot(font, key, scale) {
                    let (x0, y0) = ((ix + g.ox) as f32, (iy + g.oy) as f32);
                    quads.push(Quad {
                        x0,
                        y0,
                        s0: g.ax as f32 * inv,
                        t0: g.ay as f32 * inv,
                        x1: x0 + g.w as f32,
                        y1: y0 + g.h as f32,
                        s1: (g.ax + g.w) as f32 * inv,
                        t1: (g.ay + g.h) as f32 * inv,
                    });
                }
            }
            caret += sc.h_advance(id) + tracking;
            prev = Some(id);
        }
        let Some(atlas) = self.glyphs.as_ref().map(|a| a.img) else { return };
        if quads.is_empty() {
            return;
        }
        self.cv.reset_transform();
        let paint = Paint::color(fc(color));
        self.cv.draw_glyph_commands(
            GlyphDrawCommands { alpha_glyphs: vec![DrawCommand { image_id: atlas, quads }], color_glyphs: Vec::new() },
            &paint,
        );
    }

    fn blit_cov(&mut self, cov: &Cov, x: i32, y: i32, rgb: [f32; 3], a: f32, clip: Option<(i32, i32, i32, i32)>) {
        if a <= 0.0 {
            return;
        }
        let Some(id) = self.cov_image(cov) else { return };
        self.cv.save();
        self.cv.reset_transform();
        if let Some((x0, y0, x1, y1)) = clip {
            if x1 <= x0 || y1 <= y0 {
                self.cv.restore();
                return;
            }
            self.cv.scissor(x0 as f32, y0 as f32, (x1 - x0) as f32, (y1 - y0) as f32);
        }
        let (w, h) = (cov.w as f32, cov.h as f32);
        let paint = Paint::image_tint(id, x as f32, y as f32, w, h, 0.0, fc_rgb(rgb, a)).with_anti_alias(false);
        self.cv.fill_path(&Self::rect_path(x as f32, y as f32, w, h), &paint);
        self.cv.restore();
    }

    fn blit_cov_rows(&mut self, cov: &Cov, x: i32, rows: &[CovRow]) {
        let Some(id) = self.cov_image(cov) else { return };
        self.cv.save();
        self.cv.reset_transform();
        let (w, h) = (cov.w as f32, cov.h as f32);
        for r in rows {
            if r.a <= 0.0 || r.src_row >= cov.h {
                continue;
            }
            // the image is positioned so that its row `src_row` lands on `dst_y`
            let top = (r.dst_y - r.src_row as i32) as f32;
            let paint = Paint::image_tint(id, x as f32, top, w, h, 0.0, fc_rgb(r.rgb, r.a)).with_anti_alias(false);
            self.cv.fill_path(&Self::rect_path(x as f32, r.dst_y as f32, w, 1.0), &paint);
        }
        self.cv.restore();
    }

    fn create_layer(&mut self, w: u32, h: u32) -> LayerId {
        let layer = self.alloc_image(w, h);
        // starts transparent (like a fresh Pixmap)
        let prev = self.stack.clone();
        self.cv.set_render_target(RenderTarget::Image(layer.id));
        self.cv.reset_scissor();
        self.cv.clear_rect(0, 0, w, h, femtovg::Color::rgba(0, 0, 0, 0));
        match prev.last() {
            Some(&t) => self.select(t),
            None => self.select(Target::Frame),
        }
        match self.layers.iter().position(Option::is_none) {
            Some(i) => {
                self.layers[i] = Some(layer);
                LayerId(i as u32)
            }
            None => {
                self.layers.push(Some(layer));
                LayerId(self.layers.len() as u32 - 1)
            }
        }
    }

    fn drop_layer(&mut self, id: LayerId) {
        if let Some(l) = self.layers[id.0 as usize].take() {
            self.cv.delete_image(l.id);
        }
    }

    fn begin_layer(&mut self, id: LayerId) {
        self.stack.push(Target::Layer(id.0 as usize));
        self.select(Target::Layer(id.0 as usize));
    }

    fn end_layer(&mut self) {
        self.stack.pop();
        let t = self.cur();
        self.select(t);
    }

    fn layer_copy_in(&mut self, id: LayerId) {
        let (img, w, h) = self.layer_ref(id);
        self.draw_image(img, 0, 0, w, h, CompositeOperation::Copy);
    }

    fn layer_capture(&mut self, id: LayerId) {
        let (dst, w, h) = self.layer_ref(id);
        let src = match self.cur() {
            Target::Frame => self.cur_frame_layer().id,
            Target::Layer(i) => self.layers[i].as_ref().expect("live layer").id,
        };
        let cur = self.cur();
        self.cv.set_render_target(RenderTarget::Image(dst));
        self.draw_image(src, 0, 0, w, h, CompositeOperation::Copy);
        self.select(cur);
    }

    fn layer_blit(&mut self, id: LayerId, x: i32, y: i32) {
        let (img, w, h) = self.layer_ref(id);
        self.draw_image(img, x, y, w, h, CompositeOperation::SourceOver);
    }

    fn layer_filter(&mut self, id: LayerId, f: LayerFilter) {
        // Per-pixel cross-channel maths: read back, filter on the CPU, upload.
        // Only runs when the (cached) non-live composite is rebuilt.
        let (img, w, h) = self.layer_ref(id);
        self.cv.flush();
        let Some(mut px) = self.read_image(img, w, h) else { return };
        match f {
            LayerFilter::Dim { keep, greyw, gain } => {
                for p in px.chunks_exact_mut(4) {
                    let g = p[0] as f32 * 0.30 + p[1] as f32 * 0.59 + p[2] as f32 * 0.11;
                    for c in &mut p[..3] {
                        *c = ((*c as f32 * keep + g * greyw) * gain) as u8;
                    }
                    p[3] = (p[3] as f32 * gain) as u8;
                }
            }
        }
        let src = ImgRef::new(px.as_rgba() as &[RGBA8], w as usize, h as usize);
        let _ = self.cv.update_image(img, ImageSource::Rgba(src), 0, 0);
    }
}

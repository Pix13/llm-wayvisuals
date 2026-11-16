//! Backend-neutral drawing interface of the renderer.
//!
//! The scene / effects code only talks to [`Canvas`] and to the plain data
//! types defined here ([`Color`], [`Path`], [`PathBuilder`], [`Cov`],
//! [`FontFace`]). A backend (see `skia.rs`, the CPU one) implements the trait.
//!
//! Coordinates: every `f32` position / size of the vector operations
//! (`fill_path`, `stroke_path`, `fill_rect`, `text_run`) is in *design* units;
//! the backend maps it to pixels with the view set by [`Canvas::set_view`]
//! (`pixel = design * s + (ox, oy)`). The coverage blits ([`Canvas::blit_cov`],
//! [`Canvas::blit_cov_rows`]) and the layer operations are in *pixel* units.

use ab_glyph::FontRef;

// ---- colour ----------------------------------------------------------------

/// Straight (non-premultiplied) RGBA, every component in 0..=1.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Color {
    r: f32,
    g: f32,
    b: f32,
    a: f32,
}

impl Color {
    pub const TRANSPARENT: Color = Color { r: 0.0, g: 0.0, b: 0.0, a: 0.0 };
    pub const BLACK: Color = Color { r: 0.0, g: 0.0, b: 0.0, a: 1.0 };
    pub const WHITE: Color = Color { r: 1.0, g: 1.0, b: 1.0, a: 1.0 };

    /// `None` when a component is outside 0..=1 (or NaN).
    pub fn from_rgba(r: f32, g: f32, b: f32, a: f32) -> Option<Color> {
        let ok = |v: f32| (0.0..=1.0).contains(&v);
        (ok(r) && ok(g) && ok(b) && ok(a)).then_some(Color { r, g, b, a })
    }

    pub fn from_rgba8(r: u8, g: u8, b: u8, a: u8) -> Color {
        Color { r: r as f32 / 255.0, g: g as f32 / 255.0, b: b as f32 / 255.0, a: a as f32 / 255.0 }
    }

    pub fn red(&self) -> f32 {
        self.r
    }
    pub fn green(&self) -> f32 {
        self.g
    }
    pub fn blue(&self) -> f32 {
        self.b
    }
    pub fn alpha(&self) -> f32 {
        self.a
    }

    /// Clamps to 0..=1.
    pub fn set_alpha(&mut self, a: f32) {
        self.a = a.clamp(0.0, 1.0);
    }
}

// ---- paths -------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Seg {
    MoveTo(f32, f32),
    LineTo(f32, f32),
    /// control point, end point
    QuadTo(f32, f32, f32, f32),
    Close,
    /// Full circle: centre, radius (a closed sub-path of its own).
    Circle(f32, f32, f32),
}

/// An immutable list of segments (design units).
#[derive(Debug, Clone, PartialEq)]
pub struct Path {
    segs: Vec<Seg>,
}

impl Path {
    pub fn segments(&self) -> &[Seg] {
        &self.segs
    }
}

/// Same calling convention as `tiny_skia::PathBuilder` for the subset the
/// scene uses. `finish()` is `None` for an empty / degenerate path (a lone
/// `move_to`) or a non-finite coordinate.
#[derive(Debug, Default, Clone)]
pub struct PathBuilder {
    segs: Vec<Seg>,
}

impl PathBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn move_to(&mut self, x: f32, y: f32) {
        self.segs.push(Seg::MoveTo(x, y));
    }

    pub fn line_to(&mut self, x: f32, y: f32) {
        self.segs.push(Seg::LineTo(x, y));
    }

    pub fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        self.segs.push(Seg::QuadTo(x1, y1, x, y));
    }

    pub fn close(&mut self) {
        self.segs.push(Seg::Close);
    }

    pub fn push_circle(&mut self, cx: f32, cy: f32, r: f32) {
        self.segs.push(Seg::Circle(cx, cy, r));
    }

    pub fn from_circle(cx: f32, cy: f32, r: f32) -> Option<Path> {
        let mut pb = PathBuilder::new();
        pb.push_circle(cx, cy, r);
        pb.finish()
    }

    pub fn finish(self) -> Option<Path> {
        let drawable = self.segs.iter().any(|s| !matches!(s, Seg::MoveTo(..)));
        let finite = self.segs.iter().all(|s| match *s {
            Seg::MoveTo(a, b) | Seg::LineTo(a, b) => a.is_finite() && b.is_finite(),
            Seg::QuadTo(a, b, c, d) => a.is_finite() && b.is_finite() && c.is_finite() && d.is_finite(),
            Seg::Circle(a, b, r) => a.is_finite() && b.is_finite() && r.is_finite(),
            Seg::Close => true,
        });
        (drawable && finite).then_some(Path { segs: self.segs })
    }
}

// ---- fonts -------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FontId {
    /// Rajdhani Medium: labels, small text.
    Label,
    /// Rajdhani SemiBold: titles, values.
    Semi,
    /// Oxanium (variable, wght 700): giant digits, NO SIGNAL.
    Digits,
}

/// An embedded font: the ab_glyph face used for measuring (and for CPU glyph
/// rasterisation), plus the raw TTF bytes and variation so another backend can
/// load the very same font. Derefs to the ab_glyph face.
#[derive(Clone)]
pub struct FontFace {
    pub id: FontId,
    pub font: FontRef<'static>,
    /// The embedded TTF file.
    pub data: &'static [u8],
    /// Variable-font axis applied to `font` (tag, value), e.g. `(*b"wght", 700.0)`.
    pub variation: Option<([u8; 4], f32)>,
}

impl std::ops::Deref for FontFace {
    type Target = FontRef<'static>;
    fn deref(&self) -> &FontRef<'static> {
        &self.font
    }
}

// ---- coverage bitmaps ----------------------------------------------------------

/// An 8-bit coverage bitmap (digit mask, rain glyph), row-major, `w * h` bytes.
#[derive(Clone, Copy)]
pub struct Cov<'a> {
    pub w: usize,
    pub h: usize,
    pub data: &'a [u8],
}

/// One row of a coverage blit: source row, destination row (pixels), colour
/// (0..=255 per channel) and opacity.
#[derive(Clone, Copy, Debug)]
pub struct CovRow {
    pub src_row: usize,
    pub dst_y: i32,
    pub rgb: [f32; 3],
    pub a: f32,
}

// ---- layers --------------------------------------------------------------------

/// Handle of an offscreen layer owned by a canvas (see [`Canvas::instance_id`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayerId(pub u32);

/// In-place colour filter of a layer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LayerFilter {
    /// Desaturate and darken: `c = (c * keep + grey * greyw) * gain`, alpha
    /// `* gain` (grey = 0.30 R + 0.59 G + 0.11 B).
    Dim { keep: f32, greyw: f32, gain: f32 },
}

// ---- the trait --------------------------------------------------------------------

pub trait Canvas {
    /// Unique per backend instance: layer ids are only valid on the canvas
    /// (store) that created them. The renderer drops its caches when it sees
    /// another id.
    fn instance_id(&self) -> u64;

    /// Pixel size of the current drawing target (main surface or layer).
    fn size(&self) -> (u32, u32);

    /// Design -> pixel mapping `(s, ox, oy)`; persists across layer targets.
    fn view(&self) -> (f32, f32, f32);
    fn set_view(&mut self, s: f32, ox: f32, oy: f32);

    // -- vector operations (design units, through the view) --

    /// Anti-aliased, non-zero winding fill.
    fn fill_path(&mut self, p: &Path, c: Color);
    /// Anti-aliased stroke, butt caps, miter joins, no dash; `w` in design units.
    fn stroke_path(&mut self, p: &Path, c: Color, w: f32);
    /// Axis-aligned rectangle (no-op for an invalid one).
    fn fill_rect(&mut self, x: f32, y: f32, w: f32, h: f32, c: Color, aa: bool);
    /// Blends `c` over the whole target, pixel space, no anti-aliasing.
    fn overlay(&mut self, c: Color);
    /// Replaces every pixel of the target with `c` (pixel space).
    fn clear(&mut self, c: Color);

    /// Draws `s` (already capped) with the pen starting at `x`, baseline at
    /// `base_y`: design units, `size` and `tracking` too. Layout (alignment,
    /// width) is the caller's job, see `text::text_width`.
    #[allow(clippy::too_many_arguments)]
    fn text_run(&mut self, font: &FontFace, size: f32, x: f32, base_y: f32, s: &str, color: Color, tracking: f32);

    // -- coverage bitmaps (pixel space) --

    /// Blends colour `rgb` (0..=255) through the coverage of `cov`, top-left at
    /// `(x, y)`, optionally restricted to `clip = (x0, y0, x1, y1)`.
    fn blit_cov(&mut self, cov: &Cov, x: i32, y: i32, rgb: [f32; 3], a: f32, clip: Option<(i32, i32, i32, i32)>);
    /// Row by row version: each [`CovRow`] draws one source row at `(x, dst_y)`
    /// with its own colour / opacity.
    fn blit_cov_rows(&mut self, cov: &Cov, x: i32, rows: &[CovRow]);

    // -- layers (pixel space) --

    fn create_layer(&mut self, w: u32, h: u32) -> LayerId;
    fn drop_layer(&mut self, id: LayerId);
    /// Redirects drawing to the layer until [`Canvas::end_layer`].
    fn begin_layer(&mut self, id: LayerId);
    fn end_layer(&mut self);
    /// Replaces the target with the layer's content (same size).
    fn layer_copy_in(&mut self, id: LayerId);
    /// Replaces the layer's content with the target's (same size).
    fn layer_capture(&mut self, id: LayerId);
    /// Source-over composite of the layer on the target at `(x, y)`.
    fn layer_blit(&mut self, id: LayerId, x: i32, y: i32);
    fn layer_filter(&mut self, id: LayerId, f: LayerFilter);

    // -- hints --

    /// Marks the start of a new group of operations. The scene code emits it
    /// between independent parts (a pipe, a box, a fan) so that the CPU
    /// incremental renderer can tell what changed between two frames.
    /// Backends that redraw everything ignore it.
    fn group(&mut self) {}
}

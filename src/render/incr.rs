//! Incremental (damage based) rendering support.
//!
//! The dynamic part of a frame is *recorded* as a list of primitive draw
//! operations ([`Recorder`], a [`Canvas`] that rasterises nothing). Two
//! consecutive frames are compared operation by operation ([`diff`]); only
//! the pixels covered by an operation that changed are restored from the
//! static layer and repainted ([`replay`]). Because the repainted rectangles
//! are closed under "an operation that touches the rectangle lies entirely
//! inside it" ([`close_rects`]), every repainted pixel receives exactly the
//! same operations, in the same order, as in a full frame: the result is
//! pixel-identical to a full redraw.
//!
//! Operations are grouped (`Canvas::group` markers emitted by the scene
//! code) so that a group whose operation count varies (particles entering
//! and leaving a pipe) only invalidates its own bounding box.

use std::rc::Rc;

use super::canvas::{Canvas, Color, Cov, CovRow, FontFace, FontId, LayerFilter, LayerId, Path, Seg};
use super::text::{text_width, Fonts};
use ab_glyph::{Font, PxScale, ScaleFont};

/// Half-open integer rectangle in pixel space.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IRect {
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
}

impl IRect {
    pub const EMPTY: IRect = IRect { x0: 0, y0: 0, x1: 0, y1: 0 };

    pub fn is_empty(&self) -> bool {
        self.x1 <= self.x0 || self.y1 <= self.y0
    }

    pub fn area(&self) -> i64 {
        if self.is_empty() {
            0
        } else {
            (self.x1 - self.x0) as i64 * (self.y1 - self.y0) as i64
        }
    }

    pub fn union(&self, o: &IRect) -> IRect {
        if self.is_empty() {
            return *o;
        }
        if o.is_empty() {
            return *self;
        }
        IRect { x0: self.x0.min(o.x0), y0: self.y0.min(o.y0), x1: self.x1.max(o.x1), y1: self.y1.max(o.y1) }
    }

    pub fn intersects(&self, o: &IRect) -> bool {
        !self.is_empty() && !o.is_empty() && self.x0 < o.x1 && o.x0 < self.x1 && self.y0 < o.y1 && o.y0 < self.y1
    }

    pub fn contains(&self, o: &IRect) -> bool {
        o.x0 >= self.x0 && o.y0 >= self.y0 && o.x1 <= self.x1 && o.y1 <= self.y1
    }

    fn clamp(&self, w: i32, h: i32) -> IRect {
        let r = IRect { x0: self.x0.clamp(0, w), y0: self.y0.clamp(0, h), x1: self.x1.clamp(0, w), y1: self.y1.clamp(0, h) };
        if r.is_empty() {
            IRect::EMPTY
        } else {
            r
        }
    }
}

/// A canvas that can restore a rectangle of the target from a layer (the
/// CPU backend; needed by the incremental renderer only).
pub trait RectCanvas: Canvas {
    /// How the dirty rectangles are merged for this backend: `(ratio_pct,
    /// max_rects)`. Two rectangles merge when their bounding box is at most
    /// `ratio_pct` % of their summed area; then the closest pairs are merged
    /// until at most `max_rects` remain. The CPU pays per pixel (tight
    /// rectangles), a GPU per draw call (few rectangles).
    fn merge_tuning(&self) -> (u32, usize) {
        (125, usize::MAX)
    }

    /// Copies the pixels of `r` from layer `id` (same size as the target)
    /// into the target.
    fn layer_copy_rect(&mut self, id: LayerId, r: &IRect);
    /// Restricts the later *text and coverage blits* on the main target to
    /// `r` (`None` lifts it). Vector operations are not clipped (a mask would
    /// round differently): see [`RectCanvas::save_rect`].
    fn set_clip(&mut self, r: Option<&IRect>);
    /// Copies the pixels of `r` of the main target into `buf`.
    fn save_rect(&mut self, r: &IRect, buf: &mut Vec<u8>);
    /// Writes back the pixels saved by [`RectCanvas::save_rect`] for `r`,
    /// except those inside `keep`.
    fn restore_outside(&mut self, r: &IRect, buf: &[u8], keep: &IRect);
}

type RowKey = (usize, i32, [f32; 3], f32);

#[derive(Clone, PartialEq)]
enum Kind {
    Fill(Path, Color),
    Stroke(Path, Color, f32),
    Rect(f32, f32, f32, f32, Color, bool),
    Overlay(Color),
    Clear(Color),
    Text { font: FontId, size: f32, x: f32, y: f32, s: String, color: Color, tracking: f32 },
    Cov { w: usize, h: usize, data: Rc<[u8]>, x: i32, y: i32, rgb: [f32; 3], a: f32, clip: Option<(i32, i32, i32, i32)> },
    Rows { w: usize, h: usize, data: Rc<[u8]>, x: i32, rows: Vec<RowKey> },
}

#[derive(Clone)]
pub struct Op {
    kind: Kind,
    /// Conservative pixel bounds of everything the operation can touch.
    b: IRect,
}

/// The recorded dynamic content of one frame.
#[derive(Clone)]
pub struct Frame {
    ops: Vec<Op>,
    /// Start index in `ops` of every group (always starts with 0).
    groups: Vec<usize>,
    view: (f32, f32, f32),
    size: (u32, u32),
}

impl Frame {
    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    pub fn op_count(&self) -> usize {
        self.ops.len()
    }

    fn group(&self, g: usize) -> &[Op] {
        let a = self.groups[g];
        let b = self.groups.get(g + 1).copied().unwrap_or(self.ops.len());
        &self.ops[a..b]
    }
}

// ---- recorder ---------------------------------------------------------------

/// A [`Canvas`] that records vector / text / coverage operations. Layer
/// operations are not supported (the live frame does not use any).
pub struct Recorder {
    frame: Frame,
    last_cov: Option<(*const u8, usize, Rc<[u8]>)>,
}

impl Recorder {
    pub fn new(size: (u32, u32), view: (f32, f32, f32)) -> Self {
        Recorder { frame: Frame { ops: Vec::with_capacity(2048), groups: vec![0], view, size }, last_cov: None }
    }

    pub fn finish(self) -> Frame {
        self.frame
    }

    fn pad_rect(&self, x0: f32, y0: f32, x1: f32, y1: f32, pad: f32) -> IRect {
        let (w, h) = (self.frame.size.0 as i32, self.frame.size.1 as i32);
        if ![x0, y0, x1, y1, pad].iter().all(|v| v.is_finite()) {
            return IRect { x0: 0, y0: 0, x1: w, y1: h };
        }
        IRect { x0: (x0 - pad).floor() as i32, y0: (y0 - pad).floor() as i32, x1: (x1 + pad).ceil() as i32, y1: (y1 + pad).ceil() as i32 }.clamp(w, h)
    }

    fn path_bounds(&self, p: &Path, pad_design: f32) -> IRect {
        let (s, ox, oy) = self.frame.view;
        let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        let mut add = |x: f32, y: f32, r: f32| {
            x0 = x0.min(x - r);
            y0 = y0.min(y - r);
            x1 = x1.max(x + r);
            y1 = y1.max(y + r);
        };
        for seg in p.segments() {
            match *seg {
                Seg::MoveTo(x, y) | Seg::LineTo(x, y) => add(x, y, 0.0),
                Seg::QuadTo(a, b, x, y) => {
                    add(a, b, 0.0);
                    add(x, y, 0.0);
                }
                Seg::Circle(x, y, r) => add(x, y, r.abs()),
                Seg::Close => {}
            }
        }
        // 1.5 px of anti-aliasing reach on top of the (miter) stroke reach
        self.pad_rect(x0 * s + ox, y0 * s + oy, x1 * s + ox, y1 * s + oy, pad_design * s + 1.5)
    }

    fn push(&mut self, kind: Kind, b: IRect) {
        self.frame.ops.push(Op { kind, b });
    }

    fn intern(&mut self, data: &[u8]) -> Rc<[u8]> {
        if let Some((p, n, rc)) = &self.last_cov {
            if *p == data.as_ptr() && *n == data.len() {
                return rc.clone();
            }
        }
        let rc: Rc<[u8]> = Rc::from(data);
        self.last_cov = Some((data.as_ptr(), data.len(), rc.clone()));
        rc
    }
}

impl Canvas for Recorder {
    fn instance_id(&self) -> u64 {
        0
    }
    fn size(&self) -> (u32, u32) {
        self.frame.size
    }
    fn view(&self) -> (f32, f32, f32) {
        self.frame.view
    }
    fn set_view(&mut self, s: f32, ox: f32, oy: f32) {
        self.frame.view = (s, ox, oy);
    }
    fn group(&mut self) {
        let n = self.frame.ops.len();
        self.frame.groups.push(n);
    }

    fn fill_path(&mut self, p: &Path, c: Color) {
        let b = self.path_bounds(p, 0.0);
        self.push(Kind::Fill(p.clone(), c), b);
    }

    fn stroke_path(&mut self, p: &Path, c: Color, w: f32) {
        let b = self.path_bounds(p, w * 2.0);
        self.push(Kind::Stroke(p.clone(), c, w), b);
    }

    fn fill_rect(&mut self, x: f32, y: f32, w: f32, h: f32, c: Color, aa: bool) {
        let (s, ox, oy) = self.frame.view;
        let b = self.pad_rect(x * s + ox, y * s + oy, (x + w) * s + ox, (y + h) * s + oy, 1.5);
        self.push(Kind::Rect(x, y, w, h, c, aa), b);
    }

    fn overlay(&mut self, c: Color) {
        let b = IRect { x0: 0, y0: 0, x1: self.frame.size.0 as i32, y1: self.frame.size.1 as i32 };
        self.push(Kind::Overlay(c), b);
    }

    fn clear(&mut self, c: Color) {
        let b = IRect { x0: 0, y0: 0, x1: self.frame.size.0 as i32, y1: self.frame.size.1 as i32 };
        self.push(Kind::Clear(c), b);
    }

    fn text_run(&mut self, font: &FontFace, size: f32, x: f32, base_y: f32, s: &str, color: Color, tracking: f32) {
        let (vs, ox, oy) = self.frame.view;
        let px = size * vs;
        let sc = font.font.as_scaled(PxScale::from(px));
        let w = text_width(&font.font, px, s, tracking * vs) + (tracking * vs).abs();
        let x0 = x * vs + ox;
        let by = base_y * vs + oy;
        let b = self.pad_rect(x0, by - sc.ascent(), x0 + w, by - sc.descent(), 3.0);
        self.push(Kind::Text { font: font.id, size, x, y: base_y, s: s.to_string(), color, tracking }, b);
    }

    fn blit_cov(&mut self, cov: &Cov, x: i32, y: i32, rgb: [f32; 3], a: f32, clip: Option<(i32, i32, i32, i32)>) {
        let (w, h) = (self.frame.size.0 as i32, self.frame.size.1 as i32);
        let mut b = IRect { x0: x, y0: y, x1: x + cov.w as i32, y1: y + cov.h as i32 };
        if let Some((a0, b0, a1, b1)) = clip {
            b = IRect { x0: b.x0.max(a0), y0: b.y0.max(b0), x1: b.x1.min(a1), y1: b.y1.min(b1) };
        }
        let b = b.clamp(w, h);
        let data = self.intern(cov.data);
        self.push(Kind::Cov { w: cov.w, h: cov.h, data, x, y, rgb, a, clip }, b);
    }

    fn blit_cov_rows(&mut self, cov: &Cov, x: i32, rows: &[CovRow]) {
        let (w, h) = (self.frame.size.0 as i32, self.frame.size.1 as i32);
        let (mut y0, mut y1) = (i32::MAX, i32::MIN);
        for r in rows {
            y0 = y0.min(r.dst_y);
            y1 = y1.max(r.dst_y + 1);
        }
        let b = IRect { x0: x, y0, x1: x + cov.w as i32, y1 }.clamp(w, h);
        let data = self.intern(cov.data);
        let rows = rows.iter().map(|r| (r.src_row, r.dst_y, r.rgb, r.a)).collect();
        self.push(Kind::Rows { w: cov.w, h: cov.h, data, x, rows }, b);
    }

    fn create_layer(&mut self, _w: u32, _h: u32) -> LayerId {
        unreachable!("layers are not recordable")
    }
    fn drop_layer(&mut self, _id: LayerId) {}
    fn begin_layer(&mut self, _id: LayerId) {
        unreachable!("layers are not recordable")
    }
    fn end_layer(&mut self) {}
    fn layer_copy_in(&mut self, _id: LayerId) {
        unreachable!("layers are not recordable")
    }
    fn layer_capture(&mut self, _id: LayerId) {
        unreachable!("layers are not recordable")
    }
    fn layer_blit(&mut self, _id: LayerId, _x: i32, _y: i32) {
        unreachable!("layers are not recordable")
    }
    fn layer_filter(&mut self, _id: LayerId, _f: LayerFilter) {
        unreachable!("layers are not recordable")
    }
}

// ---- replay -------------------------------------------------------------------

/// Re-issues one recorded operation on `cv` (whose view must equal the
/// frame's).
pub fn replay_op(cv: &mut dyn Canvas, fonts: &Fonts, op: &Op) {
    match &op.kind {
        Kind::Fill(p, c) => cv.fill_path(p, *c),
        Kind::Stroke(p, c, w) => cv.stroke_path(p, *c, *w),
        Kind::Rect(x, y, w, h, c, aa) => cv.fill_rect(*x, *y, *w, *h, *c, *aa),
        Kind::Overlay(c) => cv.overlay(*c),
        Kind::Clear(c) => cv.clear(*c),
        Kind::Text { font, size, x, y, s, color, tracking } => cv.text_run(fonts.get(*font), *size, *x, *y, s, *color, *tracking),
        Kind::Cov { w, h, data, x, y, rgb, a, clip } => cv.blit_cov(&Cov { w: *w, h: *h, data }, *x, *y, *rgb, *a, *clip),
        Kind::Rows { w, h, data, x, rows } => {
            let rows: Vec<CovRow> = rows.iter().map(|&(src_row, dst_y, rgb, a)| CovRow { src_row, dst_y, rgb, a }).collect();
            cv.blit_cov_rows(&Cov { w: *w, h: *h, data }, *x, &rows)
        }
    }
}

/// Every op of `frame` in order (full redraw).
pub fn replay_all(cv: &mut dyn Canvas, fonts: &Fonts, frame: &Frame) -> usize {
    for op in &frame.ops {
        replay_op(cv, fonts, op);
    }
    frame.ops.len()
}

/// Ops of `frame` touching `r`, in order. The canvas must be clipped to `r`
/// ([`RectCanvas::set_clip`]) so that ops straddling its border only paint
/// inside it.
pub fn replay_in(cv: &mut dyn RectCanvas, fonts: &Fonts, frame: &Frame, r: &IRect, scratch: &mut Vec<u8>) -> usize {
    let mut n = 0;
    cv.set_clip(Some(r));
    for op in &frame.ops {
        if !op.b.intersects(r) {
            continue;
        }
        n += 1;
        let vector = matches!(op.kind, Kind::Fill(..) | Kind::Stroke(..) | Kind::Rect(..) | Kind::Overlay(_) | Kind::Clear(_));
        if vector && !r.contains(&op.b) {
            // not clippable exactly: draw it whole, then put back what lies outside `r`
            cv.save_rect(&op.b, scratch);
            replay_op(cv, fonts, op);
            cv.restore_outside(&op.b, scratch, r);
        } else {
            replay_op(cv, fonts, op);
        }
    }
    cv.set_clip(None);
    n
}

// ---- diff -----------------------------------------------------------------------

/// Raw dirty rectangles between two frames, or `None` when the frames are
/// not comparable (size, view or group structure changed) and a full redraw
/// is needed.
pub fn diff(prev: &Frame, cur: &Frame) -> Option<Vec<IRect>> {
    if prev.size != cur.size || prev.view != cur.view || prev.groups.len() != cur.groups.len() {
        return None;
    }
    let mut out = Vec::new();
    for g in 0..cur.groups.len() {
        let (a, b) = (prev.group(g), cur.group(g));
        if a.len() == b.len() {
            for (x, y) in a.iter().zip(b) {
                if x.kind != y.kind {
                    out.push(x.b);
                    out.push(y.b);
                }
            }
        } else {
            out.extend(a.iter().chain(b).map(|o| o.b));
        }
    }
    out.retain(|r| !r.is_empty());
    Some(out)
}

/// Merges rectangles that overlap or lie close together: two rectangles
/// become their bounding box when that box is at most 1.25 times as large as
/// both together. Overlapping results are fine for replay (see
/// [`replay_in`]): every pixel of every rectangle is rebuilt from the static
/// layer and all ops touching it, in order, so repainting a pixel twice gives
/// the same value.
pub fn close_rects(rects: Vec<IRect>, frame: &Frame) -> Vec<IRect> {
    close_rects_tuned(rects, frame, 125, usize::MAX)
}

/// [`close_rects`] with an explicit merge ratio (percent) and a cap on the
/// number of rectangles (see [`RectCanvas::merge_tuning`]).
pub fn close_rects_tuned(mut rects: Vec<IRect>, _frame: &Frame, ratio_pct: u32, max_rects: usize) -> Vec<IRect> {
    rects.sort_by_key(|r| (r.y0, r.x0));
    let mut out: Vec<IRect> = Vec::with_capacity(rects.len());
    for mut cur in rects {
        loop {
            let from = out.len().saturating_sub(24);
            let hit = (from..out.len()).rev().find(|&i| {
                let u = out[i].union(&cur);
                u.area() * 100 <= (out[i].area() + cur.area()) * ratio_pct as i64
            });
            match hit {
                Some(i) => cur = cur.union(&out.swap_remove(i)),
                None => break,
            }
        }
        out.push(cur);
    }
    // cap: merge the pair whose union adds the least area until few enough remain
    while out.len() > max_rects.max(1) {
        let mut best = (i64::MAX, 0, 1);
        for i in 0..out.len() {
            for j in i + 1..out.len() {
                let grow = out[i].union(&out[j]).area() - out[i].area() - out[j].area();
                if grow < best.0 {
                    best = (grow, i, j);
                }
            }
        }
        let b = out.swap_remove(best.2);
        out[best.1] = out[best.1].union(&b);
    }
    out
}

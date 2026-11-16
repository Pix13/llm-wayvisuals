//! Offscreen renderer of the 1920x480 sensor panel (tiny-skia + ab_glyph).
//!
//! No dependency on `source` / `history`: the caller passes plain data in a
//! [`PanelView`]. Static layers (frames, labels, ticks) are cached per theme.

pub mod canvas;
mod draw;
mod fmt;
mod fx;
#[cfg(feature = "gpu")]
pub mod gl;
pub mod incr;
mod scene;
pub mod skia;
pub mod text;
mod v2;

use tiny_skia::Pixmap;

pub use canvas::{Canvas, Color, Cov, CovRow, FontFace, FontId, LayerFilter, LayerId, Path, PathBuilder, Seg};
pub use skia::{SkiaCanvas, SkiaStore};

use crate::model::{ModelSelect, Phase, Snapshot};
use crate::profile::{self, Cat};
use crate::theme::{alpha, mix, Theme};
use draw::{Align, Fonts};
use fx::{DigitMetrics, Mask, Rain};

pub const WIDTH: u32 = 1920;
pub const HEIGHT: u32 = 480;

#[derive(Debug, Clone, PartialEq)]
pub enum Signal {
    Live,
    Connecting,
    Stale { age_s: f32 },
    Down(String),
}

pub struct PanelView<'a> {
    pub snapshot: Option<&'a Snapshot>,
    pub model: &'a ModelSelect,
    pub signal: Signal,
    pub theme: &'a Theme,
    /// Centre card title override; default: model name (+ engine).
    pub title: Option<&'a str>,
    /// decode_tps history: (age 0..1 where 0 = now, value), None = gap.
    pub wave: &'a [(f32, Option<f32>)],
    pub wave_max: Option<f32>,
    /// System CPU % history, last 15 s: (age 0..1 where 0 = now, 0..100), None = gap.
    pub cpu_wave: &'a [(f32, Option<f32>)],
    /// Animation time in seconds.
    pub t: f32,
}

#[derive(PartialEq)]
struct FrozenKey {
    theme: &'static str,
    size: (u32, u32),
    snap: Option<Snapshot>,
    sel: ModelSelect,
    title: Option<String>,
    wave: Vec<(f32, Option<f32>)>,
    wave_max: Option<f32>,
    cpu_wave: Vec<(f32, Option<f32>)>,
}

pub struct Renderer {
    fonts: Fonts,
    rain: Rain,
    dm: DigitMetrics,
    /// CPU backend state (layers) used by [`Renderer::render`].
    skia: SkiaStore,
    /// Instance id of the canvas the cached layers below belong to.
    owner: Option<u64>,
    stat: Option<(&'static str, u32, u32, LayerId)>,
    dynlayer: Option<(u32, u32, LayerId)>,
    /// Dimmed non-live composite (no NO SIGNAL text), reused while the key is unchanged.
    frozen: Option<(FrozenKey, LayerId)>,
    frozen_size: (u32, u32),
    gen_mask: Option<(String, Mask)>,
    pre_mask: Option<(String, Mask)>,
    /// Recorded dynamic content of the previous live frame ([`Renderer::render_incremental`]).
    incr: Option<(&'static str, incr::Frame)>,
}

/// A rectangle of the target pixmap, in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// What [`Renderer::render_incremental`] changed in the pixmap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Damage {
    /// The whole pixmap was repainted (`rects` is then the full area).
    pub full: bool,
    /// Disjoint repainted rectangles.
    pub rects: Vec<Rect>,
}

impl Damage {
    pub fn full(w: u32, h: u32) -> Damage {
        Damage { full: true, rects: vec![Rect { x: 0, y: 0, w, h }] }
    }

    pub fn is_empty(&self) -> bool {
        self.rects.is_empty()
    }

    /// Repainted pixels.
    pub fn area(&self) -> u64 {
        self.rects.iter().map(|r| r.w as u64 * r.h as u64).sum()
    }
}

impl Default for Renderer {
    fn default() -> Self {
        Self::new()
    }
}

/// Uniform scale used for a target width: `w / 1920`, clamped to 0.5..=2.0
/// (960..3840 px). Outside, the panel is centred horizontally.
pub fn scale_for(w: u32) -> f32 {
    (w as f32 / WIDTH as f32).clamp(0.5, 2.0)
}

/// Design height (in design pixels, 1920 wide) used for a `w` x `h` target:
/// `h / scale_for(w)` clamped to MIN_HD..=MAX_HD. Outside the range the
/// panel is centred vertically with black bands around it.
pub fn design_height_for(w: u32, h: u32) -> f32 {
    (h as f32 / scale_for(w)).clamp(MIN_HD, MAX_HD)
}

pub const MIN_HD: f32 = 320.0;
pub const MAX_HD: f32 = 540.0;

impl Renderer {
    pub fn new() -> Self {
        let fonts = Fonts::load();
        let rain = Rain::new(&fonts.label);
        let dm = fx::digit_metrics(&fonts.digits);
        Renderer { fonts, rain, dm, skia: SkiaStore::new(), owner: None, stat: None, dynlayer: None, frozen: None, frozen_size: (0, 0), gen_mask: None, pre_mask: None, incr: None }
    }

    /// Renders into `pm`, filling it, natively at any size. All geometry
    /// is drawn with the uniform scale `s = scale_for(W)` (text glyphs are
    /// rasterised at the final pixel size, no resampling). The panel is
    /// laid out for the design height `H / s` clamped to MIN_HD..=MAX_HD
    /// (320..=540): vertical positions and sizes follow the card height so
    /// nothing is squashed. Outside that range (or W outside 960..3840) the
    /// panel is centred with black around it.
    pub fn render(&mut self, pm: &mut Pixmap, view: &PanelView) {
        let (w, h) = (pm.width(), pm.height());
        let mut store = std::mem::take(&mut self.skia);
        {
            let mut cv = store.canvas(pm);
            self.render_canvas(&mut cv, w, h, view);
        }
        self.skia = store;
    }

    /// Like [`Renderer::render`], but repaints only what changed since the
    /// previous call: `pm` must be the very pixmap of the previous call
    /// (same size, content untouched in between). Returns the repainted
    /// area; the result is pixel-identical to a full `render`. Non-live
    /// signals and any size / theme change repaint everything.
    pub fn render_incremental(&mut self, pm: &mut Pixmap, view: &PanelView) -> Damage {
        let (w, h) = (pm.width(), pm.height());
        if view.signal != Signal::Live {
            self.incr = None;
            self.render(pm, view);
            return Damage::full(w, h);
        }
        let mut store = std::mem::take(&mut self.skia);
        let d = {
            let mut cv = store.canvas(pm);
            self.render_live_incremental(&mut cv, w, h, view)
        };
        self.skia = store;
        d
    }

    fn render_live_incremental(&mut self, cv: &mut dyn incr::RectCanvas, w: u32, h: u32, view: &PanelView) -> Damage {
        let _p = profile::scope(Cat::Render);
        if self.owner != Some(cv.instance_id()) {
            self.stat = None;
            self.dynlayer = None;
            self.frozen = None;
            self.incr = None;
            self.owner = Some(cv.instance_id());
        }
        let s = scale_for(w);
        let hd = design_height_for(w, h);
        let ox = (w as f32 - WIDTH as f32 * s) / 2.0;
        let oy = (h as f32 - hd * s) / 2.0;
        cv.set_view(s, ox, oy);
        let th = view.theme;
        let lay = scene::Lay::new(hd);
        let stat = self.ensure_static(cv, th, &lay, w, h);
        let model = view.snapshot.and_then(|s| s.select(view.model));
        let data = scene::Data { model, gpus: view.snapshot.map(|s| s.gpus_of(model)).unwrap_or_default() };
        let frame = {
            let _p = profile::scope(Cat::Record);
            let mut rec = incr::Recorder::new((w, h), cv.view());
            self.draw_dynamic(&mut rec, view, &data, &lay, true, view.t);
            let _p = profile::scope(Cat::Overlay);
            let right = self.title_right(view, &data);
            rec.group();
            scene::draw_centre_title(&mut rec, th, &self.fonts, &lay, &self.title_of(view, &data), right.as_deref());
            rec.finish()
        };
        let rects = {
            let _p = profile::scope(Cat::Diff);
            self.incr
                .take()
                .filter(|(n, f)| *n == th.name && f.size() == (w, h))
                .and_then(|(_, prev)| incr::diff(&prev, &frame))
                .map(|r| incr::close_rects(r, &frame))
        };
        let _p = profile::scope(Cat::Replay);
        let (damage, nops) = match rects {
            None => {
                cv.layer_copy_in(stat);
                (Damage::full(w, h), incr::replay_all(cv, &self.fonts, &frame))
            }
            Some(rs) => {
                let mut nops = 0;
                let mut scratch = Vec::new();
                for r in &rs {
                    cv.layer_copy_rect(stat, r);
                    nops += incr::replay_in(cv, &self.fonts, &frame, r, &mut scratch);
                }
                let rects = rs.iter().map(|r| Rect { x: r.x0 as u32, y: r.y0 as u32, w: (r.x1 - r.x0) as u32, h: (r.y1 - r.y0) as u32 }).collect();
                (Damage { full: false, rects }, nops)
            }
        };
        cv.set_view(1.0, 0.0, 0.0);
        profile::damage(damage.area() as f64 / (w as f64 * h as f64).max(1.0), damage.rects.len(), nops, frame.op_count());
        self.incr = Some((th.name, frame));
        damage
    }

    /// Backend-neutral entry point: renders a `w` x `h` frame into `cv`'s
    /// main target (see [`Renderer::render`] for the layout rules). Cached
    /// layers belong to one canvas instance ([`Canvas::instance_id`]).
    pub fn render_canvas(&mut self, cv: &mut dyn Canvas, w: u32, h: u32, view: &PanelView) {
        let _p = profile::scope(Cat::Render);
        if self.owner != Some(cv.instance_id()) {
            self.stat = None;
            self.dynlayer = None;
            self.frozen = None;
            self.incr = None;
            self.owner = Some(cv.instance_id());
        }
        let s = scale_for(w);
        let hd = design_height_for(w, h);
        let ox = (w as f32 - WIDTH as f32 * s) / 2.0;
        let oy = (h as f32 - hd * s) / 2.0;
        cv.set_view(s, ox, oy);
        self.render_native(cv, w, h, view, hd);
        cv.set_view(1.0, 0.0, 0.0);
    }

    fn ensure_static(&mut self, cv: &mut dyn Canvas, th: &Theme, lay: &scene::Lay, w: u32, h: u32) -> LayerId {
        if let Some((n, a, b, id)) = self.stat {
            if (n, a, b) == (th.name, w, h) {
                return id;
            }
            cv.drop_layer(id);
        }
        let id = cv.create_layer(w, h);
        cv.begin_layer(id);
        scene::draw_static(cv, th, &self.fonts, lay);
        cv.end_layer();
        self.stat = Some((th.name, w, h, id));
        id
    }

    fn render_native(&mut self, cv: &mut dyn Canvas, w: u32, h: u32, view: &PanelView, hd: f32) {
        let th = view.theme;
        let lay = scene::Lay::new(hd);
        let stat = self.ensure_static(cv, th, &lay, w, h);
        {
            let _p = profile::scope(Cat::StaticBlit);
            cv.layer_copy_in(stat);
        }

        let live = view.signal == Signal::Live;
        let model = view.snapshot.and_then(|s| s.select(view.model));
        let data = scene::Data {
            model,
            gpus: view.snapshot.map(|s| s.gpus_of(model)).unwrap_or_default(),
        };
        if live {
            self.draw_dynamic(cv, view, &data, &lay, true, view.t);
            let _p = profile::scope(Cat::Overlay);
            let right = self.title_right(view, &data);
            scene::draw_centre_title(cv, th, &self.fonts, &lay, &self.title_of(view, &data), right.as_deref());
        } else {
            let key = FrozenKey {
                theme: th.name,
                size: (w, h),
                snap: view.snapshot.cloned(),
                sel: view.model.clone(),
                title: view.title.map(str::to_string),
                wave: view.wave.to_vec(),
                wave_max: view.wave_max,
                cpu_wave: view.cpu_wave.to_vec(),
            };
            if self.frozen.as_ref().map(|(k, _)| k) != Some(&key) {
                let id = match self.frozen.take() {
                    Some((_, id)) if self.frozen_size == (w, h) => id,
                    Some((_, id)) => {
                        cv.drop_layer(id);
                        cv.create_layer(w, h)
                    }
                    None => cv.create_layer(w, h),
                };
                self.frozen_size = (w, h);
                self.build_frozen(cv, view, &data, &lay, id);
                self.frozen = Some((key, id));
            }
            let _p = profile::scope(Cat::Overlay);
            cv.layer_copy_in(self.frozen.as_ref().unwrap().1);
            self.draw_no_signal(cv, view, &lay);
        }
    }

    /// Dimmed composite: frames at ~50 %, dynamic content at ~28 %,
    /// desaturated; the matrix rain is frozen at a fixed time. The target
    /// holds the static layer on entry; the result is captured into `out`.
    fn build_frozen(&mut self, cv: &mut dyn Canvas, view: &PanelView, data: &scene::Data, lay: &scene::Lay, out: LayerId) {
        let th = view.theme;
        let (w, h) = cv.size();
        draw::fill_all(cv, Color::from_rgba(0.0, 0.0, 0.0, 0.5).unwrap());
        let d = match self.dynlayer.take() {
            Some((dw, dh, id)) if (dw, dh) == (w, h) => id,
            Some((_, _, id)) => {
                cv.drop_layer(id);
                cv.create_layer(w, h)
            }
            None => cv.create_layer(w, h),
        };
        cv.begin_layer(d);
        cv.clear(Color::TRANSPARENT);
        self.draw_dynamic(cv, view, data, lay, false, 1.5);
        cv.end_layer();
        cv.layer_filter(d, LayerFilter::Dim { keep: 0.35, greyw: 0.65, gain: 0.28 });
        cv.layer_blit(d, 0, 0);
        self.dynlayer = Some((w, h, d));
        let mut th2 = th.clone();
        th2.text = alpha(th.text, 0.5);
        scene::draw_centre_title(cv, &th2, &self.fonts, lay, &self.title_of(view, data), self.title_right(view, data).as_deref());
        cv.layer_capture(out);
    }

    /// Right part of the title bar: `QUANT · HOST` (`--` when absent).
    fn title_right(&self, view: &PanelView, d: &scene::Data) -> Option<String> {
        let q = d.model.and_then(|m| m.display_quant()).map_or(fmt::NONE.to_string(), |q| draw::cap(q).to_uppercase());
        let h = view.snapshot.and_then(|s| s.host.as_deref()).filter(|h| !h.is_empty()).map_or(fmt::NONE.to_string(), |h| draw::cap(h).to_uppercase());
        Some(format!("{q} · {h}"))
    }

    fn title_of(&self, view: &PanelView, d: &scene::Data) -> String {
        if let Some(t) = view.title {
            return t.to_string();
        }
        match d.model {
            Some(m) => {
                let n = fmt::opt_str(m.name.as_deref());
                match m.engine.as_deref().filter(|e| !e.is_empty()) {
                    Some(e) => format!("{n}  /  {e}"),
                    None => n,
                }
            }
            None => "LLM VISUALS".to_string(),
        }
    }

    fn draw_dynamic(&mut self, pm: &mut dyn Canvas, view: &PanelView, data: &scene::Data, lay: &scene::Lay, live: bool, t: f32) {
        let th = view.theme;
        {
            let _p = profile::scope(Cat::TopCards);
            scene::draw_top_dynamic(pm, th, &self.fonts, lay, data, t, live);
        }
        {
            let _p = profile::scope(Cat::BottomCards);
            scene::draw_bottom_dynamic(pm, th, &self.fonts, lay, data);
        }
        {
            let _p = profile::scope(Cat::DataPath);
            let pv = v2::PathView::from(view.snapshot, data.model);
            v2::draw_data_path(pm, th, &self.fonts, lay, &pv, t);
        }
        {
            let _p = profile::scope(Cat::CpuCard);
            let cv = v2::CpuView::from(view.snapshot, data.model, view.cpu_wave);
            v2::draw_cpu_card(pm, th, &self.fonts, lay, &cv);
        }
        let _p = profile::scope(Cat::Centre);
        self.draw_centre(pm, view, data, lay, live, t);
    }

    fn mask(
        cache: &mut Option<(String, Mask)>,
        fonts: &Fonts,
        dm: DigitMetrics,
        s: &str,
        ghost: usize,
        cap: f32,
    ) -> Mask {
        let key = format!("{s}|{ghost}|{cap:.1}");
        if let Some((k, m)) = cache {
            if *k == key {
                return m.clone();
            }
        }
        let size = cap / dm.cap;
        let cell = dm.cell * size * 0.98;
        let m = fx::digits_mask(&fonts.digits, s, size, cap, cell, cell * 0.42, ghost);
        *cache = Some((key, m.clone()));
        m
    }

    fn draw_centre(&mut self, pm: &mut dyn Canvas, view: &PanelView, d: &scene::Data, lay: &scene::Lay, live: bool, t: f32) {
        use scene::px;
        let th = view.theme;
        let f = &self.fonts;
        let c = lay.top(1);
        let u = lay.ut;

        // matrix rain behind everything, away from the label column
        pm.group();
        {
            let _p = profile::scope(Cat::Rain);
            self.rain.draw(pm, th, c.x + 140.0, c.y + 46.0 * u, c.w - 150.0, c.h - 58.0 * u, t, 1.0);
        }

        let m = d.model;
        let phase = m.map(|m| m.phase());
        pm.group();

        if live {
            // GEN tok/s: ghost brightness when unknown, or 0 outside decode
            let gv = m.and_then(|m| m.decode_tps);
            let gen_s = fmt::big_tps(gv);
            let gen_ghost = match gv {
                None => gen_s.len(),
                Some(v) if v == 0.0 && phase != Some(Phase::Decode) => gen_s.len(),
                _ => fmt::ghost_zeros(&gen_s),
            };
            let vs = pm.view().0;
            let gm = Self::mask(&mut self.gen_mask, f, self.dm, &gen_s, gen_ghost, 76.0 * u * vs);
            let cx = c.x + 376.0;
            let right = draw::tx(pm, cx) + gm.w as f32 / 2.0;
            let base = c.y + 128.0 * u;
            let top = mix(th.accent, Color::WHITE, 0.5);
            let bot = th.accent;
            let (gx, gy) = ((draw::tx(pm, cx) - gm.w as f32 / 2.0) as i32, (draw::ty(pm, base) - gm.base as f32) as i32);
            fx::draw_digits(pm, &gm, gx, gy, top, bot, th.accent, (22.0 * u * vs) as usize, 1.0);
            draw::text(pm, &f.semi, px(30.0, u, 20.0), c.x + 28.0, base - 30.0 * u, "GEN", th.value, 2.0, Align::Left);
            draw::text(pm, &f.label, px(14.0, u, 10.0), c.x + 28.0, base - 10.0 * u, "TOK/S", th.dim, 2.0, Align::Left);

            // PREFILL tok/s: same style, ~55 % height, same right edge
            let pv = m.and_then(|m| m.display_prefill_tps());
            let pre_s = fmt::big_tps(pv);
            let pre_ghost = if pv.is_none() { pre_s.len() } else { fmt::ghost_zeros(&pre_s) };
            let pmk = Self::mask(&mut self.pre_mask, f, self.dm, &pre_s, pre_ghost, 42.0 * u * vs);
            let base2 = c.y + 200.0 * u;
            let py = (draw::ty(pm, base2) - pmk.base as f32) as i32;
            fx::draw_digits(pm, &pmk, (right - pmk.w as f32) as i32, py, top, bot, th.accent, (14.0 * u * vs) as usize, 1.0);
            draw::text(pm, &f.semi, px(20.0, u, 14.0), c.x + 28.0, base2 - 16.0 * u, "PREFILL", th.value, 1.5, Align::Left);
            draw::text(pm, &f.label, px(12.0, u, 10.0), c.x + 28.0, base2, "TOK/S", th.dim, 2.0, Align::Left);
        }

        // bottom strip: phase + TTFT, waveform
        pm.group();
        let (ph_txt, ph_col) = match phase {
            Some(Phase::Idle) => ("IDLE", th.dim),
            Some(Phase::Prefill) => ("PREFILL", th.bright),
            Some(Phase::Decode) => ("DECODE", th.value),
            _ => (fmt::NONE, th.dim),
        };
        let ttft = m
            .and_then(|m| m.last_request.as_ref().and_then(|r| r.ttft_s))
            .or_else(|| m.and_then(|m| m.current_request.as_ref()).and_then(|r| r.ttft_s));
        draw::text(pm, &f.semi, px(27.0, u, 18.0), c.x + 28.0, c.y + 246.0 * u, ph_txt, ph_col, 2.0, Align::Left);
        draw::text(pm, &f.label, px(14.0, u, 10.0), c.x + 28.0, c.y + 264.0 * u, &format!("TTFT  {}", fmt::secs(ttft)), th.text, 1.0, Align::Left);
        pm.group();
        fx::draw_wave(pm, th, view.wave, view.wave_max, c.x + 230.0, c.y + 230.0 * u, c.w - 230.0 - 28.0, (34.0 * u).max(22.0), 1.0);
    }

    fn draw_no_signal(&mut self, pm: &mut dyn Canvas, view: &PanelView, lay: &scene::Lay) {
        use scene::px;
        let c = lay.top(1);
        let u = lay.ut;
        let f = &self.fonts;
        let (main, sub, col) = match &view.signal {
            Signal::Connecting => ("CONNECTING", "WAITING FOR DATA".to_string(), Color::from_rgba(1.0, 0.78, 0.25, 1.0).unwrap()),
            Signal::Stale { age_s } => ("NO SIGNAL", format!("STALE  {}", age_text(*age_s)), Color::from_rgba(1.0, 0.45, 0.2, 1.0).unwrap()),
            Signal::Down(why) => ("NO SIGNAL", why.to_uppercase(), Color::from_rgba(1.0, 0.27, 0.27, 1.0).unwrap()),
            Signal::Live => return,
        };
        let pulse = 0.78 + 0.22 * (view.t * 3.0).sin();
        let size = px(if main.len() > 9 { 44.0 } else { 54.0 }, u, 30.0);
        let cx = c.x + c.w / 2.0;
        let by = c.y + 128.0 * u;
        for (dx, dy) in [(-2.0, 0.0), (2.0, 0.0), (0.0, -2.0), (0.0, 2.0)] {
            draw::text(pm, &f.digits, size, cx + dx, by + dy, main, alpha(col, 0.18 * pulse), 3.0, Align::Center);
        }
        draw::text(pm, &f.digits, size, cx, by, main, alpha(col, pulse), 3.0, Align::Center);
        let ss = px(22.0, u, 14.0);
        let s = draw::clip_text(&f.semi, ss, &sub, c.w - 60.0, 2.0);
        draw::text(pm, &f.semi, ss, cx, by + 40.0 * u, &s, alpha(col, 0.9), 2.0, Align::Center);
    }
}

fn age_text(a: f32) -> String {
    let a = a.max(0.0);
    if a < 120.0 {
        format!("{a:.0} S")
    } else if a < 7200.0 {
        format!("{:.0} MIN", a / 60.0)
    } else {
        format!("{:.1} H", a / 3600.0)
    }
}

/// Benchmark helper: mean / max ms per frame at 1920x480 over `frames`
/// frames (after one warm-up frame). Uses the synthetic-v2-A fixture and a
/// synthetic wave; `with_snapshot = false` renders without any data.
pub fn bench(signal: Signal, frames: usize, with_snapshot: bool) -> (f64, f64) {
    bench_size(signal, frames, with_snapshot, WIDTH, HEIGHT)
}

/// Same as [`bench`] for a `w` x `h` target pixmap.
pub fn bench_size(signal: Signal, frames: usize, with_snapshot: bool, w: u32, h: u32) -> (f64, f64) {
    let snap = Snapshot::parse(include_str!("../fixtures/synthetic-v2-A.json")).unwrap();
    let th = crate::theme::theme("green").unwrap();
    let wave: Vec<(f32, Option<f32>)> =
        (0..120).map(|i| (1.0 - i as f32 / 119.0, Some(50.0 + (i % 11) as f32))).collect();
    let sel = ModelSelect::Auto;
    let mut r = Renderer::new();
    let mut pm = Pixmap::new(w, h).unwrap();
    let mk = |t: f32, signal: Signal| PanelView {
        snapshot: with_snapshot.then_some(&snap),
        model: &sel,
        signal,
        theme: &th,
        title: None,
        wave: &wave,
        wave_max: Some(80.0),
        cpu_wave: &[],
        t,
    };
    r.render(&mut pm, &mk(0.0, signal.clone()));
    let (mut sum, mut max) = (0.0f64, 0.0f64);
    for i in 0..frames {
        let t0 = std::time::Instant::now();
        r.render(&mut pm, &mk(i as f32 / 30.0, signal.clone()));
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        sum += ms;
        max = max.max(ms);
    }
    (sum / frames.max(1) as f64, max)
}

#[cfg(test)]
mod bench_tests {
    use super::*;

    #[test]
    #[ignore]
    fn bench_all_signals() {
        for (w, h) in [(1920, 348), (1920, 360), (1920, 480), (1920, 540), (1280, 320), (1440, 360), (1920, 320)] {
            for (n, s) in [("live", Signal::Live), ("stale", Signal::Stale { age_s: 12.0 })] {
                let (m, x) = bench_size(s, 200, true, w, h);
                println!("{w}x{h} {n}: mean {m:.2} ms, max {x:.2} ms");
            }
        }
    }
}

#[cfg(test)]
mod stack_tests {
    use super::*;

    fn frame(r: &mut Renderer, fx: &str, theme: &str, w: u32, h: u32) -> Pixmap {
        let snap = Snapshot::parse(&std::fs::read_to_string(format!("fixtures/{fx}.json")).unwrap()).unwrap();
        let th = crate::theme::theme(theme).unwrap();
        let wave: Vec<(f32, Option<f32>)> =
            (0..120).map(|i| (1.0 - i as f32 / 119.0, Some(50.0 + ((i * 7) % 11) as f32 * 2.0))).collect();
        let c = snap.system.as_ref().and_then(|s| s.cpu_pct).unwrap_or(0.0) as f32;
        let cpu: Vec<(f32, Option<f32>)> =
            (0..15).map(|i| (1.0 - i as f32 / 14.0, Some((c * (1.0 + 0.12 * (i as f32 * 1.7).sin())).clamp(0.0, 100.0)))).collect();
        let sel = ModelSelect::Auto;
        let view = PanelView {
            snapshot: Some(&snap), model: &sel, signal: Signal::Live, theme: &th, title: None,
            wave: &wave, wave_max: Some(90.0), cpu_wave: &cpu, t: 1.3,
        };
        let mut pm = Pixmap::new(w, h).unwrap();
        r.render(&mut pm, &view);
        pm
    }

    /// Writes v2-{A..E}.png, live-maya and 3-band/2-band stacks to $WAYVIS_OUT.
    #[test]
    #[ignore]
    fn render_sheets() {
        let Ok(dir) = std::env::var("WAYVIS_OUT") else { return };
        let mut r = Renderer::new();
        let themes = ["green", "gold", "blue", "purple", "gold"];
        for (k, sc) in ['A', 'B', 'C', 'D', 'E'].iter().enumerate() {
            frame(&mut r, &format!("synthetic-v2-{sc}"), themes[k], 1920, 480).save_png(format!("{dir}/v2-{sc}.png")).unwrap();
        }
        frame(&mut r, "live-maya-decode", "green", 1920, 480).save_png(format!("{dir}/v2-live-maya-decode.png")).unwrap();
        for (name, list, hb) in [
            ("stack-3", ["synthetic-v2-A", "synthetic-v2-C", "synthetic-v2-B"].as_slice(), 348u32),
            ("stack-2", ["synthetic-v2-A", "synthetic-v2-E"].as_slice(), 508),
        ] {
            let mut full = Pixmap::new(1920, hb * list.len() as u32).unwrap();
            full.fill(tiny_skia::Color::BLACK);
            for (k, fx) in list.iter().enumerate() {
                let band = frame(&mut r, fx, ["green", "blue", "purple"][k], 1920, hb);
                full.draw_pixmap(0, (hb * k as u32) as i32, band.as_ref(), &tiny_skia::PixmapPaint::default(), tiny_skia::Transform::identity(), None);
            }
            full.save_png(format!("{dir}/v2-{name}.png")).unwrap();
        }
    }
}

#[cfg(test)]
mod layer_cache_tests {
    use super::*;

    /// The cached layers (static, frozen, dim scratch) must give the same
    /// pixels on a cache hit as on a miss, across sizes and signals.
    #[test]
    fn cached_frames_equal_fresh_frames() {
        let snap = Snapshot::parse(include_str!("../fixtures/synthetic-v2-A.json")).unwrap();
        let th = crate::theme::theme("green").unwrap();
        let sel = ModelSelect::Auto;
        let mk = |signal: Signal| PanelView {
            snapshot: Some(&snap), model: &sel, signal, theme: &th, title: None,
            wave: &[], wave_max: None, cpu_wave: &[], t: 2.0,
        };
        let mut shared = Renderer::new();
        for (w, h) in [(1920, 480), (1920, 348), (1920, 480)] {
            for sig in [Signal::Stale { age_s: 3.0 }, Signal::Live, Signal::Stale { age_s: 3.0 }] {
                let mut a = Pixmap::new(w, h).unwrap();
                let mut b = Pixmap::new(w, h).unwrap();
                shared.render(&mut a, &mk(sig.clone()));
                Renderer::new().render(&mut b, &mk(sig.clone()));
                assert!(a.data() == b.data(), "{w}x{h} {sig:?}");
            }
        }
    }
}

#[cfg(test)]
mod incremental_tests {
    use super::*;

    /// A sequence of frames (changing snapshots, waves, time, sizes, themes
    /// and signals): the incremental pixmap must equal a fresh full render
    /// at every step, and most live frames must repaint only part of it.
    #[test]
    fn incremental_equals_full_over_a_sequence() {
        let names = ["synthetic-v2-A", "synthetic-v2-C", "live-maya-decode", "synthetic-v2-B", "live-maya-idle", "synthetic-v2-D"];
        let snaps: Vec<Snapshot> =
            names.iter().map(|n| Snapshot::parse(&std::fs::read_to_string(format!("fixtures/{n}.json")).unwrap()).unwrap()).collect();
        let themes = [crate::theme::theme("green").unwrap(), crate::theme::theme("blue").unwrap()];
        let sel = ModelSelect::Auto;
        let mut inc = Renderer::new();
        let mut full = Renderer::new();
        let mut pm: Option<Pixmap> = None;
        let (mut partial, mut live_frames) = (0, 0);
        let mut t = 0.0f32;
        for step in 0..150usize {
            t += 1.0 / 30.0;
            let (w, h) = if step < 100 { (1920, 348) } else { (1920, 480) };
            let snap = &snaps[(step / 9) % snaps.len()];
            let th = &themes[usize::from(step >= 60 && step < 75)];
            let n = 120 + (step % 5);
            let wave: Vec<(f32, Option<f32>)> = (0..n)
                .map(|i| (1.0 - i as f32 / (n - 1) as f32, if i % 37 == 5 { None } else { Some(40.0 + ((i + step) * 7 % 23) as f32) }))
                .collect();
            let cpu: Vec<(f32, Option<f32>)> = (0..15).map(|i| (1.0 - (i as f32 + t.fract()) / 15.0, Some(20.0 + ((i + step / 2) % 9) as f32 * 5.0))).collect();
            let signal = if (30..34).contains(&step) { Signal::Stale { age_s: 3.0 } } else { Signal::Live };
            let view = PanelView {
                snapshot: Some(snap), model: &sel, signal: signal.clone(), theme: th, title: None,
                wave: &wave, wave_max: Some(90.0), cpu_wave: &cpu, t,
            };
            if pm.as_ref().is_some_and(|p| (p.width(), p.height()) != (w, h)) {
                pm = None;
            }
            let pm = pm.get_or_insert_with(|| Pixmap::new(w, h).unwrap());
            let d = inc.render_incremental(pm, &view);
            let mut reference = Pixmap::new(w, h).unwrap();
            full.render(&mut reference, &view);
            if pm.data() != reference.data() {
                let (mut n, mut mx, mut first) = (0, 0i32, None);
                for (i, (a, b)) in pm.data().iter().zip(reference.data()).enumerate() {
                    if a != b {
                        n += 1;
                        mx = mx.max((*a as i32 - *b as i32).abs());
                        first.get_or_insert((i / 4 % w as usize, i / 4 / w as usize));
                    }
                }
                panic!("step {step}: incremental differs from full render: {n} bytes, max diff {mx}, first at {first:?}");
            }
            if signal == Signal::Live {
                live_frames += 1;
                if !d.full {
                    partial += 1;
                }
            }
        }
        assert!(partial * 10 >= live_frames * 7, "only {partial} of {live_frames} live frames were partial");
    }
}

#[cfg(test)]
mod dump_tests {
    use super::*;
    #[test]
    #[ignore]
    fn dump_rects() {
        let snap = Snapshot::parse(&std::fs::read_to_string(format!("fixtures/{}.json", std::env::var("FX").unwrap_or("live-maya-decode".into()))).unwrap()).unwrap();
        let th = crate::theme::theme("green").unwrap();
        let sel = ModelSelect::Auto;
        let mut r = Renderer::new();
        let mut pm = Pixmap::new(1920, 339).unwrap();
        for i in 0..204 {
            let wave: Vec<(f32, Option<f32>)> = (0..120).map(|k| (1.0 - k as f32 / 119.0, Some(50.0 + ((k + i) % 11) as f32))).collect();
            let view = PanelView { snapshot: Some(&snap), model: &sel, signal: Signal::Live, theme: &th, title: None, wave: &wave, wave_max: Some(90.0), cpu_wave: &[], t: i as f32 / 30.0 };
            let d = r.render_incremental(&mut pm, &view);
            if i >= 2 && i < 4 {
                println!("frame {i}: full {} area {:.1}%", d.full, 100.0 * d.area() as f64 / (1920.0 * 339.0));
                for x in &d.rects { println!("  {:4},{:4} {:4}x{:4}", x.x, x.y, x.w, x.h); }
            }
        }
    }
}

#[cfg(test)]
mod incr_bench {
    use super::*;

    /// Full vs incremental render time (interleaved, so machine load cancels out).
    #[test]
    #[ignore]
    fn bench_incremental() {
        let fx = std::env::var("FX").unwrap_or("live-maya-decode".into());
        let snap = Snapshot::parse(&std::fs::read_to_string(format!("fixtures/{fx}.json")).unwrap()).unwrap();
        let th = crate::theme::theme("green").unwrap();
        let sel = ModelSelect::Auto;
        let (mut a, mut b) = (Renderer::new(), Renderer::new());
        let (mut pa, mut pb) = (Pixmap::new(1920, 339).unwrap(), Pixmap::new(1920, 339).unwrap());
        let (mut tf, mut ti, mut area) = (0.0f64, 0.0f64, 0.0f64);
        let n = 300;
        for i in 0..n * (if std::env::var_os("ONLY_INC").is_some() { 10 } else { 1 }) + 20 {
            let t = i as f32 / 30.0;
            let wave: Vec<(f32, Option<f32>)> = (0..120).map(|k| (1.0 - (k as f32 + t.fract()) / 120.0, Some(50.0 + ((k + i / 15) % 11) as f32))).collect();
            let view = PanelView { snapshot: Some(&snap), model: &sel, signal: Signal::Live, theme: &th, title: None, wave: &wave, wave_max: Some(90.0), cpu_wave: &[], t };
            let t0 = std::time::Instant::now();
            if std::env::var_os("ONLY_INC").is_none() { a.render(&mut pa, &view); }
            let d0 = t0.elapsed().as_secs_f64() * 1e3;
            let t1 = std::time::Instant::now();
            let d = b.render_incremental(&mut pb, &view);
            let d1 = t1.elapsed().as_secs_f64() * 1e3;
            if i >= 20 {
                tf += d0;
                ti += d1;
                area += d.area() as f64 / (1920.0 * 339.0);
            }
        }
        println!("full {:.3} ms, incremental {:.3} ms, area {:.1} %", tf / n as f64, ti / n as f64, 100.0 * area / n as f64);
    }
}

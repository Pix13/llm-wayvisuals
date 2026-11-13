//! Offscreen renderer of the 1920x480 sensor panel (tiny-skia + ab_glyph).
//!
//! No dependency on `source` / `history`: the caller passes plain data in a
//! [`PanelView`]. Static layers (frames, labels, ticks) are cached per theme.

mod draw;
mod fmt;
mod fx;
mod scene;
mod v2;

use tiny_skia::{Color, Pixmap, PixmapPaint, Transform};

use crate::model::{ModelSelect, Phase, Snapshot};
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
    stat: Option<(&'static str, u32, u32, Pixmap)>,
    dynlayer: Option<Pixmap>,
    /// Dimmed non-live composite (no NO SIGNAL text), reused while the key is unchanged.
    frozen: Option<(FrozenKey, Pixmap)>,
    gen_mask: Option<(String, Mask)>,
    pre_mask: Option<(String, Mask)>,
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
        Renderer { fonts, rain, dm, stat: None, dynlayer: None, frozen: None, gen_mask: None, pre_mask: None }
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
        let s = scale_for(w);
        let hd = design_height_for(w, h);
        let ox = (w as f32 - WIDTH as f32 * s) / 2.0;
        let oy = (h as f32 - hd * s) / 2.0;
        draw::set_view(s, ox, oy);
        self.render_native(pm, view, hd);
        draw::set_view(1.0, 0.0, 0.0);
    }

    fn ensure_static(&mut self, th: &Theme, lay: &scene::Lay, w: u32, h: u32) {
        if self.stat.as_ref().map(|(n, a, b, _)| (*n, *a, *b)) != Some((th.name, w, h)) {
            let mut p = Pixmap::new(w, h).unwrap();
            scene::draw_static(&mut p, th, &self.fonts, lay);
            self.stat = Some((th.name, w, h, p));
        }
    }

    fn render_native(&mut self, pm: &mut Pixmap, view: &PanelView, hd: f32) {
        let th = view.theme;
        let lay = scene::Lay::new(hd);
        self.ensure_static(th, &lay, pm.width(), pm.height());
        pm.data_mut().copy_from_slice(self.stat.as_ref().unwrap().3.data());

        let live = view.signal == Signal::Live;
        let model = view.snapshot.and_then(|s| s.select(view.model));
        let data = scene::Data {
            model,
            gpus: view.snapshot.map(|s| s.gpus_of(model)).unwrap_or_default(),
        };
        if live {
            self.draw_dynamic(pm, view, &data, &lay, true, view.t);
            let right = self.title_right(view, &data);
            scene::draw_centre_title(pm, th, &self.fonts, &lay, &self.title_of(view, &data), right.as_deref());
        } else {
            let key = FrozenKey {
                theme: th.name,
                size: (pm.width(), pm.height()),
                snap: view.snapshot.cloned(),
                sel: view.model.clone(),
                title: view.title.map(str::to_string),
                wave: view.wave.to_vec(),
                wave_max: view.wave_max,
                cpu_wave: view.cpu_wave.to_vec(),
            };
            if self.frozen.as_ref().map(|(k, _)| k) != Some(&key) {
                let img = self.build_frozen(pm, view, &data, &lay);
                self.frozen = Some((key, img));
            }
            pm.data_mut().copy_from_slice(self.frozen.as_ref().unwrap().1.data());
            self.draw_no_signal(pm, view, &lay);
        }
    }

    /// Dimmed composite: frames at ~50 %, dynamic content at ~28 %,
    /// desaturated; the matrix rain is frozen at a fixed time. `pm` holds
    /// the static layer on entry (its pixels are overwritten by the caller).
    fn build_frozen(&mut self, pm: &mut Pixmap, view: &PanelView, data: &scene::Data, lay: &scene::Lay) -> Pixmap {
        let th = view.theme;
        draw::fill_all(pm, Color::from_rgba(0.0, 0.0, 0.0, 0.5).unwrap());
        let mut d = match self.dynlayer.take() {
            Some(p) if p.height() == pm.height() => p,
            _ => Pixmap::new(pm.width(), pm.height()).unwrap(),
        };
        d.fill(Color::TRANSPARENT);
        self.draw_dynamic(&mut d, view, data, lay, false, 1.5);
        for px in d.data_mut().chunks_exact_mut(4) {
            let g = px[0] as f32 * 0.30 + px[1] as f32 * 0.59 + px[2] as f32 * 0.11;
            for c in &mut px[..3] {
                *c = ((*c as f32 * 0.35 + g * 0.65) * 0.28) as u8;
            }
            px[3] = (px[3] as f32 * 0.28) as u8;
        }
        pm.draw_pixmap(0, 0, d.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
        self.dynlayer = Some(d);
        let mut th2 = th.clone();
        th2.text = alpha(th.text, 0.5);
        scene::draw_centre_title(pm, &th2, &self.fonts, lay, &self.title_of(view, data), self.title_right(view, data).as_deref());
        pm.clone()
    }

    /// Right part of the title bar: `QUANT · HOST` (`--` when absent).
    fn title_right(&self, view: &PanelView, d: &scene::Data) -> Option<String> {
        let q = d.model.and_then(|m| m.display_quant()).map_or(fmt::NONE.to_string(), |q| q.to_uppercase());
        let h = view.snapshot.and_then(|s| s.host.as_deref()).filter(|h| !h.is_empty()).map_or(fmt::NONE.to_string(), |h| h.to_uppercase());
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

    fn draw_dynamic(&mut self, pm: &mut Pixmap, view: &PanelView, data: &scene::Data, lay: &scene::Lay, live: bool, t: f32) {
        let th = view.theme;
        scene::draw_top_dynamic(pm, th, &self.fonts, lay, data, t, live);
        scene::draw_bottom_dynamic(pm, th, &self.fonts, lay, data);
        let pv = v2::PathView::from(view.snapshot, data.model);
        v2::draw_data_path(pm, th, &self.fonts, lay, &pv, t);
        let cv = v2::CpuView::from(view.snapshot, data.model, view.cpu_wave);
        v2::draw_cpu_card(pm, th, &self.fonts, lay, &cv);
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

    fn draw_centre(&mut self, pm: &mut Pixmap, view: &PanelView, d: &scene::Data, lay: &scene::Lay, live: bool, t: f32) {
        use scene::px;
        let th = view.theme;
        let f = &self.fonts;
        let c = lay.top(1);
        let u = lay.ut;

        // matrix rain behind everything, away from the label column
        self.rain.draw(pm, th, c.x + 140.0, c.y + 46.0 * u, c.w - 150.0, c.h - 58.0 * u, t, 1.0);

        let m = d.model;
        let phase = m.map(|m| m.phase());

        if live {
            // GEN tok/s: ghost brightness when unknown, or 0 outside decode
            let gv = m.and_then(|m| m.decode_tps);
            let gen_s = fmt::big_tps(gv);
            let gen_ghost = match gv {
                None => gen_s.len(),
                Some(v) if v == 0.0 && phase != Some(Phase::Decode) => gen_s.len(),
                _ => fmt::ghost_zeros(&gen_s),
            };
            let vs = draw::view().0;
            let gm = Self::mask(&mut self.gen_mask, f, self.dm, &gen_s, gen_ghost, 76.0 * u * vs);
            let cx = c.x + 376.0;
            let right = draw::tx(cx) + gm.w as f32 / 2.0;
            let base = c.y + 128.0 * u;
            let top = mix(th.accent, Color::WHITE, 0.5);
            let bot = th.accent;
            fx::draw_digits(pm, &gm, (draw::tx(cx) - gm.w as f32 / 2.0) as i32, (draw::ty(base) - gm.base as f32) as i32, top, bot, th.accent, (22.0 * u * vs) as usize, 1.0);
            draw::text(pm, &f.semi, px(30.0, u, 20.0), c.x + 28.0, base - 30.0 * u, "GEN", th.value, 2.0, Align::Left);
            draw::text(pm, &f.label, px(14.0, u, 10.0), c.x + 28.0, base - 10.0 * u, "TOK/S", th.dim, 2.0, Align::Left);

            // PREFILL tok/s: same style, ~55 % height, same right edge
            let pv = m.and_then(|m| m.display_prefill_tps());
            let pre_s = fmt::big_tps(pv);
            let pre_ghost = if pv.is_none() { pre_s.len() } else { fmt::ghost_zeros(&pre_s) };
            let pmk = Self::mask(&mut self.pre_mask, f, self.dm, &pre_s, pre_ghost, 42.0 * u * vs);
            let base2 = c.y + 200.0 * u;
            fx::draw_digits(pm, &pmk, (right - pmk.w as f32) as i32, (draw::ty(base2) - pmk.base as f32) as i32, top, bot, th.accent, (14.0 * u * vs) as usize, 1.0);
            draw::text(pm, &f.semi, px(20.0, u, 14.0), c.x + 28.0, base2 - 16.0 * u, "PREFILL", th.value, 1.5, Align::Left);
            draw::text(pm, &f.label, px(12.0, u, 10.0), c.x + 28.0, base2, "TOK/S", th.dim, 2.0, Align::Left);
        }

        // bottom strip: phase + TTFT, waveform
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
        fx::draw_wave(pm, th, view.wave, view.wave_max, c.x + 230.0, c.y + 230.0 * u, c.w - 230.0 - 28.0, (34.0 * u).max(22.0), 1.0);
    }

    fn draw_no_signal(&mut self, pm: &mut Pixmap, view: &PanelView, lay: &scene::Lay) {
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

    /// Writes v2-{A..E}.png, gpubox1 and 3-band/2-band stacks to $WAYVIS_OUT.
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
            full.fill(Color::BLACK);
            for (k, fx) in list.iter().enumerate() {
                let band = frame(&mut r, fx, ["green", "blue", "purple"][k], 1920, hb);
                full.draw_pixmap(0, (hb * k as u32) as i32, band.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
            }
            full.save_png(format!("{dir}/v2-{name}.png")).unwrap();
        }
    }
}

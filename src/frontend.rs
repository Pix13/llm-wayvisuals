//! Display frontend: backend-independent logic (output matching, per-panel
//! state, link-status logging) and the `Backend` seam.
//!
//! Today the only backend is `winit_backend` (winit + softbuffer: Wayland and
//! X11). A future `layer-shell` cargo feature adds another `Backend`
//! implementation next to it; `main` picks one and calls `run` /
//! `list_outputs`.

mod winit_backend;

use llm_wayvisuals::config::PanelConfig;
use llm_wayvisuals::history::History;
use llm_wayvisuals::model::{Bottleneck, ModelSelect, Phase, Snapshot};
use std::sync::Arc;
use llm_wayvisuals::render::{Damage, PanelView, Rect, Renderer, Signal};
use llm_wayvisuals::source::{Feed, LinkStatus, Source};
use llm_wayvisuals::theme::{self, Theme};
use std::time::{Duration, Instant};
use tiny_skia::Pixmap;

pub use winit_backend::WinitBackend;

/// Prints one line on stderr, prefixed. The only logging channel.
pub fn log(msg: impl AsRef<str>) {
    eprintln!("llm-wayvisuals: {}", msg.as_ref());
}

pub struct RunOptions {
    pub panels: Vec<PanelConfig>,
    /// Development mode: normal decorated windows, no fullscreen.
    pub windowed: bool,
    /// Frame rate while a live panel is in prefill/decode.
    pub fps: u32,
    /// Frame rate while every live panel is idle.
    pub idle_fps: u32,
    /// Drop to `IDLE_FPS` when no panel was live for this long.
    pub idle_after: Duration,
    /// `--renderer`: decided per surface when its window is created.
    pub renderer: llm_wayvisuals::rendersel::RendererPref,
}

/// Frame rate when no panel has been live for `idle_after`.
pub const NO_SIGNAL_FPS: u32 = 2;
/// A higher rate is kept this long after activity stops (no flapping).
pub const HOLD: Duration = Duration::from_secs(2);
pub const DEFAULT_IDLE_AFTER: Duration = Duration::from_secs(10);

/// Display backend seam (winit today, layer-shell later).
pub trait Backend {
    /// Opens one window/surface per panel and runs until quit.
    fn run(&self, opts: RunOptions) -> Result<(), String>;
    /// Prints the available outputs on stdout.
    fn list_outputs(&self) -> Result<(), String>;
}

// ---------------------------------------------------------------- outputs

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// Index (the `index` of the matching `(name, index)` entry).
    Monitor(usize),
    /// No monitor: windowed fallback; `reason` is set when a named output
    /// could not be found (or none was left).
    Windowed { reason: Option<String> },
}

/// Resolves the output of each panel. `outputs` is `(name, index)` per
/// monitor. A requested output matches, in order: exact name, index ("0",
/// "1"...), case-insensitive substring of the name. Panels without request
/// take, in order, the monitors not claimed by a request.
pub fn assign_outputs(requests: &[Option<&str>], outputs: &[(String, usize)]) -> Vec<Target> {
    let find = |req: &str| -> Option<usize> {
        let req = req.trim();
        if let Some((_, i)) = outputs.iter().find(|(n, _)| n == req) {
            return Some(*i);
        }
        if let Ok(idx) = req.parse::<usize>() {
            if let Some((_, i)) = outputs.iter().find(|(_, i)| *i == idx) {
                return Some(*i);
            }
        }
        let lower = req.to_lowercase();
        if lower.is_empty() {
            return None;
        }
        outputs
            .iter()
            .find(|(n, _)| n.to_lowercase().contains(&lower))
            .map(|(_, i)| *i)
    };
    let out: Vec<Option<Target>> = requests
        .iter()
        .map(|r| {
            r.map(|req| match find(req) {
                Some(i) => Target::Monitor(i),
                None => Target::Windowed {
                    reason: Some(format!("output {req:?} not found")),
                },
            })
        })
        .collect();
    let mut free: Vec<usize> = outputs
        .iter()
        .map(|(_, i)| *i)
        .filter(|i| !out.iter().any(|t| matches!(t, Some(Target::Monitor(m)) if m == i)))
        .collect();
    free.reverse();
    out.into_iter()
        .map(|t| {
            t.unwrap_or_else(|| match free.pop() {
                Some(i) => Target::Monitor(i),
                None => Target::Windowed {
                    reason: Some("no free output left".into()),
                },
            })
        })
        .collect()
}

// ----------------------------------------------------------------- pacing

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activity {
    /// Not live (connecting, stale, down).
    Off,
    /// Live, selected model idle.
    Idle,
    /// Live, selected model in prefill or decode.
    Active,
}

/// Chooses the frame rate from the activity of all panels.
pub struct Pacer {
    fps: u32,
    idle_fps: u32,
    idle_after: Duration,
    last_live: Instant,
    last_active: Instant,
}

impl Pacer {
    pub fn new(fps: u32, idle_fps: u32, idle_after: Duration, now: Instant) -> Self {
        // Start as active so the first frames are smooth.
        Pacer { fps, idle_fps, idle_after, last_live: now, last_active: now }
    }

    pub fn update(&mut self, now: Instant, acts: impl IntoIterator<Item = Activity>) -> u32 {
        for a in acts {
            if a != Activity::Off {
                self.last_live = now;
            }
            if a == Activity::Active {
                self.last_active = now;
            }
        }
        if now.duration_since(self.last_live) > self.idle_after {
            NO_SIGNAL_FPS
        } else if now.duration_since(self.last_active) <= HOLD {
            self.fps
        } else {
            self.idle_fps
        }
    }
}

// ------------------------------------------------- grouping and geometry

/// Panels sharing one surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    /// Requested output (None: first free monitor / one window).
    pub output: Option<String>,
    /// Indexes of the panels, in order.
    pub panels: Vec<usize>,
}

/// Groups panels by requested output, in order of first appearance. All
/// panels without output form one group; the same output value (trimmed,
/// case-insensitive) forms one group. At most `max` panels per group: extra
/// panels start a new group with the same output.
pub fn group_panels(outputs: &[Option<&str>], max: usize) -> Vec<Group> {
    let norm = |o: &Option<&str>| o.map(|s| s.trim().to_lowercase());
    let mut groups: Vec<(Option<String>, Group)> = Vec::new();
    for (i, o) in outputs.iter().enumerate() {
        let key = norm(o);
        let hit = groups.iter_mut().find(|(k, g)| *k == key && g.panels.len() < max);
        match hit {
            Some((_, g)) => g.panels.push(i),
            None => groups.push((
                key,
                Group { output: o.map(|s| s.to_string()), panels: vec![i] },
            )),
        }
    }
    groups.into_iter().map(|(_, g)| g).collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Band {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// Bands of `n` stacked panels in a `w` x `h` surface. n = 1: one band of
/// `w` x min(h, w/4), centred vertically. n >= 2: n bands of height h/n that
/// fill the surface (the remainder pixels go to the first bands). Bands with
/// a zero side are returned with that zero size (callers skip them).
pub fn bands(w: u32, h: u32, n: usize) -> Vec<Band> {
    if n == 0 {
        return Vec::new();
    }
    if n == 1 {
        let bh = h.min(w / 4);
        return vec![Band { x: 0, y: (h - bh) / 2, w, h: bh }];
    }
    let n32 = n as u32;
    let (base, rem) = (h / n32, h % n32);
    let mut y = 0;
    (0..n32)
        .map(|i| {
            let bh = base + u32::from(i < rem);
            let b = Band { x: 0, y, w, h: bh };
            y += bh;
            b
        })
        .collect()
}

/// Default window size (physical) for a surface: 1920x1080, scaled down
/// (keeping 16:9) to 95 % of the screen when it does not fit.
pub fn default_window_size(screen: Option<(u32, u32)>) -> (u32, u32) {
    match screen {
        Some((sw, sh)) if sw > 0 && sh > 0 && (sw < 1920 || sh < 1080) => {
            let scale = (sw as f64 / 1920.0).min(sh as f64 / 1080.0) * 0.95;
            ((1920.0 * scale).floor() as u32, (1080.0 * scale).floor() as u32)
        }
        _ => (1920, 1080),
    }
}

// ------------------------------------------------------------ panel state

fn status_label(s: &LinkStatus) -> String {
    match s {
        LinkStatus::Connecting => "Connecting".into(),
        LinkStatus::Live => "Live".into(),
        LinkStatus::Stale => "Stale".into(),
        LinkStatus::Down(r) => format!("Down({r})"),
    }
}

pub fn signal_of(feed: &Feed) -> Signal {
    match &feed.status {
        LinkStatus::Live => Signal::Live,
        LinkStatus::Connecting => Signal::Connecting,
        LinkStatus::Stale => Signal::Stale {
            age_s: feed.received.map_or(0.0, |r| r.elapsed().as_secs_f32()),
        },
        LinkStatus::Down(r) => Signal::Down(r.clone()),
    }
}

/// 15 s series of the host CPU use (`system.cpu_pct`), one point per new
/// snapshot generation; a single gap while the feed is not live.
pub struct CpuTrack {
    hist: History,
    last_generation: Option<u64>,
    in_gap: bool,
}

impl CpuTrack {
    pub fn new() -> Self {
        CpuTrack { hist: History::new(Duration::from_secs(15)), last_generation: None, in_gap: false }
    }

    pub fn observe(&mut self, feed: &Feed, now: Instant) {
        if !feed.is_live() {
            if !self.in_gap {
                self.in_gap = true;
                self.hist.push(now, None);
            }
            return;
        }
        self.in_gap = false;
        if self.last_generation == Some(feed.generation) {
            return;
        }
        self.last_generation = Some(feed.generation);
        let v = feed.snapshot.as_ref().and_then(|s| s.system.as_ref()).and_then(|s| s.cpu_pct);
        self.hist.push(now, v);
    }

    pub fn samples(&self, now: Instant) -> Vec<(f32, Option<f32>)> {
        self.hist.samples(now)
    }
}

/// A newly reported bottleneck must last this long before it is shown.
const BOTTLENECK_SETTLE: Duration = Duration::from_millis(1000);
/// The shown bottleneck stays this long after it was last reported.
const BOTTLENECK_LINGER: Duration = Duration::from_millis(1500);

/// Steadies the highlighted bottleneck. llm-visuals' verdict is instantaneous
/// and real decodes flap between hops (e.g. "ram" and "pcie/gpu1" from one
/// sample to the next), which would make the red link jump around.
///
/// - a different bottleneck replaces the shown one only after it has been
///   reported continuously for [`BOTTLENECK_SETTLE`];
/// - the shown one is kept while it keeps being reported at least every
///   [`BOTTLENECK_LINGER`], then cleared.
#[derive(Default)]
pub struct BottleneckHold {
    shown: Option<(Bottleneck, Instant)>,
    candidate: Option<(BottleneckKey, Instant)>,
}

type BottleneckKey = (String, Option<u32>);

fn key_of(b: &Bottleneck) -> BottleneckKey {
    (b.stage.clone().unwrap_or_default(), b.gpu)
}

impl BottleneckHold {
    /// `current`: the bound bottleneck reported right now, if any.
    /// Returns the bottleneck to display.
    pub fn update(&mut self, current: Option<&Bottleneck>, now: Instant) -> Option<&Bottleneck> {
        match current {
            Some(b) if self.shown.as_ref().is_some_and(|(s, _)| key_of(s) == key_of(b)) => {
                self.shown = Some((b.clone(), now));
                self.candidate = None;
            }
            Some(b) => {
                let k = key_of(b);
                match &self.candidate {
                    Some((ck, since)) if *ck == k => {
                        if now.duration_since(*since) >= BOTTLENECK_SETTLE {
                            self.shown = Some((b.clone(), now));
                            self.candidate = None;
                        }
                    }
                    _ => self.candidate = Some((k, now)),
                }
            }
            None => self.candidate = None,
        }
        if self
            .shown
            .as_ref()
            .is_some_and(|(_, seen)| now.duration_since(*seen) > BOTTLENECK_LINGER)
        {
            self.shown = None;
        }
        self.shown.as_ref().map(|(b, _)| b)
    }
}

/// `snap` with the selected model's bottleneck replaced by `shown` (or marked
/// not bound when nothing is to be shown). `None` when nothing changes.
fn with_bottleneck(snap: &Snapshot, sel: &ModelSelect, shown: Option<&Bottleneck>) -> Option<Snapshot> {
    let m = snap.select(sel)?;
    let same = match (m.active_bottleneck(), shown) {
        (None, None) => true,
        (Some(a), Some(b)) => key_of(a) == key_of(b),
        _ => false,
    };
    if same {
        return None;
    }
    let idx = snap.models.iter().position(|x| std::ptr::eq(x, m))?;
    let mut out = snap.clone();
    let bw = out.models[idx].bandwidth.get_or_insert_with(Default::default);
    bw.bottleneck = Some(match shown {
        Some(b) => Bottleneck { bound: Some(true), ..b.clone() },
        None => Bottleneck { bound: Some(false), ..bw.bottleneck.clone().unwrap_or_default() },
    });
    Some(out)
}

/// Everything a panel needs besides its window.
pub struct PanelCore {
    pub cfg: PanelConfig,
    theme: Theme,
    source: Source,
    history: History,
    cpu: CpuTrack,
    renderer: Renderer,
    feed: Feed,
    last_label: String,
    hold: BottleneckHold,
    /// The feed's snapshot with the steadied bottleneck, when it differs.
    shown_snapshot: Option<Arc<Snapshot>>,
    /// (feed generation, shown bottleneck) `shown_snapshot` was built for.
    shown_stamp: Option<(u64, Option<BottleneckKey>)>,
}

impl PanelCore {
    pub fn new(cfg: PanelConfig) -> Result<Self, String> {
        let theme = theme::theme(&cfg.theme).ok_or_else(|| format!("unknown theme {:?}", cfg.theme))?;
        let source = Source::spawn(cfg.source.clone());
        let feed = source.feed();
        let last_label = status_label(&feed.status);
        log(format!(
            "panel {}: source {} ({})",
            cfg.name,
            cfg.source.describe(),
            last_label
        ));
        Ok(PanelCore {
            theme,
            source,
            history: History::new(Duration::from_secs(60)),
            cpu: CpuTrack::new(),
            renderer: Renderer::new(),
            feed,
            last_label,
            hold: BottleneckHold::default(),
            shown_snapshot: None,
            shown_stamp: None,
            cfg,
        })
    }

    /// Runs the bottleneck hold and rebuilds the displayed snapshot.
    fn steady_bottleneck(&mut self, now: Instant) {
        let snap = self.feed.snapshot.clone();
        let current = match (&snap, self.feed.is_live()) {
            (Some(s), true) => s.select(&self.cfg.model).and_then(|m| m.active_bottleneck()),
            _ => None,
        };
        let shown = self.hold.update(current, now).cloned();
        let stamp = (self.feed.generation, shown.as_ref().map(key_of));
        if self.shown_stamp.as_ref() == Some(&stamp) {
            return;
        }
        self.shown_stamp = Some(stamp);
        self.shown_snapshot = snap
            .as_deref()
            .and_then(|s| with_bottleneck(s, &self.cfg.model, shown.as_ref()))
            .map(Arc::new);
    }

    /// Polls the source, feeds the history, logs status changes (deduplicated).
    /// Returns the panel activity.
    pub fn tick(&mut self, now: Instant) -> Activity {
        self.feed = self.source.feed();
        self.history.observe(&self.feed, &self.cfg.model, now);
        self.cpu.observe(&self.feed, now);
        self.steady_bottleneck(now);
        let label = status_label(&self.feed.status);
        if label != self.last_label {
            log(format!("panel {}: {} -> {}", self.cfg.name, self.last_label, label));
            self.last_label = label;
        }
        if !self.feed.is_live() {
            return Activity::Off;
        }
        let phase = self
            .feed
            .snapshot
            .as_ref()
            .and_then(|s| s.select(&self.cfg.model))
            .map(|m| m.phase());
        match phase {
            Some(Phase::Prefill | Phase::Decode) => Activity::Active,
            _ => Activity::Idle,
        }
    }

    /// Calls `f` with the panel's current view.
    fn with_view<R>(&mut self, t: f32, f: impl FnOnce(&mut Renderer, &PanelView) -> R) -> R {
        let now = Instant::now();
        let wave = self.history.samples(now);
        let cpu_wave = self.cpu.samples(now);
        let view = PanelView {
            snapshot: self.shown_snapshot.as_deref().or(self.feed.snapshot.as_deref()),
            model: &self.cfg.model,
            signal: signal_of(&self.feed),
            theme: &self.theme,
            title: self.cfg.title.as_deref(),
            wave: &wave,
            wave_max: self.history.max().map(|m| m as f32),
            cpu_wave: &cpu_wave,
            t,
        };
        f(&mut self.renderer, &view)
    }

    /// Renders into the panel's persistent band pixmap; returns what changed.
    pub fn render(&mut self, pm: &mut Pixmap, t: f32) -> Damage {
        self.with_view(t, |r, view| r.render_incremental(pm, view))
    }

    /// GPU path: repaints what changed of the `w` x `h` band into its persistent
    /// image `band` of `gpu`; returns what changed (empty = nothing).
    #[cfg(feature = "gpu")]
    pub fn render_gpu(&mut self, gpu: &mut llm_wayvisuals::render::gl::GpuRenderer, band: usize, w: u32, h: u32, t: f32) -> Damage {
        self.with_view(t, |r, view| gpu.render_band_incremental(band, r, w, h, view))
    }
}

/// tiny-skia premultiplied RGBA8 -> softbuffer 0RGB. The background is opaque
/// so premultiplication does not matter.
pub fn rgba_to_xrgb(src: &[u8], dst: &mut [u32]) {
    for (d, p) in dst.iter_mut().zip(src.chunks_exact(4)) {
        *d = (p[0] as u32) << 16 | (p[1] as u32) << 8 | p[2] as u32;
    }
}

/// Same, writing a `band_w` x `band_h` pixmap into `dst` (row stride
/// `stride`) at (`x`, `y`). Out-of-range rows are skipped.
pub fn blit_xrgb(src: &[u8], band_w: usize, dst: &mut [u32], stride: usize, x: usize, y: usize) {
    for (row, srow) in src.chunks_exact(band_w * 4).enumerate() {
        let start = (y + row) * stride + x;
        let Some(d) = dst.get_mut(start..start + band_w) else { break };
        rgba_to_xrgb(srow, d);
    }
}

/// Coalesces damage rectangles: two rectangles are replaced by their bounding
/// box when that box is at most 1.25 times as large as both together (so
/// overlapping and neighbouring rectangles merge, far-apart ones do not).
/// The result covers every input rectangle.
pub fn coalesce(mut rects: Vec<Rect>) -> Vec<Rect> {
    let area = |r: &Rect| r.w as u64 * r.h as u64;
    let bound = |a: &Rect, b: &Rect| {
        let (x0, y0) = (a.x.min(b.x), a.y.min(b.y));
        let (x1, y1) = ((a.x + a.w).max(b.x + b.w), (a.y + a.h).max(b.y + b.h));
        Rect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 }
    };
    let mut changed = true;
    while changed {
        changed = false;
        let mut i = 0;
        while i < rects.len() {
            let mut j = i + 1;
            while j < rects.len() {
                let u = bound(&rects[i], &rects[j]);
                if area(&u) * 100 <= (area(&rects[i]) + area(&rects[j])) * 125 {
                    rects[i] = u;
                    rects.swap_remove(j);
                    changed = true;
                } else {
                    j += 1;
                }
            }
            i += 1;
        }
    }
    rects
}

/// Converts only the band-local rectangle `r` of a `band_w`-wide RGBA band
/// into `dst` (row stride `stride`), the band sitting at (`x`, `y`).
pub fn blit_xrgb_rect(src: &[u8], band_w: usize, dst: &mut [u32], stride: usize, x: usize, y: usize, r: &Rect) {
    let (rx, rw) = (r.x as usize, r.w as usize);
    for row in r.y as usize..(r.y + r.h) as usize {
        let s = (row * band_w + rx) * 4;
        let d = (y + row) * stride + x + rx;
        let (Some(srow), Some(drow)) = (src.get(s..s + rw * 4), dst.get_mut(d..d + rw)) else { break };
        rgba_to_xrgb(srow, drow);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coalesce_covers_inputs() {
        let r = |x, y, w, h| Rect { x, y, w, h };
        let input = vec![r(0, 0, 10, 10), r(8, 0, 10, 10), r(500, 300, 5, 5), r(9, 9, 3, 3)];
        let out = coalesce(input.clone());
        assert_eq!(out.len(), 2);
        for a in &input {
            assert!(out.iter().any(|b| a.x >= b.x && a.y >= b.y && a.x + a.w <= b.x + b.w && a.y + a.h <= b.y + b.h));
        }
    }

    #[test]
    fn rect_conversion() {
        // 3x2 band at (1,1) in a 5x4 surface; convert only its (1,1) 2x1 corner
        let mut src = Vec::new();
        for i in 0..6u8 {
            src.extend_from_slice(&[i, 0, 0, 255]);
        }
        let mut d = vec![0u32; 5 * 4];
        blit_xrgb_rect(&src, 3, &mut d, 5, 1, 1, &Rect { x: 1, y: 1, w: 2, h: 1 });
        assert_eq!(d[2 * 5 + 2], 4 << 16);
        assert_eq!(d[2 * 5 + 3], 5 << 16);
        assert_eq!(d.iter().filter(|v| **v != 0).count(), 2);
    }

    fn outs() -> Vec<(String, usize)> {
        ["HDMI-1", "eDP-1", "DP-2", "DP-3"]
            .iter()
            .enumerate()
            .map(|(i, n)| (n.to_string(), i))
            .collect()
    }

    #[test]
    fn exact_index_substring() {
        let t = assign_outputs(&[Some("DP-3"), Some("1"), Some("hdmi")], &outs());
        assert_eq!(t, vec![Target::Monitor(3), Target::Monitor(1), Target::Monitor(0)]);
        // ambiguous substring: first monitor in order ("dp" is in "eDP-1")
        assert_eq!(assign_outputs(&[Some("dp")], &outs()), vec![Target::Monitor(1)]);
        assert_eq!(assign_outputs(&[Some("dp-2")], &outs()), vec![Target::Monitor(2)]);
        assert_eq!(assign_outputs(&[Some("EDP-1")], &outs()), vec![Target::Monitor(1)]);
    }

    #[test]
    fn exact_name_beats_index() {
        let o = vec![("1".to_string(), 0), ("X".to_string(), 1)];
        assert_eq!(assign_outputs(&[Some("1")], &o), vec![Target::Monitor(0)]);
    }

    #[test]
    fn free_monitors_in_order_skip_claimed() {
        let t = assign_outputs(&[None, Some("HDMI-1"), None], &outs());
        assert_eq!(t, vec![Target::Monitor(1), Target::Monitor(0), Target::Monitor(2)]);
    }

    #[test]
    fn missing_falls_back_to_windowed() {
        let t = assign_outputs(&[Some("DP-9"), None], &outs());
        assert!(matches!(&t[0], Target::Windowed { reason: Some(r) } if r.contains("DP-9")));
        assert_eq!(t[1], Target::Monitor(0));
        let t = assign_outputs(&[None, None], &[("A".into(), 0)]);
        assert_eq!(t[0], Target::Monitor(0));
        assert!(matches!(t[1], Target::Windowed { .. }));
        let t = assign_outputs(&[None], &[]);
        assert!(matches!(t[0], Target::Windowed { .. }));
    }

    #[test]
    fn pacer_levels_and_hold() {
        use Activity::*;
        let t0 = Instant::now();
        let sec = Duration::from_secs;
        let mut p = Pacer::new(30, 15, sec(10), t0);
        assert_eq!(p.update(t0, [Active]), 30);
        // activity stops: hold 30 fps for 2 s, then 15
        assert_eq!(p.update(t0 + sec(1), [Idle]), 30);
        assert_eq!(p.update(t0 + sec(3), [Idle]), 15);
        // one active panel among idle ones
        assert_eq!(p.update(t0 + sec(4), [Idle, Active, Off]), 30);
        // nothing live for more than 10 s
        assert_eq!(p.update(t0 + sec(10), [Off, Off]), 15);
        assert_eq!(p.update(t0 + sec(15), [Off, Off]), 2);
        // recovery
        assert_eq!(p.update(t0 + sec(16), [Active]), 30);
    }

    #[test]
    fn grouping() {
        let g = group_panels(&[None, None, None], 3);
        assert_eq!(g, vec![Group { output: None, panels: vec![0, 1, 2] }]);
        let g = group_panels(&[Some("DP-2"), Some("DP-3"), Some("dp-2 ")], 3);
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].panels, vec![0, 2]);
        assert_eq!(g[1].panels, vec![1]);
        let g = group_panels(&[Some("DP-2"), None, Some("DP-3"), None], 3);
        assert_eq!(g.iter().map(|g| g.panels.clone()).collect::<Vec<_>>(), vec![vec![0], vec![1, 3], vec![2]]);
        assert!(group_panels(&[], 3).is_empty());
        // cap
        let g = group_panels(&[None, None, None, None], 3);
        assert_eq!(g.len(), 2);
        assert_eq!(g[1].panels, vec![3]);
    }

    #[test]
    fn band_geometry() {
        // n=1: native ratio, centred
        assert_eq!(bands(1920, 1080, 1), vec![Band { x: 0, y: 300, w: 1920, h: 480 }]);
        assert_eq!(bands(1920, 480, 1), vec![Band { x: 0, y: 0, w: 1920, h: 480 }]);
        assert_eq!(bands(1920, 300, 1), vec![Band { x: 0, y: 0, w: 1920, h: 300 }]);
        assert_eq!(bands(1921, 1081, 1)[0].y, (1081 - 480) / 2);
        // n=2,3 fill the surface
        let b = bands(1920, 1080, 3);
        assert_eq!(b.iter().map(|b| b.h).collect::<Vec<_>>(), vec![360, 360, 360]);
        assert_eq!(b[2].y, 720);
        let b = bands(1000, 1081, 3); // odd: remainder to the first band
        assert_eq!(b.iter().map(|b| b.h).collect::<Vec<_>>(), vec![361, 360, 360]);
        assert_eq!(b[1].y, 361);
        assert_eq!(b.iter().map(|b| b.h).sum::<u32>(), 1081);
        let b = bands(100, 5, 2);
        assert_eq!(b.iter().map(|b| b.h).collect::<Vec<_>>(), vec![3, 2]);
        // tiny
        let b = bands(3, 2, 3);
        assert_eq!(b.iter().map(|b| b.h).collect::<Vec<_>>(), vec![1, 1, 0]);
        assert_eq!(bands(2, 2, 1)[0].h, 0);
        assert!(bands(10, 10, 0).is_empty());
    }

    #[test]
    fn window_size() {
        assert_eq!(default_window_size(Some((1920, 1080))), (1920, 1080));
        assert_eq!(default_window_size(None), (1920, 1080));
        assert_eq!(default_window_size(Some((3840, 2160))), (1920, 1080));
        let (w, h) = default_window_size(Some((1366, 768)));
        assert!(w <= 1366 && h <= 768 && (w as f64 / h as f64 - 16.0 / 9.0).abs() < 0.02);
        let (w, h) = default_window_size(Some((1920, 480)));
        assert!(h <= 480 && w <= 1920);
    }

    #[test]
    fn blit_at_offset() {
        let mut d = vec![0u32; 4 * 3];
        // 2x1 band at x=1,y=1
        blit_xrgb(&[1, 2, 3, 255, 4, 5, 6, 255], 2, &mut d, 4, 1, 1);
        assert_eq!(d[5], 0x010203);
        assert_eq!(d[6], 0x040506);
        assert_eq!(d[4], 0);
        assert_eq!(d[7], 0);
        // too low: skipped without panic
        blit_xrgb(&[0; 8], 2, &mut d, 4, 3, 2);
    }

    #[test]
    fn cpu_track() {
        use llm_wayvisuals::model::{Snapshot, System};
        use std::sync::Arc;
        let feed = |gen, cpu: Option<f64>, status| Feed {
            snapshot: Some(Arc::new(Snapshot {
                system: Some(System { cpu_pct: cpu, ..Default::default() }),
                ..Default::default()
            })),
            received: Some(Instant::now()),
            status,
            generation: gen,
        };
        let t0 = Instant::now();
        let mut c = CpuTrack::new();
        c.observe(&feed(1, Some(40.0), LinkStatus::Live), t0);
        c.observe(&feed(1, Some(99.0), LinkStatus::Live), t0); // same generation: ignored
        c.observe(&feed(2, Some(60.0), LinkStatus::Live), t0 + Duration::from_secs(1));
        c.observe(&feed(2, Some(60.0), LinkStatus::Stale), t0 + Duration::from_secs(2));
        c.observe(&feed(2, Some(60.0), LinkStatus::Stale), t0 + Duration::from_secs(3)); // one gap only
        let s = c.samples(t0 + Duration::from_secs(3));
        assert_eq!(s.iter().map(|x| x.1).collect::<Vec<_>>(), vec![Some(40.0), Some(60.0), None]);
        // no `system` -> a gap point
        let mut c = CpuTrack::new();
        let mut f = feed(1, None, LinkStatus::Live);
        f.snapshot = Some(Arc::new(Snapshot::default()));
        c.observe(&f, t0);
        assert_eq!(c.samples(t0)[0].1, None);
        // 15 s window
        assert!(c.samples(t0 + Duration::from_secs(16)).is_empty());
    }

    #[test]
    fn pixel_conversion() {
        let mut d = [0u32; 2];
        rgba_to_xrgb(&[0x11, 0x22, 0x33, 0xff, 0xff, 0, 0, 0xff], &mut d);
        assert_eq!(d, [0x112233, 0xff0000]);
    }

    fn bn(stage: &str, gpu: Option<u32>) -> Bottleneck {
        Bottleneck { stage: Some(stage.into()), gpu, bound: Some(true), ..Default::default() }
    }

    fn shown(h: &mut BottleneckHold, cur: Option<&Bottleneck>, at: Instant) -> Option<BottleneckKey> {
        h.update(cur, at).map(key_of)
    }

    #[test]
    fn bottleneck_settles_before_showing() {
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        let ram = bn("ram", None);
        let mut h = BottleneckHold::default();
        assert_eq!(shown(&mut h, Some(&ram), ms(0)), None);
        assert_eq!(shown(&mut h, Some(&ram), ms(900)), None);
        assert_eq!(shown(&mut h, Some(&ram), ms(1000)), Some(("ram".into(), None)));
    }

    #[test]
    fn flapping_keeps_the_first_bottleneck() {
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        let (ram, pcie) = (bn("ram", None), bn("pcie", Some(1)));
        let mut h = BottleneckHold::default();
        shown(&mut h, Some(&ram), ms(0));
        shown(&mut h, Some(&ram), ms(1000));
        // ram / pcie alternate every 200 ms for 5 s: ram stays shown.
        for i in 0..25u64 {
            let cur = if i % 2 == 0 { &pcie } else { &ram };
            assert_eq!(shown(&mut h, Some(cur), ms(1200 + i * 200)), Some(("ram".into(), None)));
        }
    }

    #[test]
    fn sustained_change_takes_over_and_silence_clears() {
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        let (ram, pcie) = (bn("ram", None), bn("pcie", Some(1)));
        let mut h = BottleneckHold::default();
        shown(&mut h, Some(&ram), ms(0));
        shown(&mut h, Some(&ram), ms(1000));
        assert_eq!(shown(&mut h, Some(&pcie), ms(1200)), Some(("ram".into(), None)));
        assert_eq!(shown(&mut h, Some(&pcie), ms(2200)), Some(("pcie".into(), Some(1))));
        // No bound bottleneck: kept 1.5 s, then cleared.
        assert_eq!(shown(&mut h, None, ms(3600)), Some(("pcie".into(), Some(1))));
        assert_eq!(shown(&mut h, None, ms(3800)), None);
        // A different gpu index is a different bottleneck.
        let pcie0 = bn("pcie", Some(0));
        shown(&mut h, Some(&pcie), ms(4000));
        shown(&mut h, Some(&pcie), ms(5000));
        assert_eq!(shown(&mut h, Some(&pcie0), ms(5100)), Some(("pcie".into(), Some(1))));
    }

    #[test]
    fn displayed_snapshot_carries_the_steady_bottleneck() {
        use llm_wayvisuals::model::{Bandwidth, Model};
        let mut snap = Snapshot::default();
        snap.models.push(Model {
            bandwidth: Some(Bandwidth { bottleneck: Some(bn("pcie", Some(1))), ..Default::default() }),
            ..Default::default()
        });
        let sel = ModelSelect::Auto;
        // Reported pcie, shown ram → patched to ram.
        let out = with_bottleneck(&snap, &sel, Some(&bn("ram", None))).unwrap();
        assert_eq!(out.models[0].active_bottleneck().map(key_of), Some(("ram".into(), None)));
        // Reported pcie, nothing shown (still settling) → not bound.
        let out = with_bottleneck(&snap, &sel, None).unwrap();
        assert!(out.models[0].active_bottleneck().is_none());
        // Same as reported → no copy.
        assert!(with_bottleneck(&snap, &sel, Some(&bn("pcie", Some(1)))).is_none());
    }
}

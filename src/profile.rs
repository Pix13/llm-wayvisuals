//! Opt-in frame profiler (`LLMV_PROFILE=1`; `LLMV_PROFILE=tiles` also hashes
//! 64x64 tiles to estimate the changed area, which costs ~2 ms per band and
//! is therefore excluded from the timings but inflates the CPU %). Zero cost when off: every entry
//! point starts with one relaxed atomic load. Single-threaded by design (the
//! UI thread), statistics live in a thread-local.
//!
//! Every 5 s [`report`] prints one stderr line per category (mean / p95 in ms,
//! share of busy time), the achieved fps, the process CPU % and the
//! percentage of 64x64 tiles that changed since the previous frame.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cat {
    /// `PanelCore::tick` for all panels (feed, history, bottleneck hold).
    Tick,
    /// Whole `redraw` of one surface (sum of the items below + glue).
    Frame,
    /// Pixmap render of one band (sum of the render items below).
    Render,
    /// Copy of the cached static layer into the band pixmap.
    StaticBlit,
    /// Top dynamic cards (GPU, model, ...).
    TopCards,
    /// Bottom dynamic cards.
    BottomCards,
    /// Data-path diagram.
    DataPath,
    /// CPU card.
    CpuCard,
    /// Centre card: rain + digits + labels + waveform.
    Centre,
    /// Matrix rain only (included in `Centre`).
    Rain,
    /// Title bar, NO SIGNAL overlay and frozen-image copy.
    Overlay,
    /// Recording of the live frame's draw operations (incremental render).
    Record,
    /// Frame diff and dirty-rectangle closure.
    Diff,
    /// Restore + repaint of the dirty rectangles.
    Replay,
    /// RGBA -> 0RGB conversion into the softbuffer buffer.
    Convert,
    /// `surface.buffer_mut()` (acquire / wait for a free buffer).
    Acquire,
    /// `buffer.present()`.
    Present,
    /// GPU path: femtovg tessellation + GL calls of a flush (CPU side).
    GlSubmit,
    /// GPU path: `swap_buffers`.
    GlSwap,
    /// GPU path: vector fills / strokes / rects handed to femtovg (recording).
    GlVector,
    /// GPU path: text runs (glyph atlas lookups + quads).
    GlText,
    /// GPU path: coverage blits (rain, digits).
    GlCov,
    /// GPU path: static layer restores.
    GlRestore,
}

const CATS: [(Cat, &str); 23] = [
    (Cat::Tick, "tick/feed"),
    (Cat::Frame, "frame (redraw)"),
    (Cat::Render, "render"),
    (Cat::StaticBlit, "  static blit"),
    (Cat::TopCards, "  top cards"),
    (Cat::BottomCards, "  bottom cards"),
    (Cat::DataPath, "  data path"),
    (Cat::CpuCard, "  cpu card"),
    (Cat::Centre, "  centre"),
    (Cat::Rain, "    rain"),
    (Cat::Overlay, "  overlay/title"),
    (Cat::Record, "  record ops"),
    (Cat::Diff, "  diff"),
    (Cat::Replay, "  restore+replay"),
    (Cat::Convert, "convert"),
    (Cat::Acquire, "acquire buf"),
    (Cat::Present, "present"),
    (Cat::GlSubmit, "gl submit"),
    (Cat::GlSwap, "gl swap"),
    (Cat::GlVector, "  gl vector ops"),
    (Cat::GlText, "  gl text ops"),
    (Cat::GlCov, "  gl cov ops"),
    (Cat::GlRestore, "  gl restore"),
];

static STATE: AtomicU8 = AtomicU8::new(0); // 0 unknown, 1 off, 2 on, 3 on + tiles

#[inline]
pub fn enabled() -> bool {
    match STATE.load(Ordering::Relaxed) {
        1 => false,
        2 | 3 => true,
        _ => {
            let v = std::env::var("LLMV_PROFILE").unwrap_or_default();
            let st = match v.as_str() {
                "" | "0" => 1,
                "tiles" => 3,
                _ => 2,
            };
            STATE.store(st, Ordering::Relaxed);
            st > 1
        }
    }
}

const TILE: usize = 64;

struct Stats {
    samples: Vec<Vec<f64>>,
    frames: u32,
    since: Instant,
    cpu_ticks: u64,
    /// (surface, band) -> previous tile hashes
    tiles: HashMap<(usize, usize), Vec<u64>>,
    /// (surface, band) -> changed-tile percentages this period
    changed: HashMap<(usize, usize), Vec<f64>>,
    /// Incremental renders: (repainted area fraction, rects, ops replayed, ops recorded).
    damage: Vec<(f64, usize, usize, usize)>,
}

thread_local! {
    static STATS: RefCell<Option<Stats>> = const { RefCell::new(None) };
}

fn with<R>(f: impl FnOnce(&mut Stats) -> R) -> R {
    STATS.with(|s| {
        let mut s = s.borrow_mut();
        let st = s.get_or_insert_with(|| Stats {
            samples: vec![Vec::new(); CATS.len()],
            frames: 0,
            since: Instant::now(),
            cpu_ticks: cpu_ticks(),
            tiles: HashMap::new(),
            changed: HashMap::new(),
            damage: Vec::new(),
        });
        f(st)
    })
}

/// Guard recording the elapsed time of its scope into `cat` (no-op when off).
pub struct Scope(Option<(Cat, Instant)>);

#[inline]
pub fn scope(cat: Cat) -> Scope {
    Scope(enabled().then(|| (cat, Instant::now())))
}

impl Drop for Scope {
    fn drop(&mut self) {
        if let Some((cat, t0)) = self.0.take() {
            let ms = t0.elapsed().as_secs_f64() * 1000.0;
            with(|s| s.samples[cat as usize].push(ms));
        }
    }
}

fn cpu_ticks() -> u64 {
    let Ok(s) = std::fs::read_to_string("/proc/self/stat") else { return 0 };
    // fields after the ")" closing the comm: state is #3, utime #14, stime #15
    let Some(rest) = s.rsplit_once(')').map(|(_, r)| r) else { return 0 };
    let f: Vec<&str> = rest.split_whitespace().collect();
    let g = |i: usize| f.get(i).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
    g(11) + g(12)
}

fn hash_tile(data: &[u8], w: usize, x0: usize, y0: usize, x1: usize, y1: usize) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for y in y0..y1 {
        let row = &data[(y * w + x0) * 4..(y * w + x1) * 4];
        for c in row.chunks_exact(8) {
            let v = u64::from_le_bytes(c.try_into().unwrap());
            h = (h ^ v).wrapping_mul(0x100000001b3).rotate_left(5);
        }
    }
    h
}

/// Records the % of 64x64 tiles of the band pixmap (`w` x `h`, RGBA) that
/// differ from the previous frame of the same (surface, band).
pub fn tiles(surface: usize, band: usize, data: &[u8], w: usize, h: usize) {
    if !enabled() || STATE.load(Ordering::Relaxed) != 3 || w == 0 || h == 0 {
        return;
    }
    let (tx, ty) = (w.div_ceil(TILE), h.div_ceil(TILE));
    let mut hashes = Vec::with_capacity(tx * ty);
    for j in 0..ty {
        for i in 0..tx {
            hashes.push(hash_tile(data, w, i * TILE, j * TILE, ((i + 1) * TILE).min(w), ((j + 1) * TILE).min(h)));
        }
    }
    with(|s| {
        if let Some(prev) = s.tiles.get(&(surface, band)).filter(|p| p.len() == hashes.len()) {
            let n = prev.iter().zip(&hashes).filter(|(a, b)| a != b).count();
            s.changed.entry((surface, band)).or_default().push(100.0 * n as f64 / hashes.len() as f64);
        }
        s.tiles.insert((surface, band), hashes);
    });
}

/// Records one incremental band render: repainted area fraction (0..1),
/// number of rectangles, ops replayed and ops recorded.
pub fn damage(area_frac: f64, rects: usize, replayed: usize, recorded: usize) {
    if !enabled() {
        return;
    }
    with(|s| s.damage.push((area_frac, rects, replayed, recorded)));
}

/// Counts one finished frame and prints the report every 5 s.
pub fn frame_done() {
    if !enabled() {
        return;
    }
    with(|s| {
        s.frames += 1;
        let el = s.since.elapsed();
        if el < Duration::from_secs(5) {
            return;
        }
        let secs = el.as_secs_f64();
        let ticks = cpu_ticks();
        let cpu = (ticks.saturating_sub(s.cpu_ticks)) as f64 / 100.0 / secs * 100.0;
        // busy time = redraws + ticks
        let sum = |c: Cat| s.samples[c as usize].iter().sum::<f64>();
        let busy = (sum(Cat::Frame) + sum(Cat::Tick)).max(1e-9);
        eprintln!(
            "[profile] {:.1} s: {:.1} fps, cpu {:.1} %, busy {:.1} % of wall",
            secs,
            s.frames as f64 / secs,
            cpu,
            busy / (secs * 10.0)
        );
        for (i, (_, name)) in CATS.iter().enumerate() {
            let v = &mut s.samples[i];
            if v.is_empty() {
                continue;
            }
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let mean = v.iter().sum::<f64>() / v.len() as f64;
            let p95 = v[((v.len() as f64 * 0.95) as usize).min(v.len() - 1)];
            eprintln!(
                "[profile]   {:<16} n={:<4} mean {:>6.3} ms  p95 {:>6.3} ms  share {:>5.1} %",
                name,
                v.len(),
                mean,
                p95,
                100.0 * v.iter().sum::<f64>() / busy
            );
            v.clear();
        }
        let mut keys: Vec<_> = s.changed.keys().copied().collect();
        keys.sort();
        for k in keys {
            let v = &s.changed[&k];
            if v.is_empty() {
                continue;
            }
            let mean = v.iter().sum::<f64>() / v.len() as f64;
            let max = v.iter().cloned().fold(0.0, f64::max);
            eprintln!("[profile]   tiles surface {} band {}: {:.1} % changed (mean), max {:.1} %", k.0, k.1, mean, max);
        }
        s.changed.clear();
        if !s.damage.is_empty() {
            let n = s.damage.len() as f64;
            let area = s.damage.iter().map(|d| d.0).sum::<f64>() / n * 100.0;
            let max = s.damage.iter().map(|d| d.0).fold(0.0, f64::max) * 100.0;
            let rects = s.damage.iter().map(|d| d.1 as f64).sum::<f64>() / n;
            let (rep, rec) = (s.damage.iter().map(|d| d.2 as f64).sum::<f64>() / n, s.damage.iter().map(|d| d.3 as f64).sum::<f64>() / n);
            eprintln!("[profile]   repainted: {area:.1} % of the band area (max {max:.1} %), {rects:.1} rects, {rep:.0} of {rec:.0} ops replayed per band frame");
            s.damage.clear();
        }
        s.frames = 0;
        s.since = Instant::now();
        s.cpu_ticks = ticks;
    });
}

mod cli;
mod frontend;

use clap::Parser;
use cli::{Cli, Command, RenderPngArgs, RunArgs};
use frontend::{log, Backend, RunOptions, WinitBackend, DEFAULT_IDLE_AFTER};
use llm_wayvisuals::config::Config;
use llm_wayvisuals::model::{ModelSelect, Snapshot};
use llm_wayvisuals::render::{PanelView, Renderer, Signal, HEIGHT, WIDTH};
use llm_wayvisuals::rendersel::{self, RendererPref};
use llm_wayvisuals::theme;
use std::time::Duration;

/// SYNTHETIC history for PNG rendering: a plausible decode_tps series
/// around the snapshot's current value (the real one comes from `history`).
fn synth_wave(snap: &Snapshot) -> (Vec<(f32, Option<f32>)>, Option<f32>) {
    let base = snap
        .select(&ModelSelect::Auto)
        .and_then(|m| m.decode_tps)
        .filter(|v| *v > 0.0);
    let Some(base) = base else {
        // idle: a flat trace with a decaying burst
        let v = (0..120)
            .map(|i| {
                let age = 1.0 - i as f32 / 119.0;
                (age, Some(if age > 0.55 && age < 0.75 { 40.0 * (1.0 - (age - 0.55) / 0.2) } else { 0.0 }))
            })
            .collect();
        return (v, Some(60.0));
    };
    let base = base as f32;
    let v = (0..120)
        .map(|i| {
            let age = 1.0 - i as f32 / 119.0;
            let saw = ((i * 7) % 11) as f32 / 11.0 - 0.5;
            let slow = (i as f32 * 0.21).sin() * 0.12;
            let gap = (50..56).contains(&i);
            (age, if gap { None } else { Some((base * (1.0 + slow + saw * 0.18)).max(0.0)) })
        })
        .collect();
    (v, Some(base * 1.4))
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        None => run(cli.run),
        Some(Command::Run(a)) => run(a),
        Some(Command::RenderPng(a)) => render_png(a),
        Some(Command::BenchRender(a)) => bench(a),
        #[cfg(feature = "gpu")]
        Some(Command::CompareRenderers(a)) => compare(a),
    }
}

/// Availability probe of the GPU path (headless hardware GL context).
fn gpu_probe() -> Result<String, String> {
    #[cfg(feature = "gpu")]
    {
        llm_wayvisuals::render::gl::probe()
    }
    #[cfg(not(feature = "gpu"))]
    {
        Err("built without the gpu feature".into())
    }
}

/// Renderer choice of the offscreen tools (`render-png`, `bench-render`):
/// `--renderer` only (default cpu), no environment variable.
fn tool_uses_gpu(v: &str) -> bool {
    let pref = v.parse::<RendererPref>().unwrap_or_else(|e| fail(2, e));
    let choice = rendersel::decide(pref, cfg!(feature = "gpu"), gpu_probe).unwrap_or_else(|e| fail(2, e));
    log(choice.log_line());
    choice.is_gpu()
}

#[cfg(feature = "gpu")]
fn compare(a: cli::CompareArgs) {
    let sizes: Vec<(u32, u32)> = a
        .sizes
        .split(',')
        .map(|s| {
            s.split_once('x')
                .and_then(|(w, h)| Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?)))
                .unwrap_or_else(|| fail(2, format!("bad size {s:?}")))
        })
        .collect();
    match llm_wayvisuals::render::gl::compare::compare_all(a.fixtures.as_ref(), a.out.as_ref(), &sizes, a.all) {
        Ok(true) => println!("all fixtures within thresholds (PSNR >= 35 dB, <= 1 % pixels off by > 16)"),
        Ok(false) => fail(1, "some fixtures are out of thresholds (side-by-side PNGs written)"),
        Err(e) => fail(1, e),
    }
}

fn fail(code: i32, msg: impl AsRef<str>) -> ! {
    eprintln!("llm-wayvisuals: {}", msg.as_ref());
    std::process::exit(code)
}

fn run(a: RunArgs) {
    let backend = WinitBackend;
    if a.list_outputs {
        if let Err(e) = backend.list_outputs() {
            fail(1, e);
        }
        return;
    }
    let default_cfg = Config::default_path();
    let cfg = if !a.servers.is_empty() {
        let existing = a.config.clone().or_else(|| default_cfg.exists().then(|| default_cfg.clone()));
        if let Some(p) = existing {
            log(format!("--server given: ignoring config file {}", p.display()));
        }
        let cfg = cli::config_from_servers(&a.servers).unwrap_or_else(|e| fail(2, e));
        for w in &cfg.warnings {
            log(format!("warning: {w}"));
        }
        cfg
    } else if let Some(p) = &a.config {
        Config::load(p).unwrap_or_else(|e| fail(2, e))
    } else if default_cfg.exists() {
        Config::load(&default_cfg).unwrap_or_else(|e| fail(2, e))
    } else {
        fail(
            2,
            format!(
                "no panel configured: pass --server SOURCE[,output=NAME][,theme=NAME]... (1 to 3 times), \
                 --config PATH, or create {}\nexample: llm-wayvisuals --server /run/user/1000/llmv-0.json,output=DP-2 \
                 --server http://gpubox1.example.lan:9464,token-file=~/.t",
                default_cfg.display()
            ),
        )
    };
    // Hidden test knob: seconds without any live panel before dropping to 2 fps.
    let idle_after = std::env::var("LLMV_IDLE_AFTER_S")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .map_or(DEFAULT_IDLE_AFTER, Duration::from_secs_f64);
    // Renderer: decided per surface when its window is created (see the backend).
    let renderer = rendersel::resolve_pref(a.renderer.as_deref(), std::env::var("LLMV_RENDERER").ok().as_deref()).unwrap_or_else(|e| fail(2, e));
    let opts = RunOptions { panels: cfg.panels, windowed: a.window, fps: a.fps, idle_fps: a.idle_fps.min(a.fps), idle_after, renderer };
    if let Err(e) = backend.run(opts) {
        fail(1, e);
    }
}

fn render_png(a: RenderPngArgs) {
    let th = theme::theme(&a.theme).unwrap_or_else(|| {
        eprintln!("unknown theme (one of {:?})", theme::THEMES);
        std::process::exit(2)
    });
    let signal = match a.signal.as_str() {
        "live" => Signal::Live,
        "stale" => Signal::Stale { age_s: 12.0 },
        "down" => Signal::Down("connection refused".into()),
        "connecting" => Signal::Connecting,
        other => fail(2, format!("unknown signal {other:?} (live|stale|down|connecting)")),
    };
    let (w, h) = a
        .size
        .split_once('x')
        .and_then(|(w, h)| Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?)))
        .unwrap_or((WIDTH, HEIGHT));
    let (input, out) = (&a.input, &a.out);
    let json = std::fs::read_to_string(input).unwrap_or_else(|e| {
        eprintln!("{input}: {e}");
        std::process::exit(1)
    });
    let snap = Snapshot::parse(&json).unwrap_or_else(|e| {
        eprintln!("{input}: {e}");
        std::process::exit(1)
    });
    let (wave, wave_max) = synth_wave(&snap);
    let sel = ModelSelect::Auto;
    // SYNTHETIC 15 s CPU history around the snapshot's current system.cpu_pct
    // (the real one will come from a History fed by the frontend).
    let cpu_wave: Vec<(f32, Option<f32>)> = match snap.system.as_ref().and_then(|s| s.cpu_pct) {
        Some(c) => (0..15)
            .map(|i| (1.0 - i as f32 / 14.0, Some((c as f32 * (1.0 + 0.12 * (i as f32 * 1.7).sin())).clamp(0.0, 100.0))))
            .collect(),
        None => Vec::new(),
    };
    let view = PanelView {
        snapshot: Some(&snap),
        model: &sel,
        signal,
        theme: &th,
        title: None,
        wave: &wave,
        wave_max,
        cpu_wave: &cpu_wave,
        t: a.t,
    };
    let mut pm = tiny_skia::Pixmap::new(w, h).expect("size");
    if tool_uses_gpu(&a.renderer) {
        #[cfg(feature = "gpu")]
        {
            let mut g = llm_wayvisuals::render::gl::GpuRenderer::new_headless().unwrap_or_else(|e| fail(1, e));
            g.render(&mut Renderer::new(), w, h, &view);
            pm = g.read_pixmap().unwrap_or_else(|| fail(1, "GPU readback failed"));
        }
    } else {
        Renderer::new().render(&mut pm, &view);
    }
    pm.save_png(out).expect("write png");
    eprintln!("wrote {out} ({w}x{h})");
}

fn bench(a: cli::BenchArgs) {
    let parse = |n: &str| match n {
        "live" => Signal::Live,
        "stale" => Signal::Stale { age_s: 12.0 },
        "down" => Signal::Down("connection refused".into()),
        "connecting" => Signal::Connecting,
        other => fail(2, format!("unknown signal {other:?} (live|stale|down|connecting)")),
    };
    let cases: Vec<(String, bool)> = match &a.signal {
        Some(s) => vec![(s.clone(), !a.no_snapshot)],
        None => ["live", "stale", "down", "connecting"]
            .iter()
            .map(|s| (s.to_string(), true))
            .chain([("down".to_string(), false), ("connecting".to_string(), false)])
            .collect(),
    };
    let (w, h) = a
        .size
        .split_once('x')
        .and_then(|(w, h)| Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?)))
        .filter(|&(w, h)| w > 0 && h > 0)
        .unwrap_or_else(|| fail(2, format!("bad --size {:?} (expected WIDTHxHEIGHT)", a.size)));
    let gpu = tool_uses_gpu(&a.renderer);
    println!("render {w}x{h}, {} frames each, {}", a.frames, if gpu { "gpu (incl. glFinish)" } else { "cpu" });
    for (name, with) in cases {
        let (mean, max) = if gpu {
            #[cfg(feature = "gpu")]
            {
                llm_wayvisuals::render::gl::compare::bench_size(parse(&name), a.frames, with, w, h).unwrap_or_else(|e| fail(1, e))
            }
            #[cfg(not(feature = "gpu"))]
            unreachable!()
        } else {
            llm_wayvisuals::render::bench_size(parse(&name), a.frames, with, w, h)
        };
        let data = if with { "snapshot" } else { "no snapshot" };
        println!("{name:<11} {data:<12} mean {mean:6.2} ms  max {max:6.2} ms");
    }
}

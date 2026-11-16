//! CPU vs GPU comparison harness (`llm-wayvisuals compare-renderers`) and the
//! GPU benchmark helper.

use std::path::Path;
use std::time::Instant;

use tiny_skia::Pixmap;

use super::GpuRenderer;
use crate::model::{ModelSelect, Snapshot};
use crate::render::{PanelView, Renderer, Signal};

#[derive(Debug, Clone, Copy)]
pub struct Diff {
    pub max_abs: u8,
    pub pct_gt8: f64,
    pub pct_gt16: f64,
    pub psnr: f64,
}

/// Per-pixel statistics of `b` against `a` (same size, RGBA bytes).
pub fn diff(a: &[u8], b: &[u8]) -> Diff {
    let n = a.len() / 4;
    let (mut max_abs, mut g8, mut g16, mut sq) = (0u8, 0usize, 0usize, 0f64);
    for (pa, pb) in a.chunks_exact(4).zip(b.chunks_exact(4)) {
        let mut worst = 0u8;
        for c in 0..4 {
            let d = pa[c].abs_diff(pb[c]);
            worst = worst.max(d);
            if c < 3 {
                sq += (d as f64) * (d as f64);
            }
        }
        max_abs = max_abs.max(worst);
        g8 += (worst > 8) as usize;
        g16 += (worst > 16) as usize;
    }
    let mse = sq / (n as f64 * 3.0);
    let psnr = if mse == 0.0 { f64::INFINITY } else { 10.0 * (255.0f64 * 255.0 / mse).log10() };
    Diff { max_abs, pct_gt8: 100.0 * g8 as f64 / n as f64, pct_gt16: 100.0 * g16 as f64 / n as f64, psnr }
}

fn waves(snap: &Snapshot) -> (Vec<(f32, Option<f32>)>, Vec<(f32, Option<f32>)>) {
    let wave = (0..120).map(|i| (1.0 - i as f32 / 119.0, Some(50.0 + ((i * 7) % 11) as f32 * 2.0))).collect();
    let c = snap.system.as_ref().and_then(|s| s.cpu_pct).unwrap_or(0.0) as f32;
    let cpu = (0..15).map(|i| (1.0 - i as f32 / 14.0, Some((c * (1.0 + 0.12 * (i as f32 * 1.7).sin())).clamp(0.0, 100.0)))).collect();
    (wave, cpu)
}

/// Side by side `[cpu | gpu | amplified |diff|]` PNG.
fn save_side_by_side(path: &Path, cpu: &Pixmap, gpu: &Pixmap) -> Result<(), String> {
    let (w, h) = (cpu.width(), cpu.height());
    let mut out = Pixmap::new(w, h * 3).ok_or("pixmap")?;
    let (o, a, b) = (out.data_mut(), cpu.data(), gpu.data());
    let n = (w * h * 4) as usize;
    o[..n].copy_from_slice(a);
    o[n..2 * n].copy_from_slice(b);
    for i in (0..n).step_by(4) {
        for c in 0..3 {
            o[2 * n + i + c] = (a[i + c].abs_diff(b[i + c]) as u32 * 4).min(255) as u8;
        }
        o[2 * n + i + 3] = 255;
    }
    out.save_png(path).map_err(|e| e.to_string())
}

/// Renders every fixture on both backends; returns `Ok(true)` if all pass
/// (PSNR >= 35 dB and <= 1 % of pixels differing by more than 16).
pub fn compare_all(fixtures: &Path, out: &Path, sizes: &[(u32, u32)], save_all: bool) -> Result<bool, String> {
    let mut gpu = GpuRenderer::new_headless()?;
    eprintln!("renderer: gpu ({})", gpu.info().describe());
    std::fs::create_dir_all(out).map_err(|e| e.to_string())?;
    let mut files: Vec<_> = std::fs::read_dir(fixtures)
        .map_err(|e| format!("{}: {e}", fixtures.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    files.sort();
    let (mut rc, mut rg) = (Renderer::new(), Renderer::new());
    let sel = ModelSelect::Auto;
    let mut all_ok = true;
    println!("{:<30} {:>9} {:<6} {:<5} {:>7} {:>8} {:>8} {:>8}", "fixture", "size", "theme", "sig", "max", ">8 %", ">16 %", "PSNR dB");
    for f in &files {
        let snap = Snapshot::parse(&std::fs::read_to_string(f).map_err(|e| e.to_string())?).map_err(|e| format!("{}: {e}", f.display()))?;
        let (wave, cpu_wave) = waves(&snap);
        let name = f.file_stem().unwrap().to_string_lossy().to_string();
        for &(w, h) in sizes {
            for (ti, tname) in ["green", "gold"].iter().enumerate() {
                let th = crate::theme::theme(tname).ok_or("theme")?;
                for (sname, signal) in [("live", Signal::Live), ("stale", Signal::Stale { age_s: 12.0 }), ("down", Signal::Down("refused".into()))] {
                    if ti == 1 && sname != "live" {
                        continue;
                    }
                    let view = PanelView {
                        snapshot: Some(&snap), model: &sel, signal, theme: &th, title: None,
                        wave: &wave, wave_max: Some(90.0), cpu_wave: &cpu_wave, t: 1.3,
                    };
                    let mut a = Pixmap::new(w, h).ok_or("pixmap")?;
                    rc.render(&mut a, &view);
                    gpu.render(&mut rg, w, h, &view);
                    let b = gpu.read_pixmap().ok_or("readback")?;
                    let d = diff(a.data(), b.data());
                    let ok = d.psnr >= 35.0 && d.pct_gt16 <= 1.0;
                    all_ok &= ok;
                    println!(
                        "{:<30} {:>9} {:<6} {:<5} {:>7} {:>8.3} {:>8.3} {:>8.2} {}",
                        name, format!("{w}x{h}"), tname, sname, d.max_abs, d.pct_gt8, d.pct_gt16, d.psnr, if ok { "" } else { "FAIL" }
                    );
                    if !ok || save_all {
                        save_side_by_side(&out.join(format!("{name}-{w}x{h}-{tname}-{sname}.png")), &a, &b)?;
                    }
                }
            }
        }
    }
    Ok(all_ok)
}

/// Mean / max ms per frame on the GPU (each frame ends with a glFinish), same
/// scenario as `render::bench_size`.
pub fn bench_size(signal: Signal, frames: usize, with_snapshot: bool, w: u32, h: u32) -> Result<(f64, f64), String> {
    let snap = Snapshot::parse(include_str!("../../../fixtures/synthetic-v2-A.json")).unwrap();
    let th = crate::theme::theme("green").unwrap();
    let wave: Vec<(f32, Option<f32>)> = (0..120).map(|i| (1.0 - i as f32 / 119.0, Some(50.0 + (i % 11) as f32))).collect();
    let sel = ModelSelect::Auto;
    let mut r = Renderer::new();
    let mut g = GpuRenderer::new_headless()?;
    let mk = |t: f32, signal: Signal| PanelView {
        snapshot: with_snapshot.then_some(&snap), model: &sel, signal, theme: &th, title: None,
        wave: &wave, wave_max: Some(80.0), cpu_wave: &[], t,
    };
    g.render(&mut r, w, h, &mk(0.0, signal.clone()));
    g.finish();
    let (mut sum, mut max, mut rec, mut sub) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
    for i in 0..frames {
        let t0 = Instant::now();
        g.render(&mut r, w, h, &mk(i as f32 / 30.0, signal.clone()));
        rec += g.last_record_ms;
        sub += g.last_submit_ms;
        g.finish();
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        sum += ms;
        max = max.max(ms);
    }
    let n = frames.max(1) as f64;
    if std::env::var_os("LLMV_GL_TIMING").is_some() {
        eprintln!("  cpu-side per frame: record {:.2} ms, submit {:.2} ms (rest = GPU wait)", rec / n, sub / n);
    }
    Ok((sum / n, max))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_stats() {
        let a = vec![10u8; 16];
        assert_eq!(diff(&a, &a).max_abs, 0);
        assert!(diff(&a, &a).psnr.is_infinite());
        let mut b = a.clone();
        b[0] = 40; // one pixel, one channel off by 30
        let d = diff(&a, &b);
        assert_eq!(d.max_abs, 30);
        assert_eq!(d.pct_gt16, 25.0);
        assert!((d.psnr - 29.4).abs() < 0.1, "{}", d.psnr);
    }

    /// Needs a hardware GL context: skipped (with a note) where there is none.
    #[test]
    fn gpu_matches_cpu_on_a_fixture() {
        let Ok(mut gpu) = GpuRenderer::new_headless() else {
            eprintln!("skipped: no hardware GL context");
            return;
        };
        let snap = Snapshot::parse(include_str!("../../../fixtures/synthetic-v2-A.json")).unwrap();
        let (wave, cpu_wave) = waves(&snap);
        let th = crate::theme::theme("green").unwrap();
        let sel = ModelSelect::Auto;
        for signal in [Signal::Live, Signal::Stale { age_s: 12.0 }] {
            let view = PanelView {
                snapshot: Some(&snap), model: &sel, signal, theme: &th, title: None,
                wave: &wave, wave_max: Some(90.0), cpu_wave: &cpu_wave, t: 1.3,
            };
            let mut a = Pixmap::new(1920, 480).unwrap();
            Renderer::new().render(&mut a, &view);
            gpu.render(&mut Renderer::new(), 1920, 480, &view);
            let b = gpu.read_pixmap().unwrap();
            let d = diff(a.data(), b.data());
            assert!(d.psnr >= 35.0 && d.pct_gt16 <= 1.0, "{d:?}");
        }
    }

    /// The incremental GPU render (persistent band image, scissored replay) must
    /// equal a full GPU redraw of the same frame, and stay close to the CPU
    /// reference, over a sequence of changing data, times, sizes, themes and
    /// signals. Skipped where there is no hardware GL.
    #[test]
    fn gpu_incremental_equals_gpu_full_over_a_sequence() {
        let Ok(mut gpu) = GpuRenderer::new_headless() else {
            eprintln!("skipped: no hardware GL context");
            return;
        };
        let names = ["synthetic-v2-A", "synthetic-v2-C", "live-maya-decode", "synthetic-v2-B", "live-maya-idle", "synthetic-v2-D"];
        let snaps: Vec<Snapshot> =
            names.iter().map(|n| Snapshot::parse(&std::fs::read_to_string(format!("fixtures/{n}.json")).unwrap()).unwrap()).collect();
        let themes = [crate::theme::theme("green").unwrap(), crate::theme::theme("blue").unwrap()];
        let sel = ModelSelect::Auto;
        let (mut inc, mut full, mut cpu_r) = (Renderer::new(), Renderer::new(), Renderer::new());
        let (mut partial, mut live, mut worst_psnr, mut worst_vs_full) = (0, 0, f64::INFINITY, f64::INFINITY);
        let mut t = 0.0f32;
        for step in 0..120usize {
            t += 1.0 / 30.0;
            let (w, h) = if step < 80 { (1920, 348) } else { (1920, 480) };
            let snap = &snaps[(step / 9) % snaps.len()];
            let th = &themes[usize::from((60..75).contains(&step))];
            let n = 120 + (step % 5);
            let wave: Vec<(f32, Option<f32>)> =
                (0..n).map(|i| (1.0 - i as f32 / (n - 1) as f32, if i % 37 == 5 { None } else { Some(40.0 + ((i + step) * 7 % 23) as f32) })).collect();
            let cpu: Vec<(f32, Option<f32>)> =
                (0..15).map(|i| (1.0 - (i as f32 + t.fract()) / 15.0, Some(20.0 + ((i + step / 2) % 9) as f32 * 5.0))).collect();
            let signal = if (30..34).contains(&step) { Signal::Stale { age_s: 3.0 } } else { Signal::Live };
            let view = PanelView {
                snapshot: Some(snap), model: &sel, signal: signal.clone(), theme: th, title: None,
                wave: &wave, wave_max: Some(90.0), cpu_wave: &cpu, t,
            };
            let d = gpu.render_band_incremental(0, &mut inc, w, h, &view);
            gpu.render_band(1, &mut full, w, h, &view);
            gpu.submit();
            let a = gpu.read_band_rgba(0).unwrap().2;
            let b = gpu.read_band_rgba(1).unwrap().2;
            let mut c = Pixmap::new(w, h).unwrap();
            cpu_r.render(&mut c, &view);
            let vs_full = diff(&b, &a);
            let vs_cpu = diff(c.data(), &a);
            worst_vs_full = worst_vs_full.min(vs_full.psnr);
            worst_psnr = worst_psnr.min(vs_cpu.psnr);
            assert!(vs_full.psnr >= 45.0, "step {step}: incremental vs full GPU {vs_full:?}");
            assert!(vs_cpu.psnr >= 35.0 && vs_cpu.pct_gt16 <= 1.0, "step {step}: incremental vs CPU {vs_cpu:?}");
            if signal == Signal::Live {
                live += 1;
                partial += usize::from(!d.full);
            }
        }
        eprintln!("worst PSNR: incremental vs full GPU {worst_vs_full:.1} dB, vs CPU {worst_psnr:.1} dB; {partial}/{live} live frames partial");
        assert!(partial * 10 >= live * 7, "only {partial} of {live} live frames were partial");
    }
}

//! Smoke test of the GPU window path: opens a window, renders a fixture with
//! the GL backend for N frames (default 300), presents each frame with swap
//! interval 0 and prints the timings.
//!
//!   cargo run --release --features gpu --example gl_window -- [FRAMES] [WxH]

use std::time::Instant;

use llm_wayvisuals::render::gl::window::GlWindow;
use llm_wayvisuals::render::gl::GpuRenderer;
use llm_wayvisuals::model::{ModelSelect, Snapshot};
use llm_wayvisuals::render::{PanelView, Renderer, Signal};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

struct App {
    frames: u32,
    size: (u32, u32),
    state: Option<(Window, GpuRenderer, Renderer)>,
    snap: Snapshot,
    n: u32,
    t0: Instant,
    ms: Vec<f64>,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.state.is_some() {
            return;
        }
        let attrs = Window::default_attributes().with_title("gl_window").with_inner_size(PhysicalSize::new(self.size.0, self.size.1));
        match GlWindow::create(el, attrs) {
            Ok((win, glw)) => {
                let g = GpuRenderer::from_context(Box::new(glw)).unwrap_or_else(|e| panic!("renderer: {e}"));
                eprintln!("renderer: gpu ({})", g.info().describe());
                win.request_redraw();
                self.state = Some((win, g, Renderer::new()));
                self.t0 = Instant::now();
            }
            Err(e) => {
                eprintln!("gpu window unavailable: {e}");
                el.exit();
            }
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, ev: WindowEvent) {
        let Some((win, g, r)) = self.state.as_mut() else { return };
        match ev {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::Resized(s) => g.resize_surface(s.width, s.height),
            WindowEvent::RedrawRequested => {
                let s = win.inner_size();
                if s.width == 0 || s.height == 0 {
                    return;
                }
                let th = llm_wayvisuals::theme::theme("green").unwrap();
                let wave: Vec<(f32, Option<f32>)> = (0..120).map(|i| (1.0 - i as f32 / 119.0, Some(50.0 + (i % 11) as f32))).collect();
                let sel = ModelSelect::Auto;
                let view = PanelView {
                    snapshot: Some(&self.snap), model: &sel, signal: Signal::Live, theme: &th, title: None,
                    wave: &wave, wave_max: Some(80.0), cpu_wave: &[], t: self.t0.elapsed().as_secs_f32(),
                };
                let t = Instant::now();
                g.render(r, s.width, s.height, &view);
                if let Err(e) = g.present(s.width, s.height) {
                    eprintln!("present: {e}");
                    el.exit();
                    return;
                }
                self.ms.push(t.elapsed().as_secs_f64() * 1e3);
                self.n += 1;
                if self.n >= self.frames {
                    let mean = self.ms.iter().sum::<f64>() / self.ms.len() as f64;
                    let max = self.ms.iter().cloned().fold(0.0, f64::max);
                    let fps = self.n as f64 / self.t0.elapsed().as_secs_f64();
                    println!("{} frames, render+present mean {mean:.2} ms, max {max:.2} ms, {fps:.0} fps unpaced", self.n);
                    el.exit();
                } else {
                    win.request_redraw();
                }
            }
            _ => {}
        }
    }
}

fn main() {
    let mut a = std::env::args().skip(1);
    let frames = a.next().and_then(|v| v.parse().ok()).unwrap_or(300);
    let size = a
        .next()
        .and_then(|v| v.split_once('x').and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?))))
        .unwrap_or((1920, 480));
    let snap = Snapshot::parse(include_str!("../fixtures/synthetic-v2-A.json")).unwrap();
    let el = EventLoop::new().unwrap();
    el.set_control_flow(ControlFlow::Wait);
    let mut app = App { frames, size, state: None, snap, n: 0, t0: Instant::now(), ms: Vec::new() };
    el.run_app(&mut app).unwrap();
}

//! winit + softbuffer backend (Wayland and X11, CPU only).

use super::{
    assign_outputs, bands, blit_xrgb, default_window_size, group_panels, log, Backend, Pacer, PanelCore,
    RunOptions, Target,
};
use llm_wayvisuals::config::MAX_PANELS;
use llm_wayvisuals::render;
use signal_hook::consts::{SIGINT, SIGTERM};
use softbuffer::{Context, Surface};
use std::collections::HashMap;
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tiny_skia::Pixmap;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, OwnedDisplayHandle};
use winit::keyboard::{Key, NamedKey};
use winit::monitor::MonitorHandle;
use winit::platform::wayland::{ActiveEventLoopExtWayland, WindowAttributesExtWayland};
use winit::platform::x11::{ActiveEventLoopExtX11, WindowAttributesExtX11};
use winit::window::{CursorIcon, Fullscreen, Window, WindowAttributes, WindowId};

const APP_ID: &str = "llm-wayvisuals";

pub struct WinitBackend;

fn backend_name(el: &ActiveEventLoop) -> &'static str {
    if el.is_wayland() {
        "Wayland"
    } else if el.is_x11() {
        "X11"
    } else {
        "unknown"
    }
}

fn monitor_names(monitors: &[MonitorHandle]) -> Vec<(String, usize)> {
    monitors
        .iter()
        .enumerate()
        .map(|(i, m)| (m.name().unwrap_or_else(|| format!("output-{i}")), i))
        .collect()
}

fn describe_monitor(i: usize, m: &MonitorHandle) -> String {
    let s = m.size();
    let p = m.position();
    let hz = m
        .refresh_rate_millihertz()
        .map_or("?".to_string(), |mhz| format!("{:.2} Hz", mhz as f64 / 1000.0));
    format!(
        "{i}  {}  {}x{}  pos {},{}  {hz}  scale {}",
        m.name().unwrap_or_else(|| "?".into()),
        s.width,
        s.height,
        p.x,
        p.y,
        m.scale_factor()
    )
}

impl Backend for WinitBackend {
    fn list_outputs(&self) -> Result<(), String> {
        let el = EventLoop::new().map_err(|e| format!("cannot connect to a display: {e}"))?;
        let mut app = ListApp { error: None };
        el.run_app(&mut app).map_err(|e| e.to_string())?;
        app.error.map_or(Ok(()), Err)
    }

    fn run(&self, opts: RunOptions) -> Result<(), String> {
        let quit = Arc::new(AtomicBool::new(false));
        for sig in [SIGINT, SIGTERM] {
            signal_hook::flag::register(sig, quit.clone()).map_err(|e| format!("signal: {e}"))?;
        }
        let mut cores = Vec::new();
        for p in &opts.panels {
            cores.push(PanelCore::new(p.clone())?);
        }
        let el = EventLoop::new().map_err(|e| format!("cannot connect to a display: {e}"))?;
        let (opts_fps, opts_idle_fps, opts_idle_after) = (opts.fps, opts.idle_fps, opts.idle_after);
        let mut app = App {
            opts,
            cores,
            surfaces: Vec::new(),
            by_id: HashMap::new(),
            ctx: None,
            started: false,
            quit,
            start: Instant::now(),
            pacer: Pacer::new(opts_fps, opts_idle_fps, opts_idle_after, Instant::now()),
            next_frame: Instant::now(),
            error: None,
        };
        el.run_app(&mut app).map_err(|e| e.to_string())?;
        app.error.map_or(Ok(()), Err)
    }
}

struct ListApp {
    error: Option<String>,
}

impl ApplicationHandler for ListApp {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        let monitors: Vec<MonitorHandle> = el.available_monitors().collect();
        println!("# backend: {}", backend_name(el));
        if monitors.is_empty() {
            self.error = Some("no monitor reported by the display server".into());
        }
        for (i, m) in monitors.iter().enumerate() {
            println!("{}", describe_monitor(i, m));
        }
        el.exit();
    }
    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
}

struct Slot {
    core: PanelCore,
    pm: Option<Pixmap>,
}

/// One window / fullscreen monitor showing 1..=3 stacked panels.
struct SurfaceWin {
    slots: Vec<Slot>,
    window: Arc<Window>,
    surface: Surface<OwnedDisplayHandle, Arc<Window>>,
    /// Monitor used when toggling to fullscreen.
    monitor: Option<MonitorHandle>,
    /// Last logged inner size (to log geometry once per change).
    last_size: Option<(u32, u32)>,
    index: usize,
}

struct App {
    opts: RunOptions,
    /// Cores waiting for their surface (moved into `surfaces` in `resumed`).
    cores: Vec<PanelCore>,
    surfaces: Vec<SurfaceWin>,
    by_id: HashMap<WindowId, usize>,
    ctx: Option<Context<OwnedDisplayHandle>>,
    started: bool,
    quit: Arc<AtomicBool>,
    start: Instant,
    pacer: Pacer,
    next_frame: Instant,
    error: Option<String>,
}

fn attrs(title: &str) -> WindowAttributes {
    let a = Window::default_attributes().with_title(title);
    let a = WindowAttributesExtWayland::with_name(a, APP_ID, APP_ID);
    WindowAttributesExtX11::with_name(a, APP_ID, APP_ID)
}

impl App {
    fn create_surfaces(&mut self, el: &ActiveEventLoop) -> Result<(), String> {
        let monitors: Vec<MonitorHandle> = el.available_monitors().collect();
        let names = monitor_names(&monitors);
        let avail = if names.is_empty() {
            "none".to_string()
        } else {
            names.iter().map(|(n, i)| format!("{i}={n}")).collect::<Vec<_>>().join(", ")
        };
        log(format!("backend: {}, {} monitor(s): {avail}", backend_name(el), monitors.len()));

        let outs: Vec<Option<&str>> = self.cores.iter().map(|c| c.cfg.output.as_deref()).collect();
        let groups = group_panels(&outs, MAX_PANELS);
        let requests: Vec<Option<&str>> = groups.iter().map(|g| g.output.as_deref()).collect();
        let targets = if self.opts.windowed {
            vec![Target::Windowed { reason: None }; groups.len()]
        } else {
            assign_outputs(&requests, &names)
        };
        let ctx = Context::new(el.owned_display_handle()).map_err(|e| format!("softbuffer: {e}"))?;
        let screen = el
            .primary_monitor()
            .or_else(|| monitors.first().cloned())
            .map(|m| (m.size().width, m.size().height));

        let mut cores: Vec<Option<PanelCore>> = std::mem::take(&mut self.cores).into_iter().map(Some).collect();
        for (n, (group, target)) in groups.iter().zip(targets).enumerate() {
            let slots: Vec<Slot> = group
                .panels
                .iter()
                .filter_map(|&i| cores[i].take())
                .map(|core| Slot { core, pm: None })
                .collect();
            let list = slots.iter().map(|s| s.core.cfg.name.clone()).collect::<Vec<_>>();
            let (window, monitor, what) = match &target {
                Target::Monitor(i) => {
                    let m = monitors[*i].clone();
                    let a = attrs(APP_ID).with_fullscreen(Some(Fullscreen::Borderless(Some(m.clone()))));
                    let w = el.create_window(a).map_err(|e| format!("create window: {e}"))?;
                    w.set_cursor_visible(false);
                    (w, Some(m), format!("{}, fullscreen", names[*i].0))
                }
                Target::Windowed { reason } => {
                    if let Some(r) = reason {
                        log(format!(
                            "surface {}: {r}; available outputs: {avail}; falling back to a window",
                            n + 1
                        ));
                    }
                    let (w, h) = default_window_size(screen);
                    let a = attrs(&format!("llm-wayvisuals \u{2014} {}", list.join(", ")))
                        .with_inner_size(winit::dpi::PhysicalSize::new(w, h));
                    let w = el.create_window(a).map_err(|e| format!("create window: {e}"))?;
                    let m = w.current_monitor();
                    (w, m, "window".to_string())
                }
            };
            log(format!("surface {} ({what}): {}", n + 1, list.join(", ")));
            let window = Arc::new(window);
            let surface = Surface::new(&ctx, window.clone()).map_err(|e| format!("softbuffer: {e}"))?;
            self.by_id.insert(window.id(), self.surfaces.len());
            window.request_redraw();
            self.surfaces.push(SurfaceWin { slots, window, surface, monitor, last_size: None, index: n + 1 });
        }
        self.ctx = Some(ctx);
        Ok(())
    }

    fn redraw(&mut self, idx: usize) {
        let t = self.start.elapsed().as_secs_f32();
        let sw = &mut self.surfaces[idx];
        let size = sw.window.inner_size();
        let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else {
            return;
        };
        if sw.surface.resize(w, h).is_err() {
            return;
        }
        let geo = bands(size.width, size.height, sw.slots.len());
        if sw.last_size != Some((size.width, size.height)) {
            sw.last_size = Some((size.width, size.height));
            let desc: Vec<String> = geo
                .iter()
                .map(|b| {
                    let hd = b.h as f32 / render::scale_for(b.w);
                    let centred = !(render::MIN_HD..=render::MAX_HD).contains(&hd);
                    format!(
                        "{}x{} (design h {:.0}, scale {:.2}{})",
                        b.w,
                        b.h,
                        render::design_height_for(b.w, b.h),
                        render::scale_for(b.w),
                        if centred { ", centred" } else { "" }
                    )
                })
                .collect();
            log(format!(
                "surface {}: {}x{} px, bands: {}",
                sw.index,
                size.width,
                size.height,
                desc.join(", ")
            ));
        }
        let mut buf = match sw.surface.buffer_mut() {
            Ok(b) => b,
            Err(e) => {
                log(format!("buffer failed: {e}"));
                return;
            }
        };
        let stride = size.width as usize;
        if geo.len() == 1 {
            // Letterbox rows outside the single band.
            let b = geo[0];
            let (top, bottom) = ((b.y as usize) * stride, ((b.y + b.h) as usize) * stride);
            buf[..top].fill(0);
            buf[bottom..].fill(0);
        }
        for (slot, b) in sw.slots.iter_mut().zip(&geo) {
            if b.w == 0 || b.h == 0 {
                continue;
            }
            if slot.pm.as_ref().is_none_or(|pm| pm.width() != b.w || pm.height() != b.h) {
                slot.pm = Pixmap::new(b.w, b.h);
            }
            let Some(pm) = slot.pm.as_mut() else { continue };
            slot.core.render(pm, t);
            blit_xrgb(pm.data(), b.w as usize, &mut buf, stride, b.x as usize, b.y as usize);
        }
        if let Err(e) = buf.present() {
            log(format!("present failed: {e}"));
        }
    }

    fn toggle_fullscreen(&mut self, idx: usize) {
        let p = &self.surfaces[idx];
        if p.window.fullscreen().is_some() {
            p.window.set_fullscreen(None);
            p.window.set_cursor_visible(true);
            p.window.set_cursor(CursorIcon::Default);
        } else {
            let m = p.monitor.clone().or_else(|| p.window.current_monitor());
            p.window.set_fullscreen(Some(Fullscreen::Borderless(m)));
            p.window.set_cursor_visible(false);
        }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.started {
            return;
        }
        self.started = true;
        if let Err(e) = self.create_surfaces(el) {
            self.error = Some(e);
            el.exit();
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        let Some(&idx) = self.by_id.get(&id) else { return };
        match event {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::RedrawRequested => self.redraw(idx),
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => {
                self.surfaces[idx].window.request_redraw()
            }
            WindowEvent::KeyboardInput {
                event: KeyEvent { logical_key, state: ElementState::Pressed, repeat: false, .. },
                ..
            } => match logical_key.as_ref() {
                Key::Named(NamedKey::Escape) => el.exit(),
                Key::Character(c) if c.eq_ignore_ascii_case("q") => el.exit(),
                Key::Character(c) if c.eq_ignore_ascii_case("f") => self.toggle_fullscreen(idx),
                _ => {}
            },
            _ => {}
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        if self.quit.load(Ordering::Relaxed) {
            el.exit();
            return;
        }
        let now = Instant::now();
        if now >= self.next_frame {
            let mut acts = Vec::new();
            for s in &mut self.surfaces {
                for slot in &mut s.slots {
                    acts.push(slot.core.tick(now));
                }
                s.window.request_redraw();
            }
            let fps = self.pacer.update(now, acts);
            self.next_frame = now + Duration::from_secs_f64(1.0 / fps as f64);
        }
        el.set_control_flow(ControlFlow::WaitUntil(self.next_frame));
    }
}

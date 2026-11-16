//! winit + softbuffer backend (Wayland and X11, CPU only).

use super::{
    assign_outputs, bands, blit_xrgb, blit_xrgb_rect, coalesce, default_window_size, group_panels, log, Backend, Pacer, PanelCore,
    RunOptions, Target,
};
use llm_wayvisuals::config::MAX_PANELS;
use llm_wayvisuals::profile::{self, Cat};
use llm_wayvisuals::render;
use signal_hook::consts::{SIGINT, SIGTERM};
use softbuffer::{Context, Surface};
use std::collections::{HashMap, VecDeque};
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

/// GPU renderer of a surface (uninhabited when built without the `gpu` feature).
#[cfg(feature = "gpu")]
type Gpu = llm_wayvisuals::render::gl::GpuRenderer;
#[cfg(not(feature = "gpu"))]
type Gpu = std::convert::Infallible;

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
    /// GL renderer (declared first: dropped before the window it draws on).
    #[cfg_attr(not(feature = "gpu"), allow(dead_code))]
    gpu: Option<Gpu>,
    slots: Vec<Slot>,
    window: Arc<Window>,
    /// softbuffer surface of the CPU path (`None` on a GPU surface).
    surface: Option<Surface<OwnedDisplayHandle, Arc<Window>>>,
    /// Monitor used when toggling to fullscreen.
    monitor: Option<MonitorHandle>,
    /// Last logged inner size (to log geometry once per change).
    last_size: Option<(u32, u32)>,
    /// GPU surfaces: time of the last swap.
    #[cfg_attr(not(feature = "gpu"), allow(dead_code))]
    last_present: Option<Instant>,
    index: usize,
    /// Damage (surface pixels) of the last presented frames, oldest first;
    /// `None` = the whole surface. Used to refresh buffers that are older than one frame.
    hist: VecDeque<Option<Vec<render::Rect>>>,
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
    /// Creates the window of surface `n` with the renderer asked for: GPU
    /// (GL context on the window) or CPU (plain window, softbuffer). In `auto`
    /// any GPU failure falls back to the CPU with one line saying why.
    fn make_window(&self, el: &ActiveEventLoop, a: WindowAttributes, n: usize) -> Result<(Arc<Window>, Option<Gpu>), String> {
        use llm_wayvisuals::rendersel::RendererPref;
        let pref = self.opts.renderer;
        #[cfg(feature = "gpu")]
        if pref != RendererPref::Cpu {
            use llm_wayvisuals::render::gl::{window::GlWindow, GpuRenderer};
            let made = GlWindow::create(el, a.clone()).and_then(|(w, glw)| GpuRenderer::from_context(Box::new(glw)).map(|g| (w, g)));
            match made {
                Ok((w, g)) => {
                    log(format!("surface {n}: renderer: gpu ({})", g.info().describe()));
                    return Ok((Arc::new(w), Some(g)));
                }
                Err(e) if pref == RendererPref::Gpu => return Err(format!("surface {n}: --renderer gpu: {e}")),
                Err(e) => log(format!("surface {n}: renderer: cpu ({e})")),
            }
        } else {
            log(format!("surface {n}: renderer: cpu (requested)"));
        }
        #[cfg(not(feature = "gpu"))]
        match pref {
            RendererPref::Gpu => return Err("--renderer gpu: this build has no GPU backend".into()),
            RendererPref::Cpu => log(format!("surface {n}: renderer: cpu (requested)")),
            RendererPref::Auto => log(format!("surface {n}: renderer: cpu (built without the gpu feature)")),
        }
        let w = el.create_window(a).map_err(|e| format!("create window: {e}"))?;
        Ok((Arc::new(w), None))
    }

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
            let gpu: Option<Gpu>;
            let (window, monitor, what) = match &target {
                Target::Monitor(i) => {
                    let m = monitors[*i].clone();
                    let a = attrs(APP_ID).with_fullscreen(Some(Fullscreen::Borderless(Some(m.clone()))));
                    let (w, g) = self.make_window(el, a, n + 1)?;
                    w.set_cursor_visible(false);
                    gpu = g;
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
                    let (w, g) = self.make_window(el, a, n + 1)?;
                    let m = w.current_monitor();
                    gpu = g;
                    (w, m, "window".to_string())
                }
            };
            log(format!("surface {} ({what}): {}", n + 1, list.join(", ")));
            let surface = match gpu {
                Some(_) => None,
                None => Some(Surface::new(&ctx, window.clone()).map_err(|e| format!("softbuffer: {e}"))?),
            };
            self.by_id.insert(window.id(), self.surfaces.len());
            window.request_redraw();
            self.surfaces.push(SurfaceWin { gpu, slots, window, surface, monitor, last_size: None, last_present: None, index: n + 1, hist: VecDeque::new() });
        }
        self.ctx = Some(ctx);
        Ok(())
    }

    fn redraw(&mut self, idx: usize) {
        let _frame = profile::scope(Cat::Frame);
        let t = self.start.elapsed().as_secs_f32();
        let sw = &mut self.surfaces[idx];
        let size = sw.window.inner_size();
        let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else {
            return;
        };
        if let Some(surface) = sw.surface.as_mut() {
            if surface.resize(w, h).is_err() {
                return;
            }
        }
        let geo = bands(size.width, size.height, sw.slots.len());
        let mut resized = false;
        if sw.last_size != Some((size.width, size.height)) {
            resized = true;
            sw.hist.clear();
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
        #[cfg(feature = "gpu")]
        if let Some(g) = sw.gpu.as_mut() {
            // GPU surface: every band is redrawn into the GL framebuffer each frame.
            if resized {
                g.resize_surface(size.width, size.height);
            }
            let mut placed = Vec::with_capacity(geo.len());
            let mut changed = resized;
            for (bi, (slot, b)) in sw.slots.iter_mut().zip(&geo).enumerate() {
                if b.w == 0 || b.h == 0 {
                    continue;
                }
                let d = slot.core.render_gpu(g, bi, b.w, b.h, t);
                changed |= d.full || !d.is_empty();
                placed.push((bi, b.x as i32, b.y as i32));
            }
            // Nothing changed on any band: no commands, no swap (the window keeps
            // its last frame). A refresh at least once per second covers exposure.
            if changed || sw.last_present.is_none_or(|p| p.elapsed() >= Duration::from_secs(1)) {
                if let Err(e) = g.present_bands(&placed, size.width, size.height) {
                    log(format!("present failed: {e}"));
                }
                sw.last_present = Some(Instant::now());
            }
            drop(_frame);
            profile::frame_done();
            return;
        }
        // 1. render every band into its persistent pixmap, collect the damage
        let mut full = resized;
        let mut cur: Vec<render::Rect> = Vec::new();
        for (bi, (slot, b)) in sw.slots.iter_mut().zip(&geo).enumerate() {
            if b.w == 0 || b.h == 0 {
                continue;
            }
            if slot.pm.as_ref().is_none_or(|pm| pm.width() != b.w || pm.height() != b.h) {
                slot.pm = Pixmap::new(b.w, b.h);
            }
            let Some(pm) = slot.pm.as_mut() else { continue };
            let d = slot.core.render(pm, t);
            if d.full {
                full = true;
            }
            cur.extend(d.rects.iter().map(|r| render::Rect { x: r.x + b.x, y: r.y + b.y, ..*r }));
            profile::tiles(idx, bi, pm.data(), b.w as usize, b.h as usize);
        }
        let cur = if full { cur } else { coalesce(cur) };
        if !full && cur.is_empty() {
            // nothing changed: the displayed frame is still right
            drop(_frame);
            profile::frame_done();
            return;
        }
        // 2. get a buffer and bring it up to date
        let acquire = profile::scope(Cat::Acquire);
        let mut buf = match sw.surface.as_mut().expect("cpu surface").buffer_mut() {
            Ok(b) => b,
            Err(e) => {
                log(format!("buffer failed: {e}"));
                return;
            }
        };
        drop(acquire);
        let age = buf.age() as usize;
        // The buffer holds the frame presented `age` frames ago: refresh the
        // damage of the frames since, plus the current one.
        let mut copy: Option<Vec<render::Rect>> = None;
        if !full && age >= 1 && age - 1 <= sw.hist.len() {
            let mut v = cur.clone();
            let mut ok = true;
            for h in sw.hist.iter().rev().take(age - 1) {
                match h {
                    Some(r) => v.extend(r.iter().copied()),
                    None => ok = false,
                }
            }
            if ok {
                copy = Some(v);
            }
        }
        let stride = size.width as usize;
        {
            let _p = profile::scope(Cat::Convert);
            match &copy {
                None => {
                    if geo.len() == 1 {
                        // Letterbox rows outside the single band.
                        let b = geo[0];
                        let (top, bottom) = ((b.y as usize) * stride, ((b.y + b.h) as usize) * stride);
                        buf[..top].fill(0);
                        buf[bottom..].fill(0);
                    }
                    for (slot, b) in sw.slots.iter().zip(&geo) {
                        if let (Some(pm), true) = (slot.pm.as_ref(), b.w > 0 && b.h > 0) {
                            blit_xrgb(pm.data(), b.w as usize, &mut buf, stride, b.x as usize, b.y as usize);
                        }
                    }
                }
                Some(rects) => {
                    for (slot, b) in sw.slots.iter().zip(&geo) {
                        let Some(pm) = slot.pm.as_ref().filter(|_| b.w > 0 && b.h > 0) else { continue };
                        for r in rects {
                            // surface rect -> band-local rect
                            let (x0, y0) = (r.x.max(b.x), r.y.max(b.y));
                            let (x1, y1) = ((r.x + r.w).min(b.x + b.w), (r.y + r.h).min(b.y + b.h));
                            if x1 > x0 && y1 > y0 {
                                let local = render::Rect { x: x0 - b.x, y: y0 - b.y, w: x1 - x0, h: y1 - y0 };
                                blit_xrgb_rect(pm.data(), b.w as usize, &mut buf, stride, b.x as usize, b.y as usize, &local);
                            }
                        }
                    }
                }
            }
        }
        {
            let _p = profile::scope(Cat::Present);
            let res = if full {
                buf.present()
            } else {
                let dmg: Vec<softbuffer::Rect> = cur
                    .iter()
                    .filter_map(|r| {
                        Some(softbuffer::Rect { x: r.x, y: r.y, width: NonZeroU32::new(r.w)?, height: NonZeroU32::new(r.h)? })
                    })
                    .collect();
                buf.present_with_damage(&dmg)
            };
            if let Err(e) = res {
                log(format!("present failed: {e}"));
                sw.hist.clear();
            } else {
                sw.hist.push_back(if full { None } else { Some(cur) });
                while sw.hist.len() > 8 {
                    sw.hist.pop_front();
                }
            }
        }
        drop(_frame);
        profile::frame_done();
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
                let _p = profile::scope(Cat::Tick);
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

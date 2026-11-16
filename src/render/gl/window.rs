//! GL context + window surface for a winit window (Wayland via EGL, X11 via
//! GLX or EGL), created with glutin.
//!
//! Entry point for the frontend: [`GlWindow::create`] replaces
//! `event_loop.create_window(attrs)` and returns the window together with its
//! GL context. On error (no GL, software renderer, ...) nothing is left
//! behind except the error string: the caller then creates a normal window
//! and uses the CPU path.

use std::ffi::{c_void, CString};
use std::num::NonZeroU32;

use glutin::config::{ConfigTemplateBuilder, GlConfig};
use glutin::context::{ContextApi, ContextAttributesBuilder, GlProfile, NotCurrentContext, PossiblyCurrentContext, Version};
use glutin::display::{GetGlDisplay, GlDisplay};
use glutin::prelude::*;
use glutin::surface::{GlSurface, Surface, SwapInterval, WindowSurface};
use glutin_winit::{DisplayBuilder, GlWindow as _};
use raw_window_handle::HasWindowHandle;
use winit::event_loop::ActiveEventLoop;
use winit::window::{Window, WindowAttributes};

use super::{GlContext, GlInfo};

pub struct GlWindow {
    // field order = drop order: surface before context before display
    surface: Surface<WindowSurface>,
    ctx: PossiblyCurrentContext,
    info: GlInfo,
}

impl GlWindow {
    /// Creates the window (with a GL-compatible visual), a GL 3.3 core (else
    /// GLES 3.0) context and the window surface, makes the context current and
    /// sets the swap interval to 0 (the application paces its frames).
    /// Software renderers are rejected.
    pub fn create(event_loop: &ActiveEventLoop, attrs: WindowAttributes) -> Result<(Window, GlWindow), String> {
        let template = ConfigTemplateBuilder::new().prefer_hardware_accelerated(Some(true)).with_alpha_size(0);
        let (window, config) = DisplayBuilder::new()
            .with_window_attributes(Some(attrs))
            .build(event_loop, template, |configs| {
                // fewest samples first: no MSAA needed (femtovg anti-aliases itself)
                configs.reduce(|a, b| if b.num_samples() < a.num_samples() { b } else { a }).expect("at least one config")
            })
            .map_err(|e| format!("GL display/config: {e}"))?;
        let window = window.ok_or("window creation failed")?;
        let raw = window.window_handle().map_err(|e| format!("window handle: {e}"))?.as_raw();
        let display = config.display();

        let desktop = ContextAttributesBuilder::new()
            .with_context_api(ContextApi::OpenGl(Some(Version::new(3, 3))))
            .with_profile(GlProfile::Core)
            .build(Some(raw));
        let gles = ContextAttributesBuilder::new().with_context_api(ContextApi::Gles(Some(Version::new(3, 0)))).build(Some(raw));
        let not_current: NotCurrentContext = unsafe { display.create_context(&config, &desktop) }
            .or_else(|e1| unsafe { display.create_context(&config, &gles) }.map_err(|e2| format!("no GL 3.3 core ({e1}) nor GLES 3.0 ({e2}) context")))?;

        let attrs = window.build_surface_attributes(Default::default()).map_err(|e| format!("surface attributes: {e}"))?;
        let surface = unsafe { display.create_window_surface(&config, &attrs) }.map_err(|e| format!("window surface: {e}"))?;
        let ctx = not_current.make_current(&surface).map_err(|e| format!("make current: {e}"))?;
        // swap interval 0: never block on the compositor / display
        if let Err(e) = surface.set_swap_interval(&ctx, SwapInterval::DontWait) {
            eprintln!("llm-wayvisuals: warning: swap interval 0 refused ({e}), frames may be paced by the display");
        }

        let gl = unsafe { glow::Context::from_loader_function_cstr(|s| display.get_proc_address(s) as *const c_void) };
        let info = GlInfo::query(&gl);
        drop(gl);
        if let Some(why) = info.rejection() {
            return Err(why);
        }
        Ok((window, GlWindow { surface, ctx, info }))
    }

    /// To call on `WindowEvent::Resized` (physical size, ignored if 0).
    pub fn resize(&self, w: u32, h: u32) {
        if let (Some(w), Some(h)) = (NonZeroU32::new(w), NonZeroU32::new(h)) {
            self.surface.resize(&self.ctx, w, h);
        }
    }
}

impl GlContext for GlWindow {
    fn info(&self) -> &GlInfo {
        &self.info
    }
    fn make_current(&self) -> Result<(), String> {
        if self.ctx.is_current() {
            return Ok(());
        }
        self.ctx.make_current(&self.surface).map_err(|e| e.to_string())
    }
    fn get_proc_address(&self, name: &str) -> *const c_void {
        match CString::new(name) {
            Ok(c) => self.ctx.display().get_proc_address(&c) as *const c_void,
            Err(_) => std::ptr::null(),
        }
    }
    fn swap_buffers(&self) -> Result<(), String> {
        self.surface.swap_buffers(&self.ctx).map_err(|e| e.to_string())
    }
    fn resize(&self, w: u32, h: u32) {
        GlWindow::resize(self, w, h)
    }
}

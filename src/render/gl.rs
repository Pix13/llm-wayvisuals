//! Optional GPU backend of the renderer: [`Canvas`](crate::render::canvas::Canvas)
//! on femtovg (OpenGL 3.3 core / GLES 3.0). Feature `gpu`.
//!
//! * `store`    : the `Canvas` implementation (`GlStore`);
//! * `headless` : surfaceless EGL context (offscreen rendering, probing);
//! * `window`   : glutin context + surface for a winit window.
//!
//! Software renderers (llvmpipe, ...) are rejected at context creation: the
//! CPU path is faster than they are.

pub mod compare;
mod headless;
mod store;
pub mod window;

use std::ffi::c_void;
use std::rc::Rc;

use glow::HasContext;

use crate::render::{PanelView, Renderer};
use crate::rendersel::is_software_renderer;

pub use headless::HeadlessContext;
pub use store::GlStore;

/// Identity of the GL device.
#[derive(Debug, Clone, Default)]
pub struct GlInfo {
    pub renderer: String,
    pub vendor: String,
    pub version: String,
}

impl GlInfo {
    pub(crate) fn query(gl: &glow::Context) -> Self {
        unsafe {
            GlInfo {
                renderer: gl.get_parameter_string(glow::RENDERER),
                vendor: gl.get_parameter_string(glow::VENDOR),
                version: gl.get_parameter_string(glow::VERSION),
            }
        }
    }

    /// Why this device must not be used, if so.
    pub fn rejection(&self) -> Option<String> {
        is_software_renderer(&self.renderer, &self.vendor)
            .then(|| format!("software GL renderer rejected: {} ({})", self.renderer, self.vendor))
    }

    /// `AMD Radeon Graphics (radeonsi, ...), GL 4.6`
    pub fn describe(&self) -> String {
        let v = self.version.trim();
        let ver = match v.strip_prefix("OpenGL ES ") {
            Some(es) => format!("GLES {}", es.split_whitespace().next().unwrap_or("")),
            None => format!("GL {}", v.split_whitespace().next().unwrap_or("")),
        };
        format!("{}, {}", self.renderer.trim(), ver)
    }
}

/// A current-able GL context (headless EGL or a window's).
pub trait GlContext {
    fn info(&self) -> &GlInfo;
    fn make_current(&self) -> Result<(), String>;
    fn get_proc_address(&self, name: &str) -> *const c_void;
    /// Presents the back buffer (no-op for a headless context).
    fn swap_buffers(&self) -> Result<(), String>;
    /// Window surface resize (physical pixels); no-op for a headless context.
    fn resize(&self, _w: u32, _h: u32) {}
}

/// GL context + femtovg canvas + the frame image: renders [`Renderer`] frames
/// on the GPU.
pub struct GpuRenderer {
    // the store must drop before the context
    store: GlStore,
    ctx: Box<dyn GlContext>,
    /// CPU time of the last frame: recording the draw calls, then submitting
    /// them to the driver (femtovg tessellation + GL calls), in ms.
    pub last_record_ms: f64,
    pub last_submit_ms: f64,
}

impl GpuRenderer {
    /// Offscreen renderer on a surfaceless EGL context.
    pub fn new_headless() -> Result<Self, String> {
        Self::from_context(Box::new(HeadlessContext::new()?))
    }

    /// Builds the canvas on a context that is current on this thread.
    pub fn from_context(ctx: Box<dyn GlContext>) -> Result<Self, String> {
        if let Some(why) = ctx.info().rejection() {
            return Err(why);
        }
        ctx.make_current()?;
        let load = |s: &str| ctx.get_proc_address(s);
        let gl = Rc::new(unsafe { glow::Context::from_loader_function(|s| load(s)) });
        let renderer = unsafe { femtovg::renderer::OpenGl::new_from_function(|s| load(s)) }.map_err(|e| format!("femtovg OpenGL: {e}"))?;
        let store = GlStore::new(gl, renderer)?;
        Ok(GpuRenderer { store, ctx, last_record_ms: 0.0, last_submit_ms: 0.0 })
    }

    pub fn info(&self) -> &GlInfo {
        self.ctx.info()
    }

    pub fn context(&self) -> &dyn GlContext {
        self.ctx.as_ref()
    }

    /// Renders one `w` x `h` frame of `view` into the internal frame image and
    /// submits it to the GPU (does not wait for completion).
    pub fn render(&mut self, r: &mut Renderer, w: u32, h: u32, view: &PanelView) {
        let t0 = std::time::Instant::now();
        self.render_band(0, r, w, h, view);
        let t1 = std::time::Instant::now();
        self.submit();
        self.last_record_ms = (t1 - t0).as_secs_f64() * 1e3;
        self.last_submit_ms = t1.elapsed().as_secs_f64() * 1e3;
    }

    /// Records one `w` x `h` band of a surface frame into its own image
    /// (index `band`, 0 for the first). Nothing reaches the GPU before
    /// [`GpuRenderer::submit`] or [`GpuRenderer::present_bands`]. Each band
    /// needs its own `Renderer` (its cached layers live in this renderer).
    pub fn render_band(&mut self, band: usize, r: &mut Renderer, w: u32, h: u32, view: &PanelView) {
        let _ = self.ctx.make_current();
        self.store.begin_frame(band, w, h);
        r.render_canvas(&mut self.store, w, h, view);
    }

    /// Like [`GpuRenderer::render_band`], but repaints only what changed since
    /// the previous call for this band (its image persists). The returned
    /// [`Damage`](crate::render::Damage) is empty (and not full) when nothing
    /// changed: then there is nothing to present for this band.
    pub fn render_band_incremental(&mut self, band: usize, r: &mut Renderer, w: u32, h: u32, view: &PanelView) -> crate::render::Damage {
        let _ = self.ctx.make_current();
        self.store.begin_frame(band, w, h);
        r.render_incremental_on(&mut self.store, w, h, view)
    }

    /// Sends the recorded commands to the driver.
    pub fn submit(&mut self) {
        let _p = crate::profile::scope(crate::profile::Cat::GlSubmit);
        self.store.flush();
    }

    /// Blocks until the GPU has finished (benchmarks).
    pub fn finish(&self) {
        unsafe { self.store.gl.finish() }
    }

    /// Top-down premultiplied RGBA of the last frame: `(w, h, bytes)`.
    pub fn read_rgba(&mut self) -> Option<(u32, u32, Vec<u8>)> {
        self.store.read_frame(0)
    }

    /// Top-down premultiplied RGBA of band `band`'s image.
    pub fn read_band_rgba(&mut self, band: usize) -> Option<(u32, u32, Vec<u8>)> {
        self.store.read_frame(band)
    }

    /// Last frame as a tiny-skia pixmap.
    pub fn read_pixmap(&mut self) -> Option<tiny_skia::Pixmap> {
        let (w, h, px) = self.read_rgba()?;
        let mut pm = tiny_skia::Pixmap::new(w, h)?;
        pm.data_mut().copy_from_slice(&px);
        Some(pm)
    }

    /// Tells the window surface its new size (call on `WindowEvent::Resized`).
    pub fn resize_surface(&self, w: u32, h: u32) {
        self.ctx.resize(w, h);
    }

    /// Composes the band images over the window surface of `sw` x `sh`
    /// pixels (black background; `(band, x, y)` = position of each band),
    /// submits everything and swaps buffers.
    pub fn present_bands(&mut self, bands: &[(usize, i32, i32)], sw: u32, sh: u32) -> Result<(), String> {
        self.ctx.make_current()?;
        {
            let _p = crate::profile::scope(crate::profile::Cat::GlSubmit);
            self.store.present_to_screen(bands, sw, sh);
        }
        let _p = crate::profile::scope(crate::profile::Cat::GlSwap);
        self.ctx.swap_buffers()
    }

    /// Single band, at the origin.
    pub fn present(&mut self, sw: u32, sh: u32) -> Result<(), String> {
        self.present_bands(&[(0, 0, 0)], sw, sh)
    }
}

/// Availability probe: creates a headless context and returns its description,
/// or why the GPU path is unusable.
pub fn probe() -> Result<String, String> {
    GpuRenderer::new_headless().map(|g| g.info().describe())
}

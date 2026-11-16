//! Surfaceless EGL context (no window, no display server): used by
//! `render-png --renderer gpu`, `bench-render --renderer gpu`, tests and the
//! availability probe.

use std::ffi::c_void;
use std::rc::Rc;

use khronos_egl as egl;

use super::{GlContext, GlInfo};

/// `EGL_PLATFORM_SURFACELESS_MESA`
const PLATFORM_SURFACELESS_MESA: egl::Enum = 0x31DD;

type Egl = egl::DynamicInstance<egl::EGL1_5>;

pub struct HeadlessContext {
    egl: Rc<Egl>,
    display: egl::Display,
    ctx: egl::Context,
    info: GlInfo,
}

fn create(egl: &Egl, display: egl::Display, api: egl::Enum, renderable: egl::Int, ctx_attribs: &[egl::Int]) -> Result<egl::Context, String> {
    egl.bind_api(api).map_err(|e| format!("eglBindAPI: {e}"))?;
    let cfg_attribs = [
        egl::SURFACE_TYPE, egl::PBUFFER_BIT, egl::RENDERABLE_TYPE, renderable,
        egl::RED_SIZE, 8, egl::GREEN_SIZE, 8, egl::BLUE_SIZE, 8, egl::ALPHA_SIZE, 8, egl::NONE,
    ];
    let cfg = egl
        .choose_first_config(display, &cfg_attribs)
        .map_err(|e| format!("eglChooseConfig: {e}"))?
        .ok_or("no matching EGL config")?;
    let ctx = egl.create_context(display, cfg, None, ctx_attribs).map_err(|e| format!("eglCreateContext: {e}"))?;
    if let Err(e) = egl.make_current(display, None, None, Some(ctx)) {
        let _ = egl.destroy_context(display, ctx);
        return Err(format!("eglMakeCurrent (surfaceless): {e}"));
    }
    Ok(ctx)
}

impl HeadlessContext {
    /// Creates a GL 3.3 core (else GLES 3.0) surfaceless context and makes it
    /// current on this thread. Software renderers are rejected.
    pub fn new() -> Result<Self, String> {
        let egl = unsafe { Egl::load_required() }.map_err(|e| format!("libEGL not available: {e}"))?;
        let egl = Rc::new(egl);
        let display = unsafe { egl.get_platform_display(PLATFORM_SURFACELESS_MESA, egl::DEFAULT_DISPLAY, &[egl::ATTRIB_NONE]) }
            .map_err(|e| format!("EGL surfaceless display: {e}"))?;
        egl.initialize(display).map_err(|e| format!("eglInitialize: {e}"))?;

        let desktop = [
            egl::CONTEXT_MAJOR_VERSION, 3, egl::CONTEXT_MINOR_VERSION, 3,
            egl::CONTEXT_OPENGL_PROFILE_MASK, egl::CONTEXT_OPENGL_CORE_PROFILE_BIT, egl::NONE,
        ];
        let gles = [egl::CONTEXT_MAJOR_VERSION, 3, egl::CONTEXT_MINOR_VERSION, 0, egl::NONE];
        let ctx = match create(&egl, display, egl::OPENGL_API, egl::OPENGL_BIT, &desktop) {
            Ok(c) => c,
            Err(e1) => create(&egl, display, egl::OPENGL_ES_API, egl::OPENGL_ES3_BIT, &gles)
                .map_err(|e2| format!("no GL 3.3 core ({e1}) nor GLES 3.0 ({e2}) context"))?,
        };
        let mut me = HeadlessContext { egl, display, ctx, info: GlInfo::default() };
        let gl = me.load_glow();
        me.info = GlInfo::query(&gl);
        if let Some(why) = me.info.rejection() {
            return Err(why);
        }
        Ok(me)
    }

    fn load_glow(&self) -> glow::Context {
        let egl = self.egl.clone();
        unsafe { glow::Context::from_loader_function(move |s| egl.get_proc_address(s).map_or(std::ptr::null(), |p| p as *const c_void)) }
    }
}

impl GlContext for HeadlessContext {
    fn info(&self) -> &GlInfo {
        &self.info
    }
    fn make_current(&self) -> Result<(), String> {
        self.egl.make_current(self.display, None, None, Some(self.ctx)).map_err(|e| e.to_string())
    }
    fn get_proc_address(&self, name: &str) -> *const c_void {
        self.egl.get_proc_address(name).map_or(std::ptr::null(), |p| p as *const c_void)
    }
    fn swap_buffers(&self) -> Result<(), String> {
        Ok(())
    }
}

impl Drop for HeadlessContext {
    fn drop(&mut self) {
        let _ = self.egl.make_current(self.display, None, None, None);
        let _ = self.egl.destroy_context(self.display, self.ctx);
        let _ = self.egl.terminate(self.display);
    }
}

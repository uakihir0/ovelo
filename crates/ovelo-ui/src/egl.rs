//! `EGL`/`GLES` shim (design doc §07): the object model the guest's
//! `libEGL`/`libGLESv2` exports resolve to. Contexts and surfaces are
//! process-local handles; `eglSwapBuffers` posts through
//! [`ANativeWindow`] — the real ANGLE-Metal backend replaces the buffer
//! transport later without changing this contract.
//!
//! `Gles` implements the clear-only subset today (`glClearColor`/
//! `glClear` rasterize through the CPU [`Surface`]); draw calls are
//! tracked so missing features surface as `GPU_FEATURE` errors, not
//! silent no-ops.

use std::collections::HashMap;

use ovelo_error::OveloError;

use crate::bufq::{ANativeWindow, FrameId};
use crate::color::Color;

fn err(cause: &str) -> OveloError {
    OveloError::new(
        ovelo_error::ErrorCategory::GpuFeature,
        ovelo_error::Phase::Gpu,
        cause,
    )
}

/// `EGLDisplay` — one per process (`eglGetDisplay`/`eglInitialize`).
#[derive(Debug, Default)]
pub struct EglDisplay {
    next_ctx: u64,
    next_surf: u64,
    /// Bound context per thread slot (single-slot model: the UI thread).
    current: Option<u64>,
    contexts: HashMap<u64, EglContext>,
    surfaces: HashMap<u64, EglSurface>,
}

/// `EGLConfig` attributes we actually honour (`eglChooseConfig`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EglConfig {
    /// `EGL_RED_SIZE`.
    pub red: u32,
    /// `EGL_GREEN_SIZE`.
    pub green: u32,
    /// `EGL_BLUE_SIZE`.
    pub blue: u32,
    /// `EGL_ALPHA_SIZE`.
    pub alpha: u32,
    /// `EGL_DEPTH_SIZE`.
    pub depth: u32,
    /// `EGL_RENDERABLE_TYPE` bit (`EGL_OPENGL_ES2_BIT` = 4).
    pub renderable: u32,
}

/// Our single supported config: RGBA8, ES2.
pub const CONFIG_RGBA8_ES2: EglConfig = EglConfig {
    red: 8,
    green: 8,
    blue: 8,
    alpha: 8,
    depth: 0,
    renderable: 4,
};

/// `EGLContext` — carries the GLES state (clear color today).
#[derive(Debug)]
pub struct EglContext {
    /// Shared-state group (`EGL_NO_CONTEXT` = none).
    pub share_group: Option<u64>,
    /// The GLES command surface.
    pub gl: Gles,
}

/// `EGLSurface` — a window surface over an `ANativeWindow`.
pub struct EglSurface {
    /// Producer side of the queue.
    pub window: ANativeWindow,
    /// The config it was created with.
    pub config: EglConfig,
    /// Set when `eglDestroySurface` ran.
    pub destroyed: bool,
}

impl std::fmt::Debug for EglSurface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EglSurface")
            .field("config", &self.config)
            .field("destroyed", &self.destroyed)
            .finish_non_exhaustive()
    }
}

impl EglDisplay {
    /// `eglInitialize`.
    pub fn initialize() -> Self {
        Self::default()
    }

    /// `eglChooseConfig` — match requested attributes against supported
    /// configs; `None` when nothing satisfies them (EGL_BAD_MATCH style).
    pub fn choose_config(&self, want: &EglConfig) -> Option<EglConfig> {
        let ok = CONFIG_RGBA8_ES2.red >= want.red
            && CONFIG_RGBA8_ES2.green >= want.green
            && CONFIG_RGBA8_ES2.blue >= want.blue
            && CONFIG_RGBA8_ES2.alpha >= want.alpha
            && want.depth == 0
            && CONFIG_RGBA8_ES2.renderable & want.renderable == want.renderable;
        ok.then_some(CONFIG_RGBA8_ES2)
    }

    /// `eglCreateContext` — returns the context handle.
    pub fn create_context(&mut self, share: Option<u64>) -> u64 {
        self.next_ctx += 1;
        self.contexts.insert(
            self.next_ctx,
            EglContext {
                share_group: share,
                gl: Gles::new(),
            },
        );
        self.next_ctx
    }

    /// `eglCreateWindowSurface` — wraps the guest's `ANativeWindow`.
    pub fn create_window_surface(&mut self, window: ANativeWindow) -> u64 {
        self.next_surf += 1;
        self.surfaces.insert(
            self.next_surf,
            EglSurface {
                window,
                config: CONFIG_RGBA8_ES2,
                destroyed: false,
            },
        );
        self.next_surf
    }

    /// `eglMakeCurrent`.
    pub fn make_current(&mut self, ctx: u64) -> Result<(), OveloError> {
        if !self.contexts.contains_key(&ctx) {
            return Err(err("ui.egl.bad_context"));
        }
        self.current = Some(ctx);
        Ok(())
    }

    /// The current context's GLES state.
    pub fn gl(&mut self) -> Result<&mut Gles, OveloError> {
        let c = self.current.ok_or_else(|| err("ui.egl.no_current"))?;
        Ok(&mut self.contexts.get_mut(&c).unwrap().gl)
    }

    /// `eglSwapBuffers(surface)` — `unlockAndPost` the producer frame.
    /// Returns the posted frame id (the completion fence for the host).
    pub fn swap_buffers(&mut self, surface: u64) -> Result<FrameId, OveloError> {
        let s = self
            .surfaces
            .get_mut(&surface)
            .ok_or_else(|| err("ui.egl.bad_surface"))?;
        if s.destroyed {
            return Err(err("ui.egl.surface_destroyed"));
        }
        s.window.unlock_and_post()
    }

    /// The buffer a surface renders into this frame (`ANativeWindow_lock`).
    pub fn surface_buffer(
        &mut self,
        surface: u64,
    ) -> Result<&mut crate::surface::Surface, OveloError> {
        let s = self
            .surfaces
            .get_mut(&surface)
            .ok_or_else(|| err("ui.egl.bad_surface"))?;
        if s.destroyed {
            return Err(err("ui.egl.surface_destroyed"));
        }
        s.window.lock()
    }

    /// Host side: latch the newest posted frame for display.
    /// Without this the producer's queue is unreachable and fills up.
    pub fn acquire(&mut self, surface: u64) -> Option<FrameId> {
        self.surfaces
            .get_mut(&surface)?
            .window
            .queue_mut()
            .acquire()
    }

    /// Host side: the latched frame's pixels.
    pub fn frame_pixels(&self, surface: u64, frame: FrameId) -> Option<&crate::surface::Surface> {
        self.surfaces.get(&surface)?.window.queue().read(frame)
    }

    /// Host side: display done — the buffer recycles to the producer.
    pub fn release(&mut self, surface: u64, frame: FrameId) -> Result<(), OveloError> {
        let s = self
            .surfaces
            .get_mut(&surface)
            .ok_or_else(|| err("ui.egl.bad_surface"))?;
        s.window.queue_mut().release(frame)
    }

    /// `eglDestroySurface`.
    pub fn destroy_surface(&mut self, surface: u64) -> Result<(), OveloError> {
        let s = self
            .surfaces
            .get_mut(&surface)
            .ok_or_else(|| err("ui.egl.bad_surface"))?;
        s.destroyed = true;
        s.window.queue_mut().destroy();
        Ok(())
    }
}

/// GLES 2.0 state, clear-only subset.
#[derive(Debug)]
pub struct Gles {
    clear_color: [f32; 4],
    /// Commands issued since the last swap (diagnostics + feature ledger).
    pub calls: Vec<&'static str>,
}

impl Gles {
    fn new() -> Self {
        Self {
            clear_color: [0.0; 4],
            calls: Vec::new(),
        }
    }

    /// `glClearColor`.
    pub fn clear_color(&mut self, r: f32, g: f32, b: f32, a: f32) {
        self.clear_color = [r, g, b, a];
        self.calls.push("glClearColor");
    }

    /// `glClear(GL_COLOR_BUFFER_BIT)` — fills `buf` with the clear color
    /// (straight-alpha floats → premultiplied sRGB8).
    pub fn clear(&mut self, buf: &mut crate::surface::Surface) {
        self.calls.push("glClear");
        let u = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
        let c = Color::from_srgb8(
            u(self.clear_color[0]),
            u(self.clear_color[1]),
            u(self.clear_color[2]),
            u(self.clear_color[3]),
        );
        buf.clear(c);
    }

    /// Any draw call we cannot rasterize yet → `GPU_FEATURE`, never silent.
    pub fn draw_unsupported(&mut self, call: &'static str) -> Result<(), OveloError> {
        self.calls.push(call);
        Err(err("ui.gl.unsupported").with_detail(call))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bufq::BufferQueue;

    #[test]
    fn egl_flow_posts_frame() {
        let mut d = EglDisplay::initialize();
        assert!(d.choose_config(&CONFIG_RGBA8_ES2).is_some());
        assert!(d
            .choose_config(&EglConfig {
                depth: 24,
                ..CONFIG_RGBA8_ES2
            })
            .is_none());
        let ctx = d.create_context(None);
        d.make_current(ctx).unwrap();
        let s = d.create_window_surface(ANativeWindow::new(BufferQueue::new(2, 4, 4).unwrap()));
        {
            let gl = d.gl().unwrap();
            gl.clear_color(0.5, 0.5, 0.5, 1.0);
        }
        {
            let mut sc = crate::surface::Surface::new(4, 4).unwrap();
            d.gl().unwrap().clear(&mut sc);
        }
        let buf = d.surface_buffer(s).unwrap();
        buf.clear(crate::color::Color::from_srgb8(128, 128, 128, 255));
        let f = d.swap_buffers(s).unwrap();
        assert_eq!(f.generation, 0);
        // Host latches the posted frame, reads it, releases the buffer.
        let latched = d.acquire(s).unwrap();
        assert_eq!(latched, f);
        assert!(d.frame_pixels(s, latched).is_some());
        d.release(s, latched).unwrap();
    }
}

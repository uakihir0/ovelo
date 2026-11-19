//! The real display loop (design doc §08, G1): a winit window presenting
//! guest frames through `softbuffer` (CPU blit — the Metal drawable path
//! replaces the sink later; the BufferQueue→Frame contract is unchanged).
//!
//! One [`HostWindow`] per app instance: on every `about_to_wait` (the
//! vsync beat) it runs [`Presenter::display`], converts the latched frame
//! to softbuffer's `0x00RRGGBB` pixels and presents. Resize propagates
//! through [`Presenter::resize`] so stale frames drop on both sides.

use std::num::NonZeroU32;
use std::rc::Rc;

use ovelo_error::{ErrorCategory, OveloError, Phase, Result};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::window::{Window, WindowId};

use crate::present::Presenter;

fn err(cause: &str) -> OveloError {
    OveloError::new(ErrorCategory::HostCrash, Phase::Host, cause)
}

/// Premultiplied RGBA8 → softbuffer's `u32` pixels (alpha dropped —
/// the window surface is opaque).
fn to_sb(px: &[u8], out: &mut [u32]) {
    for (d, c) in out.iter_mut().zip(px.chunks_exact(4)) {
        *d = ((c[0] as u32) << 16) | ((c[1] as u32) << 8) | c[2] as u32;
    }
}

/// One app window pumping a [`Presenter`] surface to the screen.
pub struct HostWindow {
    title: String,
    surface_id: u64,
    presenter: Presenter,
    window: Option<Rc<Window>>,
    sb_ctx: Option<softbuffer::Context<Rc<Window>>>,
    sb_surf: Option<softbuffer::Surface<Rc<Window>, Rc<Window>>>,
}

impl HostWindow {
    /// A window showing `surface_id` of `presenter`.
    pub fn new(title: &str, surface_id: u64, presenter: Presenter) -> Self {
        Self {
            title: title.to_string(),
            surface_id,
            presenter,
            window: None,
            sb_ctx: None,
            sb_surf: None,
        }
    }

    /// The presenter (guest producer access for tests/drivers).
    pub fn presenter(&mut self) -> &mut Presenter {
        &mut self.presenter
    }
}

impl ApplicationHandler for HostWindow {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes().with_title(&self.title);
        let w = Rc::new(el.create_window(attrs).expect("window"));
        self.sb_ctx = Some(softbuffer::Context::new(w.clone()).expect("sb context"));
        self.sb_surf = Some(
            softbuffer::Surface::new(self.sb_ctx.as_ref().unwrap(), w.clone())
                .expect("sb surface"),
        );
        self.window = Some(w);
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, ev: WindowEvent) {
        match ev {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::Resized(sz) => {
                let (w, h) = (sz.width.max(1) as usize, sz.height.max(1) as usize);
                let _ = self.presenter.resize(self.surface_id, w, h);
                self.window.as_ref().unwrap().request_redraw();
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, _el: &ActiveEventLoop) {
        let Ok(Some(presented)) = self.presenter.display(self.surface_id) else {
            return;
        };
        let (w, h) = presented.frame.size;
        let surf = self.sb_surf.as_mut().unwrap();
        surf.resize(
            NonZeroU32::new(w).unwrap(),
            NonZeroU32::new(h).unwrap(),
        )
        .expect("resize");
        let mut buf = surf.buffer_mut().expect("buffer");
        to_sb(&presented.frame.buffer, &mut buf);
        buf.present().expect("present");
        self.window.as_ref().unwrap().request_redraw();
    }
}

/// Run the app window to completion (`EventLoop::run_app`).
pub fn run(mut app: HostWindow) -> Result<()> {
    let el = EventLoop::new().map_err(|e| err("host.eventloop").with_detail(e.to_string()))?;
    el.run_app(&mut app)
        .map_err(|e| err("host.eventloop").with_detail(e.to_string()))
}

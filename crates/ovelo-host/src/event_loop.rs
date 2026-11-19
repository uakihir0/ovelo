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
    for (d, c) in out.iter_mut().zip(px.as_chunks::<4>().0) {
        *d = ((c[0] as u32) << 16) | ((c[1] as u32) << 8) | c[2] as u32;
    }
}

/// Per-beat driver hook type ([`run`] installs one).
type VsyncHook = Box<dyn FnMut(&mut Presenter)>;

/// One app window pumping a [`Presenter`] surface to the screen.
pub struct HostWindow {
    title: String,
    surface_id: u64,
    presenter: Presenter,
    window: Option<Rc<Window>>,
    sb_ctx: Option<softbuffer::Context<Rc<Window>>>,
    sb_surf: Option<softbuffer::Surface<Rc<Window>, Rc<Window>>>,
    /// Per-beat driver hook (set by [`run`]).
    vsync_hook: Option<VsyncHook>,
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
            vsync_hook: None,
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
        if let Some(h) = &mut self.vsync_hook {
            h(&mut self.presenter);
        }
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
/// `on_vsync` runs each beat BEFORE display — the driver drains its
/// frame/command channel into the presenter there (guest work is the
/// only frame source once the loop owns the window).
pub fn run(mut app: HostWindow, mut on_vsync: impl FnMut(&mut Presenter) + 'static) -> Result<()> {
    let el = EventLoop::new().map_err(|e| err("host.eventloop").with_detail(e.to_string()))?;
    // Wake at ~60Hz rather than Poll: Poll busy-loops even when the
    // queue is empty; WaitUntil sleeps between beats like a vsync.
    el.set_control_flow(winit::event_loop::ControlFlow::WaitUntil(
        std::time::Instant::now() + std::time::Duration::from_millis(16),
    ));
    app.vsync_hook = Some(Box::new(move |p| on_vsync(p)));
    el.run_app(&mut app)
        .map_err(|e| err("host.eventloop").with_detail(e.to_string()))
}

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

use std::collections::VecDeque;

use ovelo_error::{ErrorCategory, OveloError, Phase, Result};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::window::{Window, WindowId};

use crate::input::{MotionAction, MotionEvent, PointerState};
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

/// Per-beat driver hook type ([`run`] installs one): the whole window is
/// passed so the driver can both feed the presenter and drain input.
type VsyncHook = Box<dyn FnMut(&mut HostWindow)>;

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
    /// Last cursor position (physical px, top-left origin).
    cursor: (f64, f64),
    /// Translated motion events waiting for the driver.
    input_q: VecDeque<MotionEvent>,
    key_q: VecDeque<crate::KeyInput>,
    /// Whether the primary pointer is currently down.
    pointer_down: bool,
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
            cursor: (0.0, 0.0),
            input_q: VecDeque::new(),
            key_q: VecDeque::new(),
            pointer_down: false,
        }
    }

    /// The presenter (guest producer access for tests/drivers).
    pub fn presenter(&mut self) -> &mut Presenter {
        &mut self.presenter
    }

    /// Drain queued motion events (surface px, top-left origin).
    pub fn take_input(&mut self) -> Vec<MotionEvent> {
        self.input_q.drain(..).collect()
    }

    /// Drain queued key events.
    pub fn take_keys(&mut self) -> Vec<crate::KeyInput> {
        self.key_q.drain(..).collect()
    }

    /// One-pointer motion event at `cursor`.
    fn motion(&self, action: MotionAction) -> MotionEvent {
        MotionEvent {
            action,
            pointers: vec![PointerState {
                id: 0,
                x: self.cursor.0 as f32,
                y: self.cursor.1 as f32,
                pressure: if self.pointer_down { 1.0 } else { 0.0 },
            }],
            action_index: 0,
            scroll: (0.0, 0.0),
        }
    }
}

impl ApplicationHandler for HostWindow {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title(&self.title)
            // Android applications are overwhelmingly designed around a
            // portrait phone window. Winit otherwise picks a small
            // landscape desktop default, which squeezes vertical
            // RecyclerViews out of existence before apps even run.
            .with_inner_size(LogicalSize::new(360.0, 640.0));
        let w = Rc::new(el.create_window(attrs).expect("window"));
        self.sb_ctx = Some(softbuffer::Context::new(w.clone()).expect("sb context"));
        self.sb_surf = Some(
            softbuffer::Surface::new(self.sb_ctx.as_ref().unwrap(), w.clone()).expect("sb surface"),
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
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = (position.x, position.y);
                if self.pointer_down {
                    self.input_q.push_back(self.motion(MotionAction::Move));
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                self.key_q.push_back(crate::map_key_input(&event));
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } => {
                self.pointer_down = true;
                self.input_q.push_back(self.motion(MotionAction::Down));
            }
            WindowEvent::MouseInput {
                state: ElementState::Released,
                button: MouseButton::Left,
                ..
            } => {
                // Clear first: a released pointer reports pressure 0.
                self.pointer_down = false;
                self.input_q.push_back(self.motion(MotionAction::Up));
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, _el: &ActiveEventLoop) {
        if let Some(mut h) = self.vsync_hook.take() {
            h(self);
            self.vsync_hook = Some(h);
        }
        let Ok(Some(presented)) = self.presenter.display(self.surface_id) else {
            return;
        };
        let (w, h) = presented.frame.size;
        let surf = self.sb_surf.as_mut().unwrap();
        surf.resize(NonZeroU32::new(w).unwrap(), NonZeroU32::new(h).unwrap())
            .expect("resize");
        let mut buf = surf.buffer_mut().expect("buffer");
        to_sb(&presented.frame.buffer, &mut buf);
        buf.present().expect("present");
        self.window.as_ref().unwrap().request_redraw();
    }
}

/// Run the app window to completion (`EventLoop::run_app`).
/// `on_vsync` runs each beat BEFORE display with the whole window — the
/// driver feeds `presenter()` and drains `take_input()` there (guest
/// work is the only frame source once the loop owns the window).
pub fn run(mut app: HostWindow, mut on_vsync: impl FnMut(&mut HostWindow) + 'static) -> Result<()> {
    let el = EventLoop::new().map_err(|e| err("host.eventloop").with_detail(e.to_string()))?;
    // Wake at ~60Hz rather than Poll: Poll busy-loops even when the
    // queue is empty; WaitUntil sleeps between beats like a vsync.
    el.set_control_flow(winit::event_loop::ControlFlow::WaitUntil(
        std::time::Instant::now() + std::time::Duration::from_millis(16),
    ));
    app.vsync_hook = Some(Box::new(move |w| on_vsync(w)));
    el.run_app(&mut app)
        .map_err(|e| err("host.eventloop").with_detail(e.to_string()))
}

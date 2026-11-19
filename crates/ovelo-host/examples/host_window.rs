//! Minimal host window: opens a winit window and exercises the event
//! plumbing (Esc → Back, ⌘C/V hooks, IME preedit/commit, pointer translation).
//!
//! Run locally on macOS:
//!   cargo run -p ovelo-host --example host_window
//! (Build-only in CI — a real window needs a display server.)

use ovelo_host::HostApp;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::window::{Window, WindowId};

struct App {
    host: Option<HostApp>,
    window: Option<Window>,
    cmd: bool,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        let host = HostApp::new("ovelo host", 2.0, (800, 600));
        let attrs = Window::default_attributes()
            .with_title(&host.title)
            .with_inner_size(winit::dpi::LogicalSize::new(800.0, 600.0));
        match el.create_window(attrs) {
            Ok(w) => {
                w.set_ime_allowed(true);
                self.window = Some(w);
                self.host = Some(host);
            }
            Err(e) => eprintln!("window: {e}"),
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, ev: WindowEvent) {
        let Some(host) = &mut self.host else { return };
        match ev {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::ModifiersChanged(m) => {
                self.cmd = m.state().super_key();
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state == ElementState::Pressed {
                    if let Some(k) = ovelo_host::map_host_key(&event.logical_key, self.cmd)
                    {
                        println!("host key: {k:?}");
                    }
                }
            }
            WindowEvent::Ime(i) => {
                println!("ime: {:?}", ovelo_host::map_ime_event(&i));
            }
            WindowEvent::CursorMoved { position, .. } => {
                if host
                    .translator
                    .pointer_move(0, position.x, position.y)
                    .is_some()
                {
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let id = match button {
                    MouseButton::Left => 0,
                    MouseButton::Right => 1,
                    MouseButton::Middle => 2,
                    _ => 3,
                };
                let _ = (state, id); // cursor position arrives via CursorMoved
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (x as f64 * 16.0, y as f64 * 16.0),
                    MouseScrollDelta::PixelDelta(p) => (p.x, p.y),
                };
                println!("scroll: {dx} {dy}");
            }
            _ => {}
        }
    }
}

fn main() {
    let el = EventLoop::new().expect("event loop");
    let mut app = App {
        host: None,
        window: None,
        cmd: false,
    };
    el.run_app(&mut app).expect("run");
}

//! winit glue: one window per app instance (design doc §08).
//!
//! Kept deliberately thin — everything testable lives in [`crate::input`],
//! [`crate::ime`] and [`crate::frame`]. `HostApp` owns the winit
//! `ApplicationHandler` state and exposes the wiring (`window_fn` dispatch,
//! Esc → Back, ⌘C/V clipboard hooks) without doing guest work itself.

use ovelo_error::Result;

/// Keyboard shortcuts the host intercepts before the guest sees them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostKey {
    /// Esc → Android Back.
    Back,
    /// ⌘C → copy hook (feeds the Android clipboard service).
    Copy,
    /// ⌘V → paste hook.
    Paste,
    /// ⌘X → cut hook.
    Cut,
}

/// Map a logical key + modifier state to a host-intercepted action.
/// `Escape` always maps to Android Back; ⌘-letter maps to clipboard verbs.
/// Pure function — unit-testable without a window.
pub fn map_host_key(key: &winit::keyboard::Key, cmd: bool) -> Option<HostKey> {
    use winit::keyboard::{Key, NamedKey};
    match key {
        Key::Named(NamedKey::Escape) => Some(HostKey::Back),
        Key::Character(c) if cmd => match c.to_lowercase().as_str() {
            "c" => Some(HostKey::Copy),
            "v" => Some(HostKey::Paste),
            "x" => Some(HostKey::Cut),
            _ => None,
        },
        _ => None,
    }
}

/// Runtime state for one host window. Constructed inside a winit
/// `ApplicationHandler`; kept as a plain struct so non-AppKit logic is testable.
pub struct HostApp {
    /// Window title shown in the title bar and app switcher.
    pub title: String,
    /// Requested surface size in px.
    pub surface_px: (u32, u32),
    /// Frame contract gate (§02).
    pub registry: crate::frame::SurfaceRegistry,
    /// Input translator bound to the surface geometry.
    pub translator: crate::input::Translator,
}

impl HostApp {
    /// New host window state for `title` and surface geometry.
    pub fn new(title: &str, scale: f64, surface_px: (u32, u32)) -> Self {
        Self {
            title: title.to_string(),
            surface_px,
            registry: crate::frame::SurfaceRegistry::new(),
            translator: crate::input::Translator::new(
                scale,
                (surface_px.0 as f64, surface_px.1 as f64),
                (0.0, 0.0),
            ),
        }
    }

    /// Resize: keep translator geometry and surface generation in sync —
    /// in-flight frames for the old size become stale (§02).
    pub fn resize(&mut self, surface_id: u64, surface_px: (u32, u32)) -> Result<u64> {
        // Bump first: on an unknown surface nothing else may change.
        let gen = self.registry.bump_generation(surface_id)?;
        self.surface_px = surface_px;
        self.translator
            .resize((surface_px.0 as f64, surface_px.1 as f64), (0.0, 0.0));
        Ok(gen)
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use winit::keyboard::{Key, NamedKey};

    #[test]
    fn esc_is_back_cmd_letters_are_clipboard() {
        assert_eq!(
            map_host_key(&Key::Named(NamedKey::Escape), false),
            Some(HostKey::Back)
        );
        assert_eq!(
            map_host_key(&Key::Character("C".into()), true),
            Some(HostKey::Copy)
        );
        assert_eq!(
            map_host_key(&Key::Character("v".into()), true),
            Some(HostKey::Paste)
        );
        assert_eq!(map_host_key(&Key::Character("c".into()), false), None);
        assert_eq!(
            map_host_key(&Key::Named(NamedKey::Space), false),
            None
        );
    }

    #[test]
    fn resize_bumps_generation() {
        let mut app = HostApp::new("x", 2.0, (640, 480));
        let sid = app.registry.create(7);
        assert_eq!(app.resize(7, (800, 600)).unwrap(), sid + 1);
        assert_eq!(
            app.resize(99, (800, 600)).unwrap_err().cause(),
            "host.surface.unknown"
        );
    }
}

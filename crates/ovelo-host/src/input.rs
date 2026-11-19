//! macOS pointer/scroll → Android `MotionEvent` (design doc §08 "Mouse and
//! scroll").
//!
//! macOS delivers points in window coordinates (points, bottom-left origin);
//! Android MotionEvents are surface pixels, top-left origin. [`Translator`]
//! applies display scale + insets and tracks multi-pointer state so
//! down/move/up/cancel arrive in the right order.

/// Android motion action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MotionAction {
    /// First pointer touched.
    Down,
    /// Subsequent pointer while one is down.
    PointerDown,
    /// Position update.
    Move,
    /// Last pointer released.
    Up,
    /// Non-last pointer released.
    PointerUp,
    /// Gesture cancelled (app backgrounded, system grab).
    Cancel,
    /// Scroll wheel / trackpad pan.
    Scroll,
}

/// One pointer in a motion event.
#[derive(Debug, Clone, PartialEq)]
pub struct PointerState {
    /// Stable pointer id (button index or touch id).
    pub id: u32,
    /// Surface-space position (px, top-left origin).
    pub x: f32,
    /// Surface-space position.
    pub y: f32,
    /// Pressure 0..1 (mouse = 1 when down).
    pub pressure: f32,
}

/// A translated motion event.
#[derive(Debug, Clone)]
pub struct MotionEvent {
    /// Action.
    pub action: MotionAction,
    /// All active pointers, stable order.
    pub pointers: Vec<PointerState>,
    /// Index of `pointers` the action refers to.
    pub action_index: usize,
    /// Scroll delta in px (`action == Scroll` only).
    pub scroll: (f32, f32),
}

/// Converts window points → surface px and tracks pointer state.
pub struct Translator {
    scale: f64,
    /// Surface size in px (height flips Y).
    surface_px: (f64, f64),
    /// Content inset in window points (e.g. letterboxing).
    inset: (f64, f64),
    pointers: Vec<PointerState>,
}

impl Translator {
    /// `scale`: display backing factor (2.0 for Retina). `surface_px`: guest
    /// surface size in pixels. `inset`: letterbox offset in window points.
    pub fn new(scale: f64, surface_px: (f64, f64), inset: (f64, f64)) -> Self {
        Self {
            scale,
            surface_px,
            inset,
            pointers: Vec::new(),
        }
    }

    /// Window point (points, bottom-left origin) → surface px.
    pub fn to_surface(&self, x: f64, y: f64) -> (f32, f32) {
        let sx = (x - self.inset.0) * self.scale;
        // flip Y: surface origin is top-left
        let sy = (self.surface_px.1 / self.scale - (y - self.inset.1)) * self.scale;
        (sx as f32, sy as f32)
    }

    /// Register a pointer down; returns the event (Down if first, PointerDown
    /// otherwise).
    pub fn pointer_down(&mut self, id: u32, x: f64, y: f64) -> MotionEvent {
        let (sx, sy) = self.to_surface(x, y);
        let idx = self.pointers.len();
        self.pointers.push(PointerState {
            id,
            x: sx,
            y: sy,
            pressure: 1.0,
        });
        MotionEvent {
            action: if idx == 0 {
                MotionAction::Down
            } else {
                MotionAction::PointerDown
            },
            pointers: self.pointers.clone(),
            action_index: idx,
            scroll: (0.0, 0.0),
        }
    }

    /// Move an existing pointer; unknown ids are ignored (returns `None`).
    pub fn pointer_move(&mut self, id: u32, x: f64, y: f64) -> Option<MotionEvent> {
        let (sx, sy) = self.to_surface(x, y);
        let idx = self.pointers.iter().position(|p| p.id == id)?;
        self.pointers[idx].x = sx;
        self.pointers[idx].y = sy;
        Some(MotionEvent {
            action: MotionAction::Move,
            pointers: self.pointers.clone(),
            action_index: idx,
            scroll: (0.0, 0.0),
        })
    }

    /// Release a pointer; `Up` when it was the last one down.
    pub fn pointer_up(&mut self, id: u32, x: f64, y: f64) -> Option<MotionEvent> {
        let (sx, sy) = self.to_surface(x, y);
        let idx = self.pointers.iter().position(|p| p.id == id)?;
        self.pointers[idx].x = sx;
        self.pointers[idx].y = sy;
        self.pointers[idx].pressure = 0.0;
        let last = self.pointers.len() == 1;
        let ev = MotionEvent {
            action: if last {
                MotionAction::Up
            } else {
                MotionAction::PointerUp
            },
            pointers: self.pointers.clone(),
            action_index: idx,
            scroll: (0.0, 0.0),
        };
        self.pointers.remove(idx);
        Some(ev)
    }

    /// Cancel all pointers (system grabbed the event stream).
    pub fn cancel(&mut self) -> MotionEvent {
        let ev = MotionEvent {
            action: MotionAction::Cancel,
            pointers: self.pointers.clone(),
            action_index: 0,
            scroll: (0.0, 0.0),
        };
        self.pointers.clear();
        ev
    }

    /// Scroll: `dx`/`dy` in window points → surface-px scroll event.
    pub fn scroll(&self, x: f64, y: f64, dx: f64, dy: f64) -> MotionEvent {
        let (sx, sy) = self.to_surface(x, y);
        MotionEvent {
            action: MotionAction::Scroll,
            pointers: vec![PointerState {
                id: 0,
                x: sx,
                y: sy,
                pressure: 0.0,
            }],
            action_index: 0,
            scroll: ((dx * self.scale) as f32, (dy * self.scale) as f32),
        }
    }

    /// Update geometry on resize/orientation change.
    pub fn resize(&mut self, surface_px: (f64, f64), inset: (f64, f64)) {
        self.surface_px = surface_px;
        self.inset = inset;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t() -> Translator {
        // Retina 2x, 800×600 surface, no inset
        Translator::new(2.0, (800.0, 600.0), (0.0, 0.0))
    }

    #[test]
    fn points_to_surface() {
        let t = t();
        // window 400×300pt; top-left of window (y=300) → surface y=0
        assert_eq!(t.to_surface(0.0, 300.0), (0.0, 0.0));
        assert_eq!(t.to_surface(10.0, 290.0), (20.0, 20.0));
    }

    #[test]
    fn multi_pointer_order() {
        let mut t = t();
        let d1 = t.pointer_down(0, 1.0, 299.0);
        assert_eq!(d1.action, MotionAction::Down);
        let d2 = t.pointer_down(1, 2.0, 298.0);
        assert_eq!(d2.action, MotionAction::PointerDown);
        assert_eq!(d2.pointers.len(), 2);
        let u2 = t.pointer_up(1, 2.0, 298.0).unwrap();
        assert_eq!(u2.action, MotionAction::PointerUp);
        assert_eq!(u2.action_index, 1);
        let u1 = t.pointer_up(0, 1.0, 299.0).unwrap();
        assert_eq!(u1.action, MotionAction::Up);
        assert!(t.pointers.is_empty());
    }

    #[test]
    fn scroll_and_cancel() {
        let mut t = t();
        t.pointer_down(0, 1.0, 1.0);
        let c = t.cancel();
        assert_eq!(c.action, MotionAction::Cancel);
        assert!(t.pointers.is_empty());
        let s = t.scroll(5.0, 295.0, 3.0, -2.0);
        assert_eq!(s.scroll, (6.0, -4.0));
    }
}

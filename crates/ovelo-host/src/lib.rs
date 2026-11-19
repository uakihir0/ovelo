//! ovelo-host: macOS host layer — window/surface presentation, input
//! translation and IME plumbing (design doc §02, §08).
//!
//! Library choice: `winit` (0.30). One crate gives us the NSWindow-backed
//! window, mouse/trackpad events, `Ime` preedit/commit callbacks and
//! modifiers on macOS while still compiling on Linux CI. A CAMetalLayer
//! presentation path (§02 Worker→Display) will sit behind [`SurfaceRegistry`]:
//! the registry enforces "stale workers and incomplete frames are never
//! displayed" and generation tracking drops stale frames on resize — the
//! actual GPU blit is a TODO documented at [`SurfaceRegistry::present`].
//!
//! Modules:
//! * [`frame`] — `Frame`/`SurfaceRegistry`: the Worker→Display contract.
//! * [`input`] — macOS points → Android `MotionEvent` model (§08 "Mouse and
//!   scroll"), multi-pointer ids, scroll amounts, display scale/insets.
//! * [`ime`] — `InputConnection`-shaped trait + composing/commit/cancel/
//!   selection mapping from `winit::event::Ime` (§08 "Japanese IME").
//! * [`launch`] — `.app` bundle layout generation (§08 "Launch, switch,
//!   quit"); LaunchServices registration is structural, not executed.
//! * [`window`] — thin winit glue (one window per app instance, Esc → Back,
//!   ⌘C/V clipboard hooks). Untestable headless parts stay here and thin.

mod frame;
mod ime;
mod input;
mod launch;
mod window;

pub use frame::{ColorSpace, Frame, SurfaceRegistry};
pub use ime::{map_winit as map_ime_event, ImeEvent, InputConnection};
pub use input::{MotionAction, MotionEvent, PointerState, Translator};
pub use launch::{bundle_layout, LaunchRecord};
pub use window::{map_host_key, HostApp, HostKey};

pub use ovelo_error;

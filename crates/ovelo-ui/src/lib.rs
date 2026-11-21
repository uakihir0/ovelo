//! ovelo-ui: CPU raster surface and Minikin-subset text layout.
//!
//! Design doc §06 (Text/Drawing) and §07 (color correctness):
//! * [`surface`] — deterministic premultiplied-RGBA raster with source-over,
//!   clip stack, rects/rounded rects/lines/blits.
//! * [`color`] — the single sRGB↔linear path; [`surface::gray_gradient_diagnostic`]
//!   isolates missing/double conversion.
//! * [`font`] — `ttf-parser`/`rustybuzz` backed fonts behind [`font::GlyphSource`]
//!   with ordered fallback ([`font::FontSet`]).
//! * [`text`] — BiDi + UAX#14 line breaking (kinsoku), wrap, ellipsis and
//!   exact integer measurement.

pub mod bufq;
pub mod canvas;
pub mod color;
pub mod egl;
pub mod font;
pub mod surface;
pub mod text;
pub mod vulkan;

pub use bufq::{ANativeWindow, BufferQueue, FrameId};
pub use canvas::{Canvas, Paint};
pub use color::Color;
pub use egl::{EglConfig, EglDisplay, CONFIG_RGBA8_ES2};
pub use font::{FontSet, GlyphSource, TtfFont};
pub use surface::{Rect, Surface};
pub use text::{layout, Line, TextLayout, TextStyle};
pub use vulkan::{VkInstance, VkSurface, VkSwapchain};

pub use ovelo_error;

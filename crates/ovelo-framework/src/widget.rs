//! Concrete widgets (`TextView`, `Button`) and the `Drawable` contract:
//! measure → layout → draw through a [`Canvas`], with `invalidate`/`isDirty`
//! feeding [`ViewRootImpl`](crate::root::ViewRootImpl) traversals.
//!
//! `TextView` measures through the Minikin-subset layout (wrapping when a
//! max width is imposed) and draws per-line at the computed baseline.

use crate::view::{Length, MeasureSpec, Size, View, ViewGroup};
use ovelo_ui::font::FontSet;
use ovelo_ui::{Canvas, Color, Paint, Rect, TextStyle};
use std::rc::Rc;

/// A node in the view tree (`dispatchDraw` contract).
pub trait Drawable {
    /// `onMeasure` — resolve against the specs.
    fn measure(&mut self, w: MeasureSpec, h: MeasureSpec) -> Size;
    /// `onLayout` — fix the rect.
    fn layout(&mut self, left: i32, top: i32);
    /// `draw`/`dispatchDraw` — paint into the canvas (already translated
    /// to the parent's origin; implementations translate by self.left/top).
    fn draw(&mut self, c: &mut Canvas);
    /// `invalidate` — request redraw.
    fn invalidate(&mut self);
    /// `isDirty` — self or any descendant needs drawing.
    fn is_dirty(&self) -> bool;
    /// Measured size (post-`measure`).
    fn measured(&self) -> Size;
    /// Laid-out origin.
    fn origin(&self) -> (i32, i32);
    /// Requested layout params (for the parent's `for_child` spec).
    fn lp(&self) -> (Length, Length);
}

impl Drawable for View {
    fn measure(&mut self, w: MeasureSpec, h: MeasureSpec) -> Size {
        self.measure(w, h)
    }
    fn layout(&mut self, left: i32, top: i32) {
        self.layout(left, top)
    }
    fn draw(&mut self, c: &mut Canvas) {
        if let Some(bg) = self.background {
            let s = self.measured;
            c.draw_rect(Rect::new(self.left, self.top, s.w, s.h), Paint::color(bg));
        }
    }
    fn invalidate(&mut self) {
        self.invalidate()
    }
    fn is_dirty(&self) -> bool {
        self.is_dirty()
    }
    fn measured(&self) -> Size {
        self.measured
    }
    fn origin(&self) -> (i32, i32) {
        (self.left, self.top)
    }
    fn lp(&self) -> (Length, Length) {
        (self.lp_w, self.lp_h)
    }
}

impl Drawable for ViewGroup {
    fn measure(&mut self, w: MeasureSpec, h: MeasureSpec) -> Size {
        self.measure(w, h)
    }
    fn layout(&mut self, left: i32, top: i32) {
        self.layout(left, top)
    }
    fn draw(&mut self, c: &mut Canvas) {
        let s = self.view.measured;
        if let Some(bg) = self.view.background {
            c.draw_rect(Rect::new(self.view.left, self.view.top, s.w, s.h), Paint::color(bg));
        }
        // Children draw at their own laid-out origins (absolute coords).
        for ch in &mut self.children {
            ch.draw(c);
        }
    }
    fn invalidate(&mut self) {
        self.view.invalidate();
        for ch in &mut self.children {
            ch.invalidate();
        }
    }
    fn is_dirty(&self) -> bool {
        self.view.is_dirty() || self.children.iter().any(|c| c.is_dirty())
    }
    fn measured(&self) -> Size {
        self.view.measured
    }
    fn origin(&self) -> (i32, i32) {
        (self.view.left, self.view.top)
    }
    fn lp(&self) -> (Length, Length) {
        (self.view.lp_w, self.view.lp_h)
    }
}

/// `TextView` — single-style text with optional background.
pub struct TextView {
    /// Underlying view params/state.
    pub view: View,
    /// Text content.
    pub text: String,
    /// Text color.
    pub color: Color,
    /// Font set shared with the root.
    pub fonts: Rc<FontSet>,
    /// Em size in px.
    pub text_size: f32,
    /// Uniform padding px.
    pub padding: i32,
}

impl TextView {
    /// Wrap-content text view.
    pub fn new(text: impl Into<String>, text_size: f32, color: Color, fonts: Rc<FontSet>) -> Self {
        let mut v = Self {
            view: View::new(Length::WrapContent, Length::WrapContent, Size::default()),
            text: text.into(),
            color,
            fonts,
            text_size,
            padding: 0,
        };
        v.view.desired = v.measure_text(None);
        v
    }

    /// `setText` — re-measure and invalidate.
    pub fn set_text(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.view.desired = self.measure_text(None);
        self.view.invalidate();
    }

    fn measure_text(&self, max_w: Option<i32>) -> Size {
        let t = ovelo_ui::layout(
            &self.text,
            TextStyle {
                size_px: self.text_size,
            },
            &self.fonts,
            max_w,
            false,
        );
        Size {
            w: t.width + self.padding * 2,
            h: t.height + self.padding * 2,
        }
    }
}

impl Drawable for TextView {
    fn measure(&mut self, w: MeasureSpec, h: MeasureSpec) -> Size {
        let cap = match w {
            MeasureSpec::Exactly(s) | MeasureSpec::AtMost(s) => Some(s - self.padding * 2),
            MeasureSpec::Unspecified => None,
        };
        self.view.desired = self.measure_text(cap.map(|c| c.max(0)));
        self.view.measure(w, h)
    }
    fn layout(&mut self, left: i32, top: i32) {
        self.view.layout(left, top)
    }
    fn draw(&mut self, c: &mut Canvas) {
        let s = self.view.measured;
        if let Some(bg) = self.view.background {
            c.draw_rect(Rect::new(self.view.left, self.view.top, s.w, s.h), Paint::color(bg));
        }
        let t = ovelo_ui::layout(
            &self.text,
            TextStyle {
                size_px: self.text_size,
            },
            &self.fonts,
            Some((s.w - self.padding * 2).max(0)),
            false,
        );
        let lh = t.ascent + t.descent;
        let mut baseline = self.view.top + self.padding + t.ascent;
        for line in &t.lines {
            let seg = &self.text[line.range.0..line.range.1];
            c.draw_text(
                seg,
                self.view.left + self.padding,
                baseline,
                Paint {
                    color: self.color,
                    text_size: self.text_size,
                },
            );
            baseline += lh;
        }
    }
    fn invalidate(&mut self) {
        self.view.invalidate()
    }
    fn is_dirty(&self) -> bool {
        self.view.is_dirty()
    }
    fn measured(&self) -> Size {
        self.view.measured
    }
    fn origin(&self) -> (i32, i32) {
        (self.view.left, self.view.top)
    }
    fn lp(&self) -> (Length, Length) {
        (self.view.lp_w, self.view.lp_h)
    }
}

/// `Button` — TextView with a rounded background and padding.
pub struct Button {
    /// The text view it delegates to.
    pub tv: TextView,
    /// Background corner radius px.
    pub radius: i32,
    /// Background fill.
    pub bg: Color,
}

impl Button {
    /// Wrap-content button.
    pub fn new(text: impl Into<String>, text_size: f32, color: Color, fonts: Rc<FontSet>) -> Self {
        let mut b = Self {
            tv: TextView::new(text, text_size, color, fonts),
            radius: 4,
            bg: Color::srgb(0xDD, 0xDD, 0xDD),
        };
        b.tv.padding = 12;
        b.tv.view.desired = b.tv.measure_text(None);
        b
    }
}

impl Drawable for Button {
    fn measure(&mut self, w: MeasureSpec, h: MeasureSpec) -> Size {
        self.tv.measure(w, h)
    }
    fn layout(&mut self, left: i32, top: i32) {
        self.tv.layout(left, top)
    }
    fn draw(&mut self, c: &mut Canvas) {
        let s = self.tv.view.measured;
        c.draw_round_rect(
            Rect::new(self.tv.view.left, self.tv.view.top, s.w, s.h),
            self.radius,
            Paint::color(self.bg),
        );
        // The label draws inside the bg; skip tv's own background pass.
        let keep = self.tv.view.background.take();
        self.tv.draw(c);
        self.tv.view.background = keep;
    }
    fn invalidate(&mut self) {
        self.tv.invalidate()
    }
    fn is_dirty(&self) -> bool {
        self.tv.is_dirty()
    }
    fn measured(&self) -> Size {
        self.tv.measured()
    }
    fn origin(&self) -> (i32, i32) {
        self.tv.origin()
    }
    fn lp(&self) -> (Length, Length) {
        self.tv.lp()
    }
}

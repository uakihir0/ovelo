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
    /// Dex-assigned frame `(l, t, r, b)` relative to the parent, when
    /// the guest already laid this node out — the parent's `layout`
    /// positions the child at `(left + l, top + t)` instead of its
    /// own stacking/overlay heuristic.
    fn dex_frame(&self) -> Option<(i32, i32, i32, i32)> {
        None
    }
    /// `dispatchTouchEvent` (tap): `down` presses the topmost clickable
    /// view under the point, `!down` releases it — a click only fires
    /// when press and release land on the same view (Android's
    /// `performClick` on ACTION_UP inside a pressed view).
    fn dispatch_touch(&mut self, down: bool, x: i32, y: i32) -> bool {
        let _ = (down, x, y);
        false
    }
}

/// Whether `(x,y)` is inside the view's laid-out rect.
fn inside(view: &View, x: i32, y: i32) -> bool {
    let s = view.measured;
    x >= view.left && x < view.left + s.w && y >= view.top && y < view.top + s.h
}

/// ACTION_DOWN: mark the view pressed when it is clickable and hit.
fn press_hit(view: &mut View, x: i32, y: i32) -> bool {
    if inside(view, x, y) && view.on_click.is_some() {
        view.pressed = true;
        return true;
    }
    false
}

/// ACTION_UP: a pressed view consumes the release; the click fires
/// only when the release lands inside the same view.
fn release_hit(view: &mut View, x: i32, y: i32) -> bool {
    if !view.pressed {
        return false;
    }
    view.pressed = false;
    if inside(view, x, y) {
        if let Some(f) = &mut view.on_click {
            f();
        }
        view.invalidate();
    }
    true
}

impl Drawable for View {
    fn dispatch_touch(&mut self, down: bool, x: i32, y: i32) -> bool {
        if down {
            press_hit(self, x, y)
        } else {
            release_hit(self, x, y)
        }
    }
    fn measure(&mut self, w: MeasureSpec, h: MeasureSpec) -> Size {
        self.measure(w, h)
    }
    fn layout(&mut self, left: i32, top: i32) {
        self.layout(left, top)
    }
    fn draw(&mut self, c: &mut Canvas) {
        let s = self.measured;
        if let Some(bg) = self.background {
            c.draw_rect(Rect::new(self.left, self.top, s.w, s.h), Paint::color(bg));
        }
        if let Some((iw, ih, px)) = &self.image {
            c.draw_image(
                self.left,
                self.top,
                s.w.max(0) as usize,
                s.h.max(0) as usize,
                *iw as usize,
                *ih as usize,
                px,
            );
        }
        if let Some(ops) = self.custom_ops.clone() {
            // Guest `onDraw` ops are view-local — translate into the
            // view's absolute origin before replaying.
            c.save();
            c.translate(self.left, self.top);
            replay_guest_ops(c, &ops);
            c.restore();
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
    fn dispatch_touch(&mut self, down: bool, x: i32, y: i32) -> bool {
        if down {
            // Topmost first: later-drawn children get the first shot.
            for (i, ch) in self.children.iter_mut().enumerate().rev() {
                if ch.dispatch_touch(true, x, y) {
                    self.pressed_child = Some(i);
                    return true;
                }
            }
            press_hit(&mut self.view, x, y)
        } else {
            if let Some(i) = self.pressed_child.take() {
                if let Some(ch) = self.children.get_mut(i) {
                    return ch.dispatch_touch(false, x, y);
                }
            }
            release_hit(&mut self.view, x, y)
        }
    }
    fn measure(&mut self, w: MeasureSpec, h: MeasureSpec) -> Size {
        self.measure(w, h)
    }
    fn layout(&mut self, left: i32, top: i32) {
        self.layout(left, top)
    }
    fn draw(&mut self, c: &mut Canvas) {
        let s = self.view.measured;
        if let Some(bg) = self.view.background {
            c.draw_rect(
                Rect::new(self.view.left, self.view.top, s.w, s.h),
                Paint::color(bg),
            );
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
    fn dispatch_touch(&mut self, down: bool, x: i32, y: i32) -> bool {
        if down {
            press_hit(&mut self.view, x, y)
        } else {
            release_hit(&mut self.view, x, y)
        }
    }
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
            c.draw_rect(
                Rect::new(self.view.left, self.view.top, s.w, s.h),
                Paint::color(bg),
            );
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
    /// `setOnClickListener` — fired on tap inside the rect.
    pub on_click: Option<Box<dyn FnMut()>>,
}

impl Button {
    /// Wrap-content button.
    pub fn new(text: impl Into<String>, text_size: f32, color: Color, fonts: Rc<FontSet>) -> Self {
        let mut b = Self {
            tv: TextView::new(text, text_size, color, fonts),
            radius: 4,
            bg: Color::srgb(0xDD, 0xDD, 0xDD),
            on_click: None,
        };
        b.tv.padding = 12;
        b.tv.view.desired = b.tv.measure_text(None);
        b
    }
}

impl Drawable for Button {
    fn dispatch_touch(&mut self, down: bool, x: i32, y: i32) -> bool {
        if down {
            if inside(&self.tv.view, x, y)
                && (self.tv.view.on_click.is_some() || self.on_click.is_some())
            {
                self.tv.view.pressed = true;
                return true;
            }
            false
        } else {
            if !self.tv.view.pressed {
                return false;
            }
            self.tv.view.pressed = false;
            if inside(&self.tv.view, x, y) {
                if let Some(f) = &mut self.tv.view.on_click {
                    f();
                } else if let Some(f) = &mut self.on_click {
                    f();
                }
                self.tv.invalidate();
            }
            true
        }
    }
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

/// Replay a guest `onDraw` recording: each [`ovelo_dexrt::CanvasOp`]
/// maps onto the `Canvas` subset (already translated to the view's
/// absolute origin by the caller). Stroke-styled shapes approximate
/// as outlines via `draw_line`; `drawText` honors LEFT/CENTER/RIGHT
/// alignment using the shared font metrics.
fn replay_guest_ops(c: &mut Canvas, ops: &[ovelo_dexrt::CanvasOp]) {
    use ovelo_dexrt::CanvasOp as O;
    let paint = |p: &ovelo_dexrt::PaintState| Paint {
        color: Color::from_srgb8(
            (p.argb >> 16) as u8,
            (p.argb >> 8) as u8,
            p.argb as u8,
            (p.argb >> 24) as u8,
        ),
        text_size: p.text_size.max(1.0),
    };
    let stroke_or_fill = |c: &mut Canvas, r: Rect, p: &ovelo_dexrt::PaintState, rad: i32| {
        let col = paint(p);
        if p.style == 1 {
            // STROKE — outline via edge lines at the stroke width.
            let w = (p.stroke_w.max(1.0)) as i32;
            c.draw_line(r.x, r.y, r.x1, r.y, w, col);
            c.draw_line(r.x1, r.y, r.x1, r.y1, w, col);
            c.draw_line(r.x1, r.y1, r.x, r.y1, w, col);
            c.draw_line(r.x, r.y1, r.x, r.y, w, col);
        } else if rad > 0 {
            c.draw_round_rect(r, rad, col);
        } else {
            c.draw_rect(r, col);
        }
    };
    for op in ops {
        match op {
            O::Save => c.save(),
            O::Restore => c.restore(),
            O::Translate { dx, dy } => c.translate(*dx as i32, *dy as i32),
            O::ClipRect { l, t, r, b } => c.clip_rect(Rect::new(
                *l as i32,
                *t as i32,
                (*r - *l) as i32,
                (*b - *t) as i32,
            )),
            O::Color { argb } => c.draw_color(Color::from_srgb8(
                (argb >> 16) as u8,
                (argb >> 8) as u8,
                *argb as u8,
                (argb >> 24) as u8,
            )),
            O::Rect { l, t, r, b, paint: p } => {
                let r = Rect::new(*l as i32, *t as i32, (*r - *l) as i32, (*b - *t) as i32);
                stroke_or_fill(c, r, p, 0);
            }
            O::RoundRect {
                l,
                t,
                r,
                b,
                rx,
                ry,
                paint: p,
            } => {
                let r = Rect::new(*l as i32, *t as i32, (*r - *l) as i32, (*b - *t) as i32);
                stroke_or_fill(c, r, p, rx.max(*ry) as i32);
            }
            O::Oval { l, t, r, b, paint: p } => {
                let r = Rect::new(*l as i32, *t as i32, (*r - *l) as i32, (*b - *t) as i32);
                let rad = (r.x1 - r.x).min(r.y1 - r.y) / 2;
                stroke_or_fill(c, r, p, rad);
            }
            O::Circle {
                cx,
                cy,
                r,
                paint: p,
            } => {
                let rect = Rect::new(
                    (*cx - *r) as i32,
                    (*cy - *r) as i32,
                    (*r * 2.0) as i32,
                    (*r * 2.0) as i32,
                );
                stroke_or_fill(c, rect, p, *r as i32);
            }
            O::Line {
                x0,
                y0,
                x1,
                y1,
                paint: p,
            } => {
                c.draw_line(
                    *x0 as i32,
                    *y0 as i32,
                    *x1 as i32,
                    *y1 as i32,
                    p.stroke_w.max(1.0) as i32,
                    paint(p),
                );
            }
            O::Text { text, x, y, paint: p } => {
                let mut px = *x;
                if p.align == 1 {
                    px -= c.measure(text, p.text_size.max(1.0)) / 2.0;
                } else if p.align == 2 {
                    px -= c.measure(text, p.text_size.max(1.0));
                }
                c.draw_text(text, px as i32, *y as i32, paint(p));
            }
            O::Bitmap {
                x,
                y,
                w,
                h,
                src_w,
                src_h,
                rgba,
            } => {
                let (dw, dh) = if *w == 0 || *h == 0 {
                    (*src_w as usize, *src_h as usize)
                } else {
                    (*w as usize, *h as usize)
                };
                if dw > 0 && dh > 0 && !rgba.is_empty() {
                    c.draw_image(
                        *x as i32,
                        *y as i32,
                        dw,
                        dh,
                        *src_w as usize,
                        *src_h as usize,
                        rgba,
                    );
                }
            }
        }
    }
}

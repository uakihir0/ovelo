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
    /// Downcast hook — the guest bridge needs the concrete wrapper
    /// (`SharedView`/`AbsView`) to match a child box to its dex handle
    /// for positional insert/remove.
    fn as_any(&mut self) -> Option<&mut dyn std::any::Any> {
        None
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
        // AOSP's default `onMeasure` is `getDefaultSize` — a plain View
        // FILLS the spec under EXACTLY and AT_MOST (only UNSPECIFIED
        // reports the desired size). Widgets that wrap content
        // (TextView, Button, groups) bypass this via their own measure
        // which computes `desired` first, so this fill only applies to
        // the bare leaf — SurfaceView/custom canvases, which otherwise
        // measured 0x0 and never drew (andstatus 2048's GLSurfaceView).
        self.measured = Size {
            w: match w {
                MeasureSpec::Exactly(s) | MeasureSpec::AtMost(s) => s,
                MeasureSpec::Unspecified => self.desired.w,
            },
            h: match h {
                MeasureSpec::Exactly(s) | MeasureSpec::AtMost(s) => s,
                MeasureSpec::Unspecified => self.desired.h,
            },
        };
        self.measured
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
            let (pl, pt, pr, pb) = self.padding;
            let cw = (s.w - pl - pr).max(0);
            let ch = (s.h - pt - pb).max(0);
            if cw > 0 && ch > 0 && *iw > 0 && *ih > 0 {
                let fit = (cw as f32 / *iw as f32).min(ch as f32 / *ih as f32);
                let dw = (*iw as f32 * fit).max(1.0) as i32;
                let dh = (*ih as f32 * fit).max(1.0) as i32;
                c.draw_image(
                    self.left + pl + (cw - dw) / 2,
                    self.top + pt + (ch - dh) / 2,
                    dw as usize,
                    dh as usize,
                    *iw as usize,
                    *ih as usize,
                    px,
                );
            }
        }
        if let Some(ops) = self.custom_ops.clone() {
            // Guest `onDraw` ops are view-local — translate into the
            // view's absolute origin before replaying. Clipping to the
            // view's bounds is the parent's job (clipChildren).
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
            let (cx, cy) = (x + self.scroll_x, y + self.scroll_y);
            for (i, ch) in self.children.iter_mut().enumerate().rev() {
                if ch.dispatch_touch(true, cx, cy) {
                    self.pressed_child = Some(i);
                    return true;
                }
            }
            press_hit(&mut self.view, x, y)
        } else {
            if let Some(i) = self.pressed_child.take() {
                if let Some(ch) = self.children.get_mut(i) {
                    return ch.dispatch_touch(false, x + self.scroll_x, y + self.scroll_y);
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
        // AOSP order: background → onDraw → children. A group whose
        // dex class overrides onDraw still paints itself first —
        // e.g. ViewPager2's page containers carrying recorded ops.
        if let Some(ops) = self.view.custom_ops.clone() {
            c.save();
            c.translate(self.view.left, self.view.top);
            replay_guest_ops(c, &ops);
            c.restore();
        }
        // Children draw at their own laid-out origins (absolute
        // coords), clipped to their bounds unless the app opted out
        // with clipChildren=false (overflowing menus, FABs, …).
        for ch in &mut self.children {
            if self.clip_children {
                if dbg_flag("OVELO_CLIPDBG") {
                    eprintln!(
                        "[clipdbg] parent clip {},{} {s:?}",
                        self.view.left, self.view.top
                    );
                }
                c.save();
                c.clip_rect(Rect::new(self.view.left, self.view.top, s.w, s.h));
                let (cl, ct) = ch.origin();
                let cs = ch.measured();
                c.clip_rect(Rect::new(cl, ct, cs.w, cs.h));
                c.translate(-self.scroll_x, -self.scroll_y);
                ch.draw(c);
                c.restore();
            } else {
                c.save();
                c.translate(-self.scroll_x, -self.scroll_y);
                ch.draw(c);
                c.restore();
            }
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
    /// AOSP gravity flags (CENTER_HORIZONTAL=1, CENTER_VERTICAL=0x10,
    /// RIGHT/END=5, BOTTOM=0x50). 0 = top|start.
    pub gravity: i32,
    /// `android:maxLines` — draws at most this many wrapped lines.
    pub max_lines: Option<i32>,
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
            gravity: 0,
            max_lines: None,
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
        // `maxLines` truncates to the first n wrapped lines (AOSP
        // ellipsizes the last one — hiding the overflow reads close
        // enough for labels).
        let lines: &[ovelo_ui::Line] = match self.max_lines {
            Some(m) if m > 0 => &t.lines[..t.lines.len().min(m as usize)],
            _ => &t.lines,
        };
        // Vertical gravity: 0x10 center, 0x50 bottom, else top.
        let text_h = lh * lines.len().max(1) as i32;
        let vy = match self.gravity & 0x70 {
            0x10 => self.view.top + (s.h - text_h) / 2,
            0x50 => self.view.top + s.h - self.padding - text_h,
            _ => self.view.top + self.padding,
        };
        let mut baseline = vy + t.ascent;
        for line in lines {
            let seg = &self.text[line.range.0..line.range.1];
            // Horizontal gravity: 1 center, 5 right/end, else left.
            let tx = match self.gravity & 0x7 {
                0x1 => self.view.left + (s.w - line.width) / 2,
                0x5 => self.view.left + s.w - self.padding - line.width,
                _ => self.view.left + self.padding,
            };
            if dbg_flag("OVELO_POSDBG") && !self.text.is_empty() {
                eprintln!(
                    "[posdbg] text {:?} view {},{} {:?} grav={:x} lines={} draw at {},{} size {}",
                    self.text,
                    self.view.left,
                    self.view.top,
                    s,
                    self.gravity,
                    lines.len(),
                    tx,
                    baseline,
                    self.text_size
                );
            }
            c.draw_text(
                seg,
                tx,
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
            Paint::color(self.tv.view.background.unwrap_or(self.bg)),
        );
        // An ImageView-style src (FAB icon, ImageButton) draws centered
        // inside the bounds, fit-center scaled down when larger.
        if let Some((iw, ih, px)) = &self.tv.view.image {
            let fit = ((s.w as f32 / *iw as f32).min(s.h as f32 / *ih as f32)).min(1.0);
            let (dw, dh) = ((*iw as f32 * fit) as i32, (*ih as f32 * fit) as i32);
            let x = self.tv.view.left + (s.w - dw) / 2;
            let y = self.tv.view.top + (s.h - dh) / 2;
            c.draw_image(
                x,
                y,
                dw.max(0) as usize,
                dh.max(0) as usize,
                *iw as usize,
                *ih as usize,
                px,
            );
        }
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

/// Flatten absolute path commands into polylines, one per contour.
/// Cubics use enough subdivisions for icon-sized curves; fill mode
/// implicitly closes open contours during scan conversion.
fn flatten_path(cmds: &[ovelo_dexrt::PathCmd]) -> Vec<Vec<f32>> {
    use ovelo_dexrt::PathCmd as P;
    let mut out = Vec::new();
    let mut pts: Vec<f32> = Vec::new();
    let mut cur = (0.0f32, 0.0f32);
    let mut start = cur;
    let mut open = false;
    fn finish(out: &mut Vec<Vec<f32>>, pts: &mut Vec<f32>, open: &mut bool) {
        if pts.len() >= 4 {
            out.push(std::mem::take(pts));
        } else {
            pts.clear();
        }
        *open = false;
    }
    for cmd in cmds {
        match *cmd {
            P::MoveTo { x, y } => {
                finish(&mut out, &mut pts, &mut open);
                cur = (x, y);
                start = cur;
                pts.extend_from_slice(&[x, y]);
                open = true;
            }
            P::LineTo { x, y } => {
                if !open {
                    pts.extend_from_slice(&[cur.0, cur.1]);
                    start = cur;
                    open = true;
                }
                pts.extend_from_slice(&[x, y]);
                cur = (x, y);
            }
            P::QuadTo { cx, cy, x, y } => {
                if !open {
                    pts.extend_from_slice(&[cur.0, cur.1]);
                    start = cur;
                    open = true;
                }
                let (x0, y0) = cur;
                for i in 1..=12 {
                    let t = i as f32 / 12.0;
                    let u = 1.0 - t;
                    pts.extend_from_slice(&[
                        u * u * x0 + 2.0 * u * t * cx + t * t * x,
                        u * u * y0 + 2.0 * u * t * cy + t * t * y,
                    ]);
                }
                cur = (x, y);
            }
            P::CubicTo {
                cx1,
                cy1,
                cx2,
                cy2,
                x,
                y,
            } => {
                if !open {
                    pts.extend_from_slice(&[cur.0, cur.1]);
                    start = cur;
                    open = true;
                }
                let (x0, y0) = cur;
                for i in 1..=16 {
                    let t = i as f32 / 16.0;
                    let u = 1.0 - t;
                    pts.extend_from_slice(&[
                        u * u * u * x0
                            + 3.0 * u * u * t * cx1
                            + 3.0 * u * t * t * cx2
                            + t * t * t * x,
                        u * u * u * y0
                            + 3.0 * u * u * t * cy1
                            + 3.0 * u * t * t * cy2
                            + t * t * t * y,
                    ]);
                }
                cur = (x, y);
            }
            P::Close => {
                if open && cur != start {
                    pts.extend_from_slice(&[start.0, start.1]);
                    cur = start;
                }
                finish(&mut out, &mut pts, &mut open);
            }
        }
    }
    finish(&mut out, &mut pts, &mut open);
    out
}

/// Replay a guest `onDraw` recording: each [`ovelo_dexrt::CanvasOp`]
/// maps onto the `Canvas` subset (already translated to the view's
/// absolute origin by the caller). Stroke-styled shapes approximate
/// as outlines via `draw_line`; `drawText` honors LEFT/CENTER/RIGHT
/// alignment using the shared font metrics.
fn replay_guest_ops(c: &mut Canvas, ops: &[ovelo_dexrt::CanvasOp]) {
    use ovelo_dexrt::CanvasOp as O;
    let paint = |p: &ovelo_dexrt::PaintState| {
        let argb = p.draw_argb();
        Paint {
            color: Color::from_srgb8(
                (argb >> 16) as u8,
                (argb >> 8) as u8,
                argb as u8,
                (argb >> 24) as u8,
            ),
            text_size: p.text_size.max(1.0),
        }
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
            O::Rect {
                l,
                t,
                r,
                b,
                paint: p,
            } => {
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
            O::Oval {
                l,
                t,
                r,
                b,
                paint: p,
            } => {
                let r = Rect::new(*l as i32, *t as i32, (*r - *l) as i32, (*b - *t) as i32);
                if p.style == 1 {
                    // STROKE — a real ring, not a rect outline.
                    c.stroke_oval(r, p.stroke_w.max(1.0) as i32, paint(p));
                } else {
                    let rad = (r.x1 - r.x).min(r.y1 - r.y) / 2;
                    stroke_or_fill(c, r, p, rad);
                }
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
                if p.style == 1 {
                    c.stroke_oval(rect, p.stroke_w.max(1.0) as i32, paint(p));
                } else {
                    stroke_or_fill(c, rect, p, *r as i32);
                }
            }
            O::Path {
                cmds,
                fill_type,
                paint: p,
            } => {
                let contours = flatten_path(cmds);
                let col = paint(p);
                if p.style != 1 {
                    let contours_i: Vec<Vec<i32>> = contours
                        .iter()
                        .map(|pts| pts.iter().map(|v| v.round() as i32).collect())
                        .collect();
                    c.fill_polys(&contours_i, fill_type % 2 == 1, *fill_type >= 2, col);
                }
                if p.style != 0 {
                    let w = p.stroke_w.max(1.0) as i32;
                    for pts in &contours {
                        for xy in pts.chunks(2).zip(pts.chunks(2).skip(1)) {
                            c.draw_line(
                                xy.0[0] as i32,
                                xy.0[1] as i32,
                                xy.1[0] as i32,
                                xy.1[1] as i32,
                                w,
                                col,
                            );
                        }
                    }
                }
            }
            O::FillPoly { pts, argb } => {
                let ip: Vec<i32> = pts.iter().map(|f| *f as i32).collect();
                c.fill_poly(
                    &ip,
                    Paint {
                        color: Color::from_srgb8(
                            (argb >> 16) as u8,
                            (argb >> 8) as u8,
                            *argb as u8,
                            (argb >> 24) as u8,
                        ),
                        text_size: 12.0,
                    },
                );
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
            O::Text {
                text,
                x,
                y,
                paint: p,
            } => {
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
                paint: p,
            } => {
                let (dw, dh) = if *w == 0 || *h == 0 {
                    (*src_w as usize, *src_h as usize)
                } else {
                    (*w as usize, *h as usize)
                };
                // Compose vector drawables are cached as alpha-mask
                // bitmaps and recolored by a SRC_IN color filter when
                // the mask is drawn. Model that filter by recoloring
                // the source copy before host blitting.
                let tinted = p.tint_argb.map(|tint| {
                    let tr = ((tint >> 16) & 0xff) as u32;
                    let tg = ((tint >> 8) & 0xff) as u32;
                    let tb = (tint & 0xff) as u32;
                    let ta = ((tint >> 24) & 0xff) as u32;
                    let mut out = rgba.as_ref().clone();
                    for px in out.as_chunks_mut::<4>().0 {
                        let sa = px[3] as u32;
                        px[0] = tr as u8;
                        px[1] = tg as u8;
                        px[2] = tb as u8;
                        px[3] = (ta * sa / 255) as u8;
                    }
                    out
                });
                let pixels: &[u8] = tinted.as_deref().unwrap_or(rgba.as_ref());
                if dw > 0 && dh > 0 && !pixels.is_empty() {
                    c.draw_image(
                        *x as i32,
                        *y as i32,
                        dw,
                        dh,
                        *src_w as usize,
                        *src_h as usize,
                        pixels,
                    );
                }
            }
            // References are flattened inside the runtime before ops
            // reach the framework — nothing to replay here.
            O::Nested { .. } => {}
        }
    }
}

/// Cached env flag — per-frame checks must not hit getenv.
fn dbg_flag(name: &'static str) -> bool {
    static CACHE: std::sync::OnceLock<
        std::sync::RwLock<std::collections::HashMap<&'static str, bool>>,
    > = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| std::sync::RwLock::new(std::collections::HashMap::new()));
    if let Some(v) = cache.read().unwrap().get(name) {
        return *v;
    }
    let v = std::env::var_os(name).is_some();
    cache.write().unwrap().insert(name, v);
    v
}

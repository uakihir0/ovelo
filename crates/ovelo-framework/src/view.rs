//! Minimal `View`/`ViewGroup` measure + layout model.
//!
//! Implements Android's `MeasureSpec` protocol (EXACTLY / AT_MOST /
//! UNSPECIFIED) and linear container propagation — enough for the host to run
//! real layout arithmetic while app `onMeasure` overrides stay in DEX.

use crate::widget::Drawable;
use ovelo_ui::Color;

/// A dimension from `LayoutParams`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Length {
    /// `match_parent` (-1).
    MatchParent,
    /// `wrap_content` (-2).
    WrapContent,
    /// An exact pixel size.
    Exact(i32),
}

/// `View.MeasureSpec`: a mode plus a size hint from the parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeasureSpec {
    /// Parent imposes an exact size.
    Exactly(i32),
    /// Child may be at most this size.
    AtMost(i32),
    /// No constraint.
    Unspecified,
}

impl MeasureSpec {
    /// `ViewGroup.getChildMeasureSpec` — resolve a child's spec from the
    /// parent's spec and the child's `LayoutParams` length.
    pub fn for_child(parent: MeasureSpec, lp: Length) -> MeasureSpec {
        match (parent, lp) {
            (_, Length::Exact(d)) => MeasureSpec::Exactly(d.max(0)),
            (MeasureSpec::Exactly(s), Length::MatchParent) => MeasureSpec::Exactly(s),
            (MeasureSpec::Exactly(s), Length::WrapContent) => MeasureSpec::AtMost(s),
            (MeasureSpec::AtMost(s), Length::MatchParent) => MeasureSpec::AtMost(s),
            (MeasureSpec::AtMost(s), Length::WrapContent) => MeasureSpec::AtMost(s),
            (MeasureSpec::Unspecified, _) => MeasureSpec::Unspecified,
        }
    }
}

/// A measured or laid-out size.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Size {
    /// Width in px.
    pub w: i32,
    /// Height in px.
    pub h: i32,
}

/// A leaf view: layout params, measured size, laid-out rect, dirty flag.
pub struct View {
    /// Requested width.
    pub lp_w: Length,
    /// Requested height.
    pub lp_h: Length,
    /// Content's natural size (what `onMeasure` would report for
    /// WRAP_CONTENT / inside AT_MOST caps).
    pub desired: Size,
    /// Result of `measure()`.
    pub measured: Size,
    /// Laid-out rect (left, top).
    pub left: i32,
    /// Laid-out rect (top).
    pub top: i32,
    /// `setBackgroundColor` — drawn under `draw` when set.
    pub background: Option<Color>,
    /// `setOnClickListener` — fired on tap inside the laid-out rect.
    pub on_click: Option<Box<dyn FnMut()>>,
    dirty: bool,
}

impl View {
    /// New view with the given params and content's natural size.
    pub fn new(lp_w: Length, lp_h: Length, desired: Size) -> Self {
        Self {
            lp_w,
            lp_h,
            desired,
            measured: Size::default(),
            left: 0,
            top: 0,
            background: None,
            on_click: None,
            dirty: true,
        }
    }

    /// `View.measure`: resolve `desired` against the specs.
    pub fn measure(&mut self, w: MeasureSpec, h: MeasureSpec) -> Size {
        self.measured = Size {
            w: resolve(self.desired.w, w),
            h: resolve(self.desired.h, h),
        };
        self.measured
    }

    /// `View.layout`: set the rect; marks clean.
    pub fn layout(&mut self, left: i32, top: i32) {
        self.left = left;
        self.top = top;
        self.dirty = false;
    }

    /// `invalidate()` — mark for redraw.
    pub fn invalidate(&mut self) {
        self.dirty = true;
    }

    /// Whether a redraw is pending (`isDirty`).
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }
}

/// `resolveSizeAndState` (no state bits): size under a spec.
fn resolve(desired: i32, spec: MeasureSpec) -> i32 {
    match spec {
        MeasureSpec::Exactly(s) => s,
        MeasureSpec::AtMost(s) => desired.min(s),
        MeasureSpec::Unspecified => desired,
    }
}

/// A vertical linear container (`LinearLayout`, vertical orientation):
/// children stacked top-to-bottom, width = widest child.
pub struct ViewGroup {
    /// The group's own params and measured size.
    pub view: View,
    /// Children in z-order (`addView`).
    pub children: Vec<Box<dyn Drawable>>,
}

impl ViewGroup {
    /// New group with layout params and padding-free contents.
    pub fn new(lp_w: Length, lp_h: Length) -> Self {
        Self {
            view: View::new(lp_w, lp_h, Size::default()),
            children: Vec::new(),
        }
    }

    /// Vertical linear measure: width = max child width, height = sum of
    /// child heights, each resolved against the child spec.
    pub fn measure(&mut self, w: MeasureSpec, h: MeasureSpec) -> Size {
        let mut mw = 0;
        let mut mh = 0;
        for c in &mut self.children {
            let (lp_w, lp_h) = c.lp();
            let cs = c.measure(
                MeasureSpec::for_child(w, lp_w),
                MeasureSpec::for_child(h, lp_h),
            );
            mw = mw.max(cs.w);
            mh += cs.h;
        }
        self.view.desired = Size { w: mw, h: mh };
        self.view.measure(w, h)
    }

    /// Layout children stacked from (left, top).
    pub fn layout(&mut self, left: i32, top: i32) {
        self.view.layout(left, top);
        let mut y = top;
        for c in &mut self.children {
            c.layout(left, y);
            y += c.measured().h;
        }
    }
}

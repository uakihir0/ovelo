//! Minimal `View`/`ViewGroup` measure + layout model.
//!
//! Implements Android's `MeasureSpec` protocol (EXACTLY / AT_MOST /
//! UNSPECIFIED) and linear container propagation — enough for the host to run
//! real layout arithmetic while app `onMeasure` overrides stay in DEX.

use crate::widget::Drawable;
use ovelo_ui::Color;
use std::rc::Rc;

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

impl Length {
    /// Map an AOSP `LayoutParams` dim — `-1` match_parent, `-2`
    /// wrap_content, any other value an exact px size.
    pub fn from_aosp(v: i32) -> Self {
        match v {
            -1 => Length::MatchParent,
            -2 => Length::WrapContent,
            other => Length::Exact(other.max(0)),
        }
    }
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
    /// `setImage*` — decoded pixels (w, h, RGBA8), scaled into the
    /// measured rect at draw.
    pub image: Option<(u32, u32, std::rc::Rc<Vec<u8>>)>,
    /// `setOnClickListener` — fired on tap inside the laid-out rect.
    pub on_click: Option<Box<dyn FnMut()>>,
    /// Guest `onDraw(Canvas)` ops recorded by the dexrt materializer —
    /// replayed view-locally after background/image in `draw`.
    pub custom_ops: Option<Rc<Vec<ovelo_dexrt::CanvasOp>>>,
    /// Unflattened guest ops retained when they contain RenderNode
    /// references; child display lists are re-resolved from these.
    pub raw_custom_ops: Option<Rc<Vec<ovelo_dexrt::CanvasOp>>>,
    /// Guest invalidate sequence at which `raw_custom_ops` was recorded.
    pub raw_custom_ops_seq: i32,
    /// Pressed while the pointer that went down on this view is still
    /// down; a click only fires when the release also lands inside.
    pub pressed: bool,
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
            image: None,
            on_click: None,
            custom_ops: None,
            raw_custom_ops: None,
            raw_custom_ops_seq: -1,
            pressed: false,
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
/// `overlay` flips it to FrameLayout-style: every child at the same
/// origin (RelativeLayout/CoordinatorLayout/DrawerLayout/…).
pub struct ViewGroup {
    /// The group's own params and measured size.
    pub view: View,
    /// Children in z-order (`addView`).
    pub children: Vec<Box<dyn Drawable>>,
    /// Child that consumed the current pointer-down (for routing the
    /// release back to the same pressed view).
    pub pressed_child: Option<usize>,
    /// Overlay (frame) layout instead of vertical stacking.
    pub overlay: bool,
    /// `android:clipChildren` — when true (default) each child draws
    /// clipped to its own bounds; false lets children overflow.
    pub clip_children: bool,
    /// View scroll offsets; child drawing shifts opposite these values.
    pub scroll_x: i32,
    pub scroll_y: i32,
}

impl ViewGroup {
    /// New group with layout params and padding-free contents.
    pub fn new(lp_w: Length, lp_h: Length) -> Self {
        Self {
            view: View::new(lp_w, lp_h, Size::default()),
            children: Vec::new(),
            pressed_child: None,
            overlay: false,
            clip_children: true,
            scroll_x: 0,
            scroll_y: 0,
        }
    }

    /// FrameLayout-style group: children overlay at the same origin.
    pub fn overlay(lp_w: Length, lp_h: Length) -> Self {
        let mut g = Self::new(lp_w, lp_h);
        g.overlay = true;
        g
    }

    /// Vertical linear measure: width = max child width, height = sum of
    /// child heights, each resolved against the child spec. Overlay
    /// measure: height = max child height.
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
            if self.overlay {
                mh = mh.max(cs.h);
            } else {
                mh += cs.h;
            }
        }
        self.view.desired = Size { w: mw, h: mh };
        self.view.measure(w, h)
    }

    /// Layout children stacked from (left, top), or all at (left, top)
    /// for an overlay group.
    pub fn layout(&mut self, left: i32, top: i32) {
        self.view.layout(left, top);
        let mut y = top;
        for c in &mut self.children {
            // A guest-laid-out child carries its own rect relative to
            // this parent — stack offsets would double-position it.
            let (cl, ct) = match c.dex_frame() {
                Some((fl, ft, _, _)) => (left + fl, top + ft),
                None => (left, y),
            };
            c.layout(cl, ct);
            if !self.overlay {
                y += c.measured().h;
            }
        }
    }
}

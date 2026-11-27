//! Guest view bridge — maps DEX-side framework objects (`new Button`,
//! `setText`, `setOnClickListener`, `setContentView`) onto host view
//! objects so a guest `Activity` drives a real window's view tree.
//!
//! Object model: `new Landroid/widget/Button;` allocates a guest heap
//! instance; its `<init>` is a host stub that binds the heap handle to a
//! concrete [`GuestView`]. Methods resolve per-handle through
//! [`GuestUi::views`]. `setOnClickListener` does NOT call into the VM —
//! that would re-enter the interpreter mid-dispatch; it enqueues
//! `(listener, view)` handles the driver pumps via
//! [`GuestUi::drain_clicks`] after touch dispatch returns.

use crate::root::ViewRootImpl;
use crate::view::{Length, MeasureSpec, Size, View, ViewGroup};
use crate::widget::{Button, Drawable, TextView};
use ovelo_dexrt::{HostCall, Vm, V};
use ovelo_error::Result;
use ovelo_ui::{Color, FontSet};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;

/// Click queue: `(listener handle, view handle)` awaiting
/// `OnClickListener.onClick(view)` dispatch on the VM.
pub type ClickQueue = Rc<RefCell<VecDeque<(u32, u32)>>>;

/// A host view object bound to a guest heap handle.
pub enum GuestView {
    /// `android.view.View`.
    Plain(View),
    /// `android.widget.TextView`.
    Text(TextView),
    /// `android.widget.Button`.
    Btn(Button),
    /// `android.view.ViewGroup` / `android.widget.LinearLayout`
    /// (vertical).
    Group(ViewGroup),
}

impl GuestView {
    /// The `View` part shared by every widget.
    fn view(&self) -> &View {
        match self {
            GuestView::Plain(v) => v,
            GuestView::Text(t) => &t.view,
            GuestView::Btn(b) => &b.tv.view,
            GuestView::Group(g) => &g.view,
        }
    }
    /// The `View` part shared by every widget.
    fn view_mut(&mut self) -> &mut View {
        match self {
            GuestView::Plain(v) => v,
            GuestView::Text(t) => &mut t.view,
            GuestView::Btn(b) => &mut b.tv.view,
            GuestView::Group(g) => &mut g.view,
        }
    }
    /// The `TextView` part, when the widget has text.
    fn text_mut(&mut self) -> Option<&mut TextView> {
        match self {
            GuestView::Text(t) => Some(t),
            GuestView::Btn(b) => Some(&mut b.tv),
            _ => None,
        }
    }
}

impl Drawable for GuestView {
    fn measure(&mut self, w: MeasureSpec, h: MeasureSpec) -> Size {
        match self {
            // `v.measure` would resolve the inherent method (desired.min)
            // — the leaf must fill the spec AOSP-style, so call the
            // trait impl explicitly.
            GuestView::Plain(v) => Drawable::measure(v, w, h),
            GuestView::Text(t) => t.measure(w, h),
            GuestView::Btn(b) => b.measure(w, h),
            GuestView::Group(g) => g.measure(w, h),
        }
    }
    fn layout(&mut self, left: i32, top: i32) {
        match self {
            GuestView::Plain(v) => v.layout(left, top),
            GuestView::Text(t) => t.layout(left, top),
            GuestView::Btn(b) => b.layout(left, top),
            GuestView::Group(g) => g.layout(left, top),
        }
    }
    fn draw(&mut self, c: &mut ovelo_ui::Canvas) {
        match self {
            GuestView::Plain(v) => v.draw(c),
            GuestView::Text(t) => t.draw(c),
            GuestView::Btn(b) => b.draw(c),
            GuestView::Group(g) => g.draw(c),
        }
    }
    fn invalidate(&mut self) {
        match self {
            GuestView::Plain(v) => v.invalidate(),
            GuestView::Text(t) => t.invalidate(),
            GuestView::Btn(b) => b.invalidate(),
            GuestView::Group(g) => g.invalidate(),
        }
    }
    fn is_dirty(&self) -> bool {
        match self {
            GuestView::Plain(v) => v.is_dirty(),
            GuestView::Text(t) => t.is_dirty(),
            GuestView::Btn(b) => b.is_dirty(),
            GuestView::Group(g) => g.is_dirty(),
        }
    }
    fn measured(&self) -> Size {
        match self {
            GuestView::Plain(v) => v.measured,
            GuestView::Text(t) => t.measured(),
            GuestView::Btn(b) => b.measured(),
            GuestView::Group(g) => g.measured(),
        }
    }
    fn origin(&self) -> (i32, i32) {
        match self {
            GuestView::Plain(v) => v.origin(),
            GuestView::Text(t) => t.origin(),
            GuestView::Btn(b) => b.origin(),
            GuestView::Group(g) => g.origin(),
        }
    }
    fn lp(&self) -> (Length, Length) {
        match self {
            GuestView::Plain(v) => v.lp(),
            GuestView::Text(t) => t.lp(),
            GuestView::Btn(b) => b.lp(),
            GuestView::Group(g) => g.lp(),
        }
    }
    fn dispatch_touch(&mut self, down: bool, x: i32, y: i32) -> bool {
        match self {
            GuestView::Plain(v) => v.dispatch_touch(down, x, y),
            GuestView::Text(t) => t.dispatch_touch(down, x, y),
            GuestView::Btn(b) => b.dispatch_touch(down, x, y),
            GuestView::Group(g) => g.dispatch_touch(down, x, y),
        }
    }
}

/// Shared handle to a guest view — lets `addView`/`setContentView`
/// attach an object the guest can still mutate through the bridge.
pub struct SharedView(pub Rc<RefCell<GuestView>>);

macro_rules! shared_delegate {
    ($($m:ident($($a:ident : $t:ty),*) -> $r:ty),* $(,)?) => {$(
        fn $m(&mut self, $($a: $t),*) -> $r {
            self.0.borrow_mut().$m($($a),*)
        }
    )*};
}

impl Drawable for SharedView {
    shared_delegate!(
        measure(w: MeasureSpec, h: MeasureSpec) -> Size,
        layout(left: i32, top: i32) -> (),
        invalidate() -> (),
        dispatch_touch(down: bool, x: i32, y: i32) -> bool,
    );
    fn draw(&mut self, c: &mut ovelo_ui::Canvas) {
        self.0.borrow_mut().draw(c);
    }
    fn is_dirty(&self) -> bool {
        self.0.borrow().is_dirty()
    }
    fn measured(&self) -> Size {
        self.0.borrow().measured()
    }
    fn origin(&self) -> (i32, i32) {
        self.0.borrow().origin()
    }
    fn lp(&self) -> (Length, Length) {
        self.0.borrow().lp()
    }
    fn as_any(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

/// Wraps a materialized child whose dex `layout`/`setFrame` assigned a
/// real rect: `dex_frame()` tells the parent where it goes (relative
/// to the parent's own origin), and its reported size is the dex frame.
struct AbsView {
    inner: SharedView,
    /// Dex frame relative to the parent `(l, t, r, b)`.
    rect: (i32, i32, i32, i32),
}

impl AbsView {
    fn frame_size(&self) -> Size {
        Size {
            w: self.rect.2 - self.rect.0,
            h: self.rect.3 - self.rect.1,
        }
    }
}

impl Drawable for AbsView {
    fn measure(&mut self, w: MeasureSpec, h: MeasureSpec) -> Size {
        let s = self.frame_size();
        let _ = self.inner.measure(w, h);
        // The dex rect wins — draw and hit tests read the inner
        // view's measured size, so it must agree with the frame.
        self.inner.0.borrow_mut().view_mut().measured = s;
        s
    }
    fn layout(&mut self, left: i32, top: i32) {
        if dbg_flag("OVELO_POSDBG") {
            eprintln!(
                "[posdbg] AbsView layout left={left} top={top} rect={:?}",
                self.rect
            );
        }
        self.inner.layout(left, top);
    }
    fn draw(&mut self, c: &mut ovelo_ui::Canvas) {
        self.inner.draw(c);
    }
    fn invalidate(&mut self) {
        self.inner.invalidate();
    }
    fn is_dirty(&self) -> bool {
        self.inner.is_dirty()
    }
    fn measured(&self) -> Size {
        self.frame_size()
    }
    fn origin(&self) -> (i32, i32) {
        self.inner.origin()
    }
    fn lp(&self) -> (Length, Length) {
        self.inner.lp()
    }
    fn dex_frame(&self) -> Option<(i32, i32, i32, i32)> {
        Some(self.rect)
    }
    fn dispatch_touch(&mut self, down: bool, x: i32, y: i32) -> bool {
        self.inner.dispatch_touch(down, x, y)
    }
    fn as_any(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

/// Same-Rc check for a boxed child vs a dex handle's bound view —
/// matches `SharedView` and `AbsView`-wrapped SharedViews; used by the
/// removeView bridge and addView dedupe.
fn child_matches(b: &mut Box<dyn Drawable>, cv: &Rc<RefCell<GuestView>>) -> bool {
    let Some(a) = b.as_any() else { return false };
    if let Some(sv) = a.downcast_ref::<SharedView>() {
        return Rc::ptr_eq(&sv.0, cv);
    }
    if let Some(av) = a.downcast_ref::<AbsView>() {
        return Rc::ptr_eq(&av.inner.0, cv);
    }
    false
}

/// Host-side state for guest UI: the view-object map plus the
/// [`ViewRootImpl`] the content view hangs off.
pub struct GuestUi {
    /// Window root (`perform_traversals`/`dispatch_touch` driver).
    pub root: ViewRootImpl,
    /// Guest heap handle → bound host view.
    views: HashMap<u32, Rc<RefCell<GuestView>>>,
    /// Fonts for text widgets (and for host-built fallback trees).
    pub fonts: Rc<FontSet>,
    /// Deferred onClick dispatches.
    clicks: ClickQueue,
    /// View handle → registered OnClickListener handle — the closure
    /// captures it opaquely, so GC needs this to keep listeners alive.
    click_listeners: HashMap<u32, u32>,
    /// Dex-view-tree handles → materialized host view. Persistent so
    /// per-view state (pressed, listeners) survives frame rebuilds.
    dex_views: HashMap<u32, Rc<RefCell<GuestView>>>,
    /// Handles that belonged to the last materialized tree — the GL
    /// frame pump must not drive renderers of detached views.
    dex_live: HashSet<u32>,
    /// Last materialized child list per parent — (handle, dex frame)
    /// pairs. A changed set (removal, hide) or a late-arriving frame
    /// must dirty the group or stale pixels / misplaced children
    /// persist.
    dex_kids: HashMap<u32, Vec<KidEntry>>,
    /// The dex view-tree content root last materialized.
    dex_root: u32,
    /// The host view currently mounted as content — remount only when
    /// it differs (a bridge view may replace the materialized one).
    dex_mounted: Option<Rc<RefCell<GuestView>>>,
    /// `vm.view_gen` the mounted tree was materialized against —
    /// re-materialize only when the view store actually changed.
    dex_gen_seen: u64,
    /// Dex handles that already received `onAttachedToWindow` — the
    /// callback fires once per attach, like ViewRootImpl's
    /// dispatchAttachedToWindow (fossify views start their data loads
    /// there; without it adapters never populate).
    dex_attached: HashSet<u32>,
    /// Handles awaiting their first `onAttachedToWindow` dispatch —
    /// collected during materialize (the GuestUi borrow is held) and
    /// fired by the frame loop once the borrow is released, since the
    /// callback runs guest code that re-enters bridged host calls.
    attach_pending: Vec<u32>,
    /// True once the guest set a real `GuestView` content view — the
    /// dex-tree renderer must not clobber it.
    guest_content_set: bool,
}

fn argb(c: i32) -> Color {
    Color::from_srgb8((c >> 16) as u8, (c >> 8) as u8, c as u8, (c >> 24) as u8)
}

fn ref_arg(args: &[V], i: usize) -> Result<u32> {
    match args.get(i) {
        Some(V::Ref(h)) if *h != 0 => Ok(*h),
        _ => Err(crate::err("guest.arg.null")),
    }
}

// ---------------------------------------------------------------------
// Dex view-tree materialization — real apps inflate layout xml into the
// VM's view-tree model; these turn each node into a drawable host view.

/// One materialized child: its dex handle plus the `setFrame` rect seen
/// at materialize time (None until dex layout runs — tracking it lets a
/// late frame flip the child into `AbsView` and re-layout).
type KidEntry = (u32, Option<(i32, i32, i32, i32)>);

fn dex_len(l: i32) -> Length {
    match l {
        -1 => Length::MatchParent,
        -2 => Length::WrapContent,
        px => Length::Exact(px),
    }
}

fn make_dex_view(vm: &Vm, info: &ovelo_dexrt::DexViewInfo, fonts: &Rc<FontSet>) -> GuestView {
    let lp = (dex_len(info.lp_w), dex_len(info.lp_h));
    let dark = Color::srgb(0x20, 0x20, 0x20);
    if info.is_group {
        // FrameLayout-style containers (Frame/Relative/Coordinator/
        // DrawerLayout, ConstraintLayout, decor frames…) lay children
        // on top of each other — stacking them vertically pushes every
        // match_parent sibling offscreen and renders a blank window.
        const STACK: &[&str] = &[
            "LinearLayout",
            "LinearLayoutCompat",
            "TabLayout",
            "RecyclerView",
            "ListView",
            "GridView",
            "AdapterView",
            "ScrollView",
            "MenuView",
            "Toolbar",
            "RadioGroup",
            "TableLayout",
        ];
        // Ancestry, not name substring — a custom `MyLayout extends
        // LinearLayout` stacks its children even when its own name
        // doesn't say so.
        let mut g = if STACK.iter().any(|s| vm.extends_named(&info.desc, s)) {
            ViewGroup::new(lp.0, lp.1)
        } else {
            ViewGroup::overlay(lp.0, lp.1)
        };
        g.clip_children = info.clip_children;
        g.scroll_x = info.scroll_x;
        g.scroll_y = info.scroll_y;
        g.horizontal = !info.linear_vertical;
        GuestView::Group(g)
    } else if info.desc.contains("Button") {
        GuestView::Btn(Button::new("", 16.0, dark, fonts.clone()))
    } else if info.text.is_some()
        || info.desc.contains("TextView")
        || info.desc.contains("EditText")
        // Name checks miss TextView descendants whose names don't
        // contain it (`MyTextClock` → TextClock → TextView): a view
        // that gains text later must already be a Text widget.
        || vm.extends_named(&info.desc, "TextView")
    {
        GuestView::Text(TextView::new("", 16.0, dark, fonts.clone()))
    } else {
        GuestView::Plain(View::new(lp.0, lp.1, Size::default()))
    }
}

fn update_dex_view(g: &mut GuestView, info: &ovelo_dexrt::DexViewInfo) {
    let mut changed = false;
    {
        let v = g.view_mut();
        let (w, h) = (dex_len(info.lp_w), dex_len(info.lp_h));
        if v.lp_w != w || v.lp_h != h {
            v.lp_w = w;
            v.lp_h = h;
            changed = true;
        }
        let bg = info.bg_color.map(argb);
        if v.background != bg {
            v.background = bg;
            changed = true;
        }
        let img_changed = match (&v.image, &info.image) {
            (None, None) => false,
            (Some((w, h, px)), Some((w2, h2, px2))) => {
                w != w2 || h != h2 || !std::rc::Rc::ptr_eq(px, px2)
            }
            _ => true,
        };
        if img_changed {
            v.image = info.image.clone();
            changed = true;
        }
    }
    if let GuestView::Group(gr) = g {
        if gr.clip_children != info.clip_children {
            gr.clip_children = info.clip_children;
            gr.invalidate();
        }
        if gr.horizontal != !info.linear_vertical {
            gr.horizontal = !info.linear_vertical;
            gr.invalidate();
        }
        if gr.scroll_x != info.scroll_x || gr.scroll_y != info.scroll_y {
            gr.scroll_x = info.scroll_x;
            gr.scroll_y = info.scroll_y;
            gr.invalidate();
        }
    }
    if let Some(tv) = g.text_mut() {
        if let Some(t) = &info.text {
            if tv.text != *t {
                tv.set_text(t.clone());
            }
        }
        if let Some(s) = info.text_size {
            if tv.text_size != s {
                tv.text_size = s;
                tv.set_text(tv.text.clone()); // re-measure at the new size
            }
        }
        if let Some(c) = info.text_color {
            let col = argb(c);
            if tv.color != col {
                tv.color = col;
                tv.invalidate();
            }
        }
        if tv.gravity != info.gravity {
            tv.gravity = info.gravity;
            tv.invalidate();
        }
        if tv.max_lines != info.max_lines {
            tv.max_lines = info.max_lines;
            tv.invalidate();
        }
    }
    if changed {
        g.invalidate();
    }
}

impl GuestUi {
    /// New bridge: empty view map, root at `(w, h)` sharing `fonts`.
    pub fn new(w: i32, h: i32, fonts: Rc<FontSet>) -> Rc<RefCell<Self>> {
        Rc::new(RefCell::new(Self {
            root: ViewRootImpl::new(w, h, fonts.clone()),
            views: HashMap::new(),
            fonts,
            clicks: Rc::new(RefCell::new(VecDeque::new())),
            click_listeners: HashMap::new(),
            dex_views: HashMap::new(),
            dex_live: HashSet::new(),
            dex_kids: HashMap::new(),
            dex_root: 0,
            dex_mounted: None,
            dex_gen_seen: u64::MAX,
            dex_attached: HashSet::new(),
            attach_pending: Vec::new(),
            guest_content_set: false,
        }))
    }

    /// `(listener, view)` pairs queued by click listeners — pump them
    /// through `Vm::call_virtual(l, "onClick", &[Ref(v)])` after touch
    /// dispatch (not inside it, which would re-enter the VM).
    pub fn drain_clicks(&self) -> Vec<(u32, u32)> {
        self.clicks.borrow_mut().drain(..).collect()
    }

    /// Dex handles awaiting `onAttachedToWindow`, collected during
    /// materialize. The caller dispatches them via
    /// `vm.call_virtual(h, "onAttachedToWindow", &[])` after releasing
    /// the GuestUi borrow — the callback re-enters bridged host calls.
    pub fn take_attach_pending(&mut self) -> Vec<u32> {
        std::mem::take(&mut self.attach_pending)
    }

    /// Every guest heap handle the UI keeps alive — GC roots for
    /// `Vm::collect_garbage`: bound views, materialized dex views and
    /// their child lists, the dex root, and queued click targets.
    pub fn guest_handles(&self) -> Vec<u32> {
        let mut v: Vec<u32> = self.views.keys().copied().collect();
        v.extend(self.dex_views.keys().copied());
        v.extend(self.dex_kids.keys().copied());
        v.extend(
            self.dex_kids
                .values()
                .flat_map(|kids| kids.iter().map(|(h, _)| *h)),
        );
        v.push(self.dex_root);
        v.extend(self.clicks.borrow().iter().flat_map(|(a, b)| [*a, *b]));
        v.extend(self.click_listeners.keys().copied());
        v.extend(self.click_listeners.values().copied());
        v
    }

    /// The host view bound to a guest heap handle.
    pub fn view(&self, h: u32) -> Option<Rc<RefCell<GuestView>>> {
        self.views.get(&h).cloned()
    }

    /// Register every bridged framework method on `vm`. Unreferenced
    /// methods simply don't register (the app never calls them).
    pub fn register(ui: &Rc<RefCell<Self>>, vm: &mut Vm) {
        // Compose's InlineClassHelperKt.throwIllegalArgumentException is
        // the require() inside DrawScope.inset — PainterNode can hand a
        // painter an Unspecified/negative size (degenerate layout while
        // the composition is still settling), and letting it throw
        // aborts the ENTIRE dispatchDraw traversal, leaving a black
        // screen. A silent return lets the frame finish; the offending
        // painter just draws nothing.
        let _ = vm.register_host(
            "Landroidx/compose/ui/graphics/InlineClassHelperKt;",
            "throwIllegalArgumentException",
            "(Ljava/lang/String;)V",
            Rc::new(RefCell::new(|_vm: &mut Vm, _a: &[V]| Ok(None))) as HostCall,
        );
        // Fragment host callbacks are only partially modeled; once a
        // fragment transaction reaches onGetLayoutInflater, returning
        // null makes every fragment onCreateView fault before it can
        // inflate even though the activity's inflater works.
        let fragment_inflater_registered = vm.register_host(
            "Landroidx/fragment/app/Fragment;",
            "onGetLayoutInflater",
            "(Landroid/os/Bundle;)Landroid/view/LayoutInflater;",
            Rc::new(RefCell::new(|vm: &mut Vm, a: &[V]| {
                let fragment = match a.first() {
                    Some(V::Ref(h)) => *h,
                    _ => 0,
                };
                if fragment != 0 {
                    let real = vm.call_exact_direct(
                        fragment,
                        "Landroidx/fragment/app/Fragment;",
                        "onGetLayoutInflater",
                        "(Landroid/os/Bundle;)Landroid/view/LayoutInflater;",
                        a.get(1).copied().as_slice(),
                    )?;
                    if !matches!(real, Some(V::Ref(0)) | None) {
                        return Ok(real);
                    }
                }
                let activity = vm.current_activity();
                let ctx = (activity != 0).then_some(V::Ref(activity));
                Ok(Some(vm.layout_inflater(ctx)))
            })) as HostCall,
        );
        if std::env::var_os("OVELO_VIEWDBG").is_some() {
            eprintln!("[viewdbg] fragment inflater host={fragment_inflater_registered:?}");
        }
        // The cooperative VM can deliver a frame callback while a Compose
        // apply pass is still unwinding host view operations. Modern Compose
        // treats the resulting pending-modification drain as a fatal internal
        // error; on Android's strictly serialized UI thread that ordering does
        // not occur. Keep the drained state and continue instead of tearing
        // down the whole composition for a scheduling artifact.
        let _ = vm.register_host(
            "Landroidx/compose/runtime/ComposerKt;",
            "composeImmediateRuntimeError",
            "(Ljava/lang/String;)V",
            Rc::new(RefCell::new(|_vm: &mut Vm, _a: &[V]| Ok(None))) as HostCall,
        );
        // ---- constructors -------------------------------------------------
        Self::ctor(ui, vm, "Landroid/view/View;", |_ui| {
            GuestView::Plain(View::new(
                Length::WrapContent,
                Length::WrapContent,
                Size::default(),
            ))
        });
        Self::ctor(ui, vm, "Landroid/widget/TextView;", |ui| {
            GuestView::Text(TextView::new(
                "",
                16.0,
                Color::srgb(0x20, 0x20, 0x20),
                ui.fonts.clone(),
            ))
        });
        Self::ctor(ui, vm, "Landroid/widget/Button;", |ui| {
            GuestView::Btn(Button::new(
                "",
                16.0,
                Color::srgb(0x20, 0x20, 0x20),
                ui.fonts.clone(),
            ))
        });
        for cls in ["Landroid/view/ViewGroup;", "Landroid/widget/LinearLayout;"] {
            Self::ctor(ui, vm, cls, |_ui| {
                GuestView::Group(ViewGroup::new(Length::MatchParent, Length::WrapContent))
            });
        }
        // Stacking groups — children overlap rather than flow.
        for cls in [
            "Landroid/widget/FrameLayout;",
            "Landroid/widget/RelativeLayout;",
            "Landroid/widget/AbsoluteLayout;",
            "Landroid/widget/ScrollView;",
        ] {
            Self::ctor(ui, vm, cls, |_ui| {
                GuestView::Group(ViewGroup::overlay(Length::MatchParent, Length::MatchParent))
            });
        }

        // ---- ViewGroup.addView / removeView -------------------------------
        // `addViewInLayout` shares the bridge: AOSP semantics differ
        // only in skipping measure/layout, which our tree re-runs each
        // pump anyway. Apps inflate children through it directly
        // (fossify documents' ViewPager did) — an unregistered call
        // attached children parent_att=0 and they never drew.
        for cls in ["Landroid/view/ViewGroup;", "Landroid/widget/LinearLayout;"] {
            for (name, sig) in [
                ("addView", "(Landroid/view/View;)V"),
                ("addView", "(Landroid/view/View;I)V"),
                (
                    "addView",
                    "(Landroid/view/View;ILandroid/view/ViewGroup$LayoutParams;)V",
                ),
                (
                    "addView",
                    "(Landroid/view/View;Landroid/view/ViewGroup$LayoutParams;)V",
                ),
                (
                    "addView",
                    "(Landroid/view/View;ILandroid/view/ViewGroup$LayoutParams;Z)V",
                ),
                ("addViewInLayout", "(Landroid/view/View;)V"),
                ("addViewInLayout", "(Landroid/view/View;I)V"),
                (
                    "addViewInLayout",
                    "(Landroid/view/View;ILandroid/view/ViewGroup$LayoutParams;)V",
                ),
                (
                    "addViewInLayout",
                    "(Landroid/view/View;Landroid/view/ViewGroup$LayoutParams;)V",
                ),
                (
                    "addViewInLayout",
                    "(Landroid/view/View;ILandroid/view/ViewGroup$LayoutParams;Z)V",
                ),
                // Some androidx/internal helpers declare the boolean
                // overload with a boolean return.
                (
                    "addView",
                    "(Landroid/view/View;ILandroid/view/ViewGroup$LayoutParams;Z)Z",
                ),
                (
                    "addViewInLayout",
                    "(Landroid/view/View;ILandroid/view/ViewGroup$LayoutParams;Z)Z",
                ),
                // RecyclerView attaches every visible row through
                // attachViewToParent — bridge it or bridged groups
                // render empty lists. Transient scrap views that
                // also pass through are re-removed by
                // detachViewFromParent (registered below).
                (
                    "attachViewToParent",
                    "(Landroid/view/View;ILandroid/view/ViewGroup$LayoutParams;)V",
                ),
            ] {
                Self::on_rc(ui, vm, cls, name, sig, move |ui, vm, a| {
                    let this = ref_arg(a, 0)?;
                    let child = ref_arg(a, 1)?;
                    // Boolean-returning overloads must produce a value
                    // — `Ok(None)` is void and aborts the caller with
                    // dexrt.result.void.
                    let z_ret = if sig.ends_with(")Z") {
                        Some(V::I32(1))
                    } else {
                        None
                    };
                    // Overloads differ after the child arg: LayoutParams is
                    // the last Ref; the index is the first non-negative I32
                    // (the dex arm parses them the same way).
                    let lp = a[2.min(a.len())..]
                        .iter()
                        .rev()
                        .find(|x| matches!(x, V::Ref(h) if *h != 0))
                        .copied();
                    // The index is the int right after the child — except
                    // the (View,w,h) dimensions overload, which has two
                    // ints and no LayoutParams. The (V,I,LP,Z) overload's
                    // trailing boolean must not count as a second int.
                    let ints = a[2.min(a.len())..]
                        .iter()
                        .filter(|x| matches!(x, V::I32(_)))
                        .count();
                    let idx = if lp.is_none() && ints >= 2 {
                        None
                    } else {
                        match a.get(2) {
                            Some(V::I32(i)) if *i >= 0 => Some(*i as usize),
                            _ => None,
                        }
                    };
                    // Dex-side bookkeeping first: registered overloads
                    // intercept the invoke, so `attach_at` never runs
                    // unless the bridge does it — parent/attach mark,
                    // LP resolution and onViewAdded all live there.
                    // Runs outside the ui borrow: onViewAdded is guest
                    // code and re-enters bridged methods.
                    // A retried bridge call skips dex bookkeeping the
                    // builtin fallback already did for this invocation:
                    // re-running would re-fire onViewAdded and
                    // re-append an indexless child, moving it past
                    // siblings added meanwhile. Fresh calls (including
                    // legit re-attaches that move the child) always run.
                    if !(vm.host_retrying() && vm.view_parent_of(child) == Some(this)) {
                        vm.view_tree_attach_at(
                            this,
                            child,
                            idx,
                            lp,
                            !matches!(name, "attachViewToParent" | "attachViewToParentInLayout"),
                        );
                    }
                    // Bridged parents adopt dex-view-tree children too —
                    // materialize them so e.g. ViewPager2's empty
                    // FrameLayout still hosts inflated content.
                    // Look up then materialize in two statements — the
                    // `or_else` closure would run `borrow_mut` while the
                    // `borrow` temporary is still live and panic.
                    let mut cv = ui.borrow().views.get(&child).cloned();
                    if cv.is_none() {
                        cv = ui.borrow_mut().materialize_dex(vm, child);
                    }
                    let Some(cv) = cv else {
                        return Ok(z_ret);
                    };
                    // Dex-materialized parents live in `dex_views`,
                    // bridged `new` parents in `views` — a parent of
                    // either kind must accept the child or rows added
                    // to a dex group (RecyclerView's attachViewToParent)
                    // orphan the whole subtree.
                    let Some(rc) = ui
                        .borrow()
                        .views
                        .get(&this)
                        .cloned()
                        .or_else(|| ui.borrow().dex_views.get(&this).cloned())
                    else {
                        return Ok(z_ret);
                    };
                    let mut g = rc.borrow_mut();
                    let GuestView::Group(gr) = &mut *g else {
                        return Ok(z_ret);
                    };
                    if let Some(V::Ref(lp)) = lp {
                        let (w, h) = vm.layout_params_dims(lp);
                        let mut c = cv.borrow_mut();
                        let v = c.view_mut();
                        if let Some(w) = w {
                            v.lp_w = Length::from_aosp(w);
                        }
                        if let Some(h) = h {
                            v.lp_h = Length::from_aosp(h);
                        }
                        drop(c);
                    }
                    // Dedupe a re-added child (move, not duplicate) — the
                    // dex side does the same in attach_at.
                    if let Some(old) = gr.children.iter_mut().position(|b| child_matches(b, &cv)) {
                        gr.children.remove(old);
                    }
                    let boxed: Box<dyn Drawable> = match vm.dex_view(child).and_then(|i| i.frame) {
                        Some((l, t, r, b)) => Box::new(AbsView {
                            inner: SharedView(cv),
                            rect: (l, t, r, b),
                        }),
                        None => Box::new(SharedView(cv)),
                    };
                    // Place at the dex child index when dex bookkeeping
                    // already ran (fresh attach_at or a retried call the
                    // builtin fallback handled): siblings added between
                    // the original call and the retry would otherwise
                    // shift this child past its dex position.
                    match vm.view_child_index(this, child).or(idx) {
                        Some(i) if i <= gr.children.len() => gr.children.insert(i, boxed),
                        _ => gr.children.push(boxed),
                    }
                    gr.view.invalidate();
                    Ok(z_ret)
                });
            }
            for (name, notify, sigs) in [
                ("removeView", true, vec!["(Landroid/view/View;)V", "(I)V"]),
                ("removeViewInLayout", true, vec!["(Landroid/view/View;)V"]),
                ("removeViewAt", true, vec!["(I)V"]),
                (
                    "detachViewFromParent",
                    false,
                    vec!["(Landroid/view/View;)V", "(I)V"],
                ),
            ] {
                for sig in sigs {
                    Self::on_rc(ui, vm, cls, name, sig, move |ui, vm, a| {
                        let this = ref_arg(a, 0)?;
                        // Child by ref arg, else by index into the dex
                        // child list (it still reflects the pre-remove
                        // state — the dex bookkeeping runs below).
                        let child = match ref_arg(a, 1) {
                            Ok(c) => Some(c),
                            Err(_) => match a.get(1).copied() {
                                Some(V::I32(i)) if i >= 0 => vm
                                    .dex_view(this)
                                    .and_then(|i0| i0.children.get(i as usize).copied()),
                                _ => None,
                            },
                        };
                        let Some(child) = child else {
                            return Ok(None);
                        };
                        // Dex bookkeeping: registered overloads intercept
                        // the invoke, so the bridge must run detach —
                        // outside the ui borrow since onViewRemoved is
                        // guest code re-entering bridged methods.
                        vm.view_tree_detach(this, child, notify);
                        let ui = ui.borrow();
                        let Some(cv) = ui
                            .views
                            .get(&child)
                            .cloned()
                            .or_else(|| ui.dex_views.get(&child).cloned())
                        else {
                            return Ok(None);
                        };
                        let Some(rc) = ui
                            .views
                            .get(&this)
                            .cloned()
                            .or_else(|| ui.dex_views.get(&this).cloned())
                        else {
                            return Ok(None);
                        };
                        let mut g = rc.borrow_mut();
                        let GuestView::Group(gr) = &mut *g else {
                            return Ok(None);
                        };
                        if let Some(i) = gr.children.iter_mut().position(|b| child_matches(b, &cv))
                        {
                            gr.children.remove(i);
                            gr.view.invalidate();
                        }
                        Ok(None)
                    });
                }
            }
            Self::on_rc(ui, vm, cls, "removeAllViews", "()V", |ui, vm, a| {
                let this = ref_arg(a, 0)?;
                for c in vm
                    .dex_view(this)
                    .map(|i| i.children.clone())
                    .unwrap_or_default()
                {
                    vm.view_tree_detach(this, c, true);
                }
                if let Some(rc) = ui.borrow().views.get(&this).cloned() {
                    if let GuestView::Group(gr) = &mut *rc.borrow_mut() {
                        gr.children.clear();
                        gr.view.invalidate();
                    }
                }
                Ok(None)
            });
        }

        // ---- Activity.setContentView -------------------------------------
        Self::on_rc(
            ui,
            vm,
            "Landroid/app/Activity;",
            "setContentView",
            "(Landroid/view/View;)V",
            |ui, vm, a| {
                let v = ref_arg(a, 1)?;
                let activity = ref_arg(a, 0)?;
                // Dex-side bookkeeping (attach, content-frame registration)
                // runs for bridged views too — the GL attach gate and the
                // dex view tree both need it. Outside the ui borrow:
                // inflation and onViewAdded are guest code.
                vm.view_set_content(activity, v);
                let mut ui = ui.borrow_mut();
                if let Some(cv) = ui.views.get(&v).cloned() {
                    ui.guest_content_set = true;
                    if viewdbg() {
                        eprintln!("[viewdbg] guest_content_set via bridged h{v}");
                    }
                    ui.root.set_content_view(Box::new(SharedView(cv)));
                }
                Ok(None)
            },
        );

        // ---- shared View setters ------------------------------------------
        for cls in [
            "Landroid/view/View;",
            "Landroid/view/ViewGroup;",
            "Landroid/widget/TextView;",
            "Landroid/widget/Button;",
            "Landroid/widget/LinearLayout;",
        ] {
            Self::on(ui, vm, cls, "setBackgroundColor", "(I)V", |ui, vm, a| {
                let this = ref_arg(a, 0)?;
                let raw = match a.get(1) {
                    Some(V::I32(c)) => *c,
                    _ => return Err(crate::err("guest.arg.type")),
                };
                let Some(rc) = ui.views.get(&this).cloned() else {
                    // Dex-inflated view — record so dex_view_info sees it.
                    ovelo_dexrt::pseudo_set(vm, this, ovelo_dexrt::P_BGCOLOR, V::I32(raw));
                    return Ok(None);
                };
                let color = argb(raw);
                rc.borrow_mut().view_mut().background = Some(color);
                rc.borrow_mut().view_mut().invalidate();
                Ok(None)
            });
            Self::on(
                ui,
                vm,
                cls,
                "setOnClickListener",
                "(Landroid/view/View$OnClickListener;)V",
                |ui, vm, a| {
                    let this = ref_arg(a, 0)?;
                    // A null listener clears the click handler; a
                    // const/4'd null can arrive as I32(0).
                    let listener = match a.get(1) {
                        Some(V::Ref(h)) => *h,
                        Some(V::I32(0)) => 0,
                        _ => return Err(crate::err("guest.arg.type")),
                    };
                    let q = ui.clicks.clone();
                    // Dex-materialized views read the listener from VM
                    // pseudo state; host widgets additionally queue clicks
                    // through their host on_click closure.
                    vm.set_dex_click_listener(this, V::Ref(listener));
                    let Some(rc) = ui.views.get(&this).cloned() else {
                        return Ok(None);
                    };
                    if listener == 0 {
                        ui.click_listeners.remove(&this);
                    } else {
                        ui.click_listeners.insert(this, listener);
                    }
                    rc.borrow_mut().view_mut().on_click = if listener == 0 {
                        None
                    } else {
                        Some(Box::new(move || {
                            q.borrow_mut().push_back((listener, this));
                        }))
                    };
                    Ok(None)
                },
            );
        }

        // ---- TextView setters ---------------------------------------------
        for cls in ["Landroid/widget/TextView;", "Landroid/widget/Button;"] {
            Self::on(
                ui,
                vm,
                cls,
                "setText",
                "(Ljava/lang/CharSequence;)V",
                |ui, vm, a| {
                    let this = ref_arg(a, 0)?;
                    let Some(rc) = ui.views.get(&this).cloned() else {
                        // Dex-inflated (non-GuestView) receiver: record the
                        // argument on the pseudo text slot so the inflated-
                        // tree renderer still sees the string.
                        ovelo_dexrt::pseudo_set(
                            vm,
                            this,
                            ovelo_dexrt::P_TEXT,
                            a.get(1).copied().unwrap_or(V::Ref(0)),
                        );
                        ovelo_dexrt::mark_layout_requested(vm, this);
                        return Ok(None);
                    };
                    let text = match a.get(1) {
                        Some(V::Ref(h)) => vm.heap().str_val(*h).unwrap_or("").to_string(),
                        Some(V::I32(0)) => String::new(),
                        _ => return Err(crate::err("guest.arg.type")),
                    };
                    let mut g = rc.borrow_mut();
                    let tv = g
                        .text_mut()
                        .ok_or_else(|| crate::err("guest.setText.not_text"))?;
                    tv.set_text(text);
                    ovelo_dexrt::mark_layout_requested(vm, this);
                    Ok(None)
                },
            );
            Self::on(ui, vm, cls, "setTextSize", "(F)V", |ui, vm, a| {
                let this = ref_arg(a, 0)?;
                // A float literal arrives as V::I32 (const loads the
                // bit pattern); a computed float is V::F32.
                let size = match a.get(1) {
                    Some(V::F32(s)) => *s,
                    Some(V::I32(b)) => f32::from_bits(*b as u32),
                    _ => return Err(crate::err("guest.arg.type")),
                };
                // setTextSize uses scaled pixels; DisplayMetrics exposes
                // scaledDensity=2 in this runtime.
                let size = size * 2.0;
                ovelo_dexrt::pseudo_set(vm, this, ovelo_dexrt::P_TSIZE, V::F32(size));
                let Some(rc) = ui.views.get(&this).cloned() else {
                    ovelo_dexrt::mark_layout_requested(vm, this);
                    return Ok(None);
                };
                let mut g = rc.borrow_mut();
                let tv = g
                    .text_mut()
                    .ok_or_else(|| crate::err("guest.setTextSize.not_text"))?;
                tv.text_size = size;
                tv.set_text(tv.text.clone());
                ovelo_dexrt::mark_layout_requested(vm, this);
                Ok(None)
            });
            Self::on_rc(ui, vm, cls, "setTextColor", "(I)V", |ui, vm, a| {
                let this = ref_arg(a, 0)?;
                let raw = match a.get(1) {
                    Some(V::I32(c)) => *c,
                    _ => return Err(crate::err("guest.arg.type")),
                };
                // Runtime colors win over XML inflation colors during
                // later sync passes, and remain authoritative even when
                // this setter is re-entered while the UI is borrowed
                // for drawing.
                ovelo_dexrt::pseudo_set(vm, this, ovelo_dexrt::P_TCOLOR, V::I32(raw));
                let Some(rc) = ui
                    .try_borrow()
                    .ok()
                    .and_then(|ui| ui.views.get(&this).cloned())
                else {
                    return Ok(None);
                };
                if let Ok(mut g) = rc.try_borrow_mut() {
                    let tv = g
                        .text_mut()
                        .ok_or_else(|| crate::err("guest.setTextColor.not_text"))?;
                    tv.color = argb(raw);
                    tv.invalidate();
                }
                Ok(None)
            });
            Self::on_rc(
                ui,
                vm,
                cls,
                "setTextColor",
                "(Landroid/content/res/ColorStateList;)V",
                |ui, vm, a| {
                    let this = ref_arg(a, 0)?;
                    let raw = match a.get(1) {
                        Some(V::Ref(h)) => match vm.color_state_list_resting(*h) {
                            Some(V::I32(c)) => c,
                            _ => 0,
                        },
                        Some(V::I32(c)) => *c,
                        _ => return Err(crate::err("guest.arg.type")),
                    };
                    ovelo_dexrt::pseudo_set(vm, this, ovelo_dexrt::P_TCOLOR, V::I32(raw));
                    let Some(rc) = ui
                        .try_borrow()
                        .ok()
                        .and_then(|ui| ui.views.get(&this).cloned())
                    else {
                        return Ok(None);
                    };
                    if let Ok(mut g) = rc.try_borrow_mut() {
                        let tv = g
                            .text_mut()
                            .ok_or_else(|| crate::err("guest.setTextColor.not_text"))?;
                        tv.color = argb(raw);
                        tv.invalidate();
                    }
                    Ok(None)
                },
            );
        }
    }

    /// Materialize the Activity's dex view tree (from `setContentView(res)`
    /// inflation) into real host views and hang it under the root — the
    /// renderer for apps that build UI from layout xml rather than
    /// bridged `new` calls. Per-view objects persist across calls so
    /// pressed state and listener closures survive rebuilds. Skipped
    /// once the guest bound a real `GuestView` as its content view.
    pub fn refresh_dex_tree(&mut self, vm: &mut Vm, activity: u32) -> bool {
        // A bound GLSurfaceView.Renderer draws every frame in
        // continuous mode — its output isn't triggered by view-tree
        // changes, so pump it every tick: for materialized dex views
        // AND for bridged `new` views (guest_content_set apps like
        // 2048 whose whole UI is a bridged GLSurfaceView).
        vm.sync_decor_frames(self.root.w, self.root.h);
        self.pump_gl_frames(vm);
        if self.guest_content_set {
            return false;
        }
        let Some(root) = vm.activity_content(activity) else {
            if viewdbg() {
                eprintln!("[viewdbg] refresh: no content for activity={activity}");
            }
            return false;
        };
        // Tree unchanged since the mounted materialize — skip the
        // per-node info build (the per-frame dominant cost).
        if self.dex_root == root && self.dex_mounted.is_some() && self.dex_gen_seen == vm.view_gen {
            return true;
        }
        self.dex_live.clear();
        let Some(rc) = self.materialize_dex(vm, root) else {
            // Content root went GONE/INVISIBLE: swap in an empty group
            // so the stale tree stops drawing and answering touches.
            if self.dex_root != 0 {
                self.dex_root = 0;
                self.dex_mounted = None;
                self.dex_gen_seen = u64::MAX;
                self.dex_attached.clear();
                self.root.set_content_view(Box::new(ViewGroup::new(
                    Length::MatchParent,
                    Length::MatchParent,
                )));
            }
            return false;
        };
        // Mount once per root: per-view objects persist across calls,
        // so the same SharedView stays attached and only changes
        // (update_dex_view) mark the tree dirty — a static screen
        // doesn't re-measure/re-draw every frame.
        let mounted = self.dex_root == root
            && self
                .dex_mounted
                .as_ref()
                .is_some_and(|m| Rc::ptr_eq(m, &rc));
        if !mounted {
            self.dex_root = root;
            self.dex_mounted = Some(rc.clone());
            if viewdbg() {
                let kids = match &*rc.borrow() {
                    GuestView::Group(g) => g.children.len(),
                    _ => 0,
                };
                eprintln!(
                    "[viewdbg] mount root h{root} kids={kids} gcs={}",
                    self.guest_content_set
                );
            }
            self.root.set_content_view(Box::new(SharedView(rc)));
        }
        // The window's background comes from the theme's
        // `windowBackground` attribute — app views only paint their own
        // rect, so a dark themed window must show through everywhere a
        // view doesn't cover. Applied on each pass; a theme switch
        // (fossify's color settings) re-tints without a remount.
        // `windowBackground` may point at a platform drawable the APK
        // doesn't carry — fall back through the app's own color attrs.
        let win_bg = ovelo_dexrt::theme_color(vm, 0x0101_0054)
            .or_else(|| ovelo_dexrt::theme_color_named(vm, "colorSurface"))
            .or_else(|| ovelo_dexrt::theme_color_named(vm, "colorBackground"))
            .or_else(|| ovelo_dexrt::theme_color_named(vm, "colorPrimaryDark"));
        if let Some(argb) = win_bg {
            self.root.bg = Color::srgb((argb >> 16) as u8, (argb >> 8) as u8, argb as u8);
        }
        self.dex_gen_seen = vm.view_gen;
        true
    }

    /// Run `onDrawFrame` for every view that has a bound GL renderer —
    /// continuous renderers produce a new frame independent of
    /// view-tree generation changes. Covers both the materialized dex
    /// tree (`dex_views`) and bridged `new` views (`views`).
    fn pump_gl_frames(&mut self, vm: &mut Vm) {
        // Push host-computed bounds back onto the dex view objects:
        // `getWidth()`/`getHeight()` read the dex-side frame, which the
        // host layout engine never ran `layout()` to set (Korge's GL
        // view builds its ortho matrix from these — 0x0 NaNs it).
        // Host layout coords are window-absolute; dex frames are
        // parent-relative (the hit test accumulates ancestors), so
        // subtract the parent's absolute origin.
        for (&h, rc) in self.views.iter() {
            let (l, t, s) = {
                let g = rc.borrow();
                let v = g.view();
                (v.left, v.top, v.measured)
            };
            if s.w > 0 && s.h > 0 && !vm.view_layout_done(h) {
                let (pl, pt) = abs_origin(vm, &self.views, vm.dex_parent(h));
                vm.view_set_frame(h, l - pl, t - pt, l - pl + s.w, t - pt + s.h);
            }
        }
        if dbg_flag("OVELO_DRAWDBG") {
            eprintln!(
                "[gldbg] pump views={:?} dex_views={:?}",
                self.views.keys().collect::<Vec<_>>(),
                self.dex_views.keys().collect::<Vec<_>>()
            );
        }
        for (&h, rc) in self.views.iter().chain(self.dex_views.iter()) {
            if dbg_flag("OVELO_DRAWDBG") {
                eprintln!(
                    "[gldbg] cand h={h} gl={} att={} cls={} parent={:?} measured={:?}",
                    vm.has_gl_renderer(h),
                    vm.view_is_attached(h),
                    vm.dbg_class_of(h),
                    vm.dbg_view_parent(h),
                    rc.borrow_mut().view_mut().measured
                );
            }
            let compose_view = vm.dbg_class_of(h).contains("/compose/ui/platform/");
            if !vm.has_gl_renderer(h) && !compose_view {
                continue;
            }
            let (w, hh) = if self.dex_views.contains_key(&h) {
                // Materialized node: must still be mounted, and its
                // size comes from the dex-side frame.
                if !self.dex_live.contains(&h) {
                    continue;
                }
                let Some((l, t, r, b)) = vm.dex_view(h).and_then(|i| i.frame) else {
                    continue;
                };
                (r - l, b - t)
            } else {
                // Bridged view: must still be attached to the dex tree —
                // a removed SurfaceView (e.g. after a second
                // setContentView) must stop pumping its renderer, or it
                // keeps drawing over the new content view.
                if !vm.view_is_attached(h) {
                    continue;
                }
                // Laid out by the host tree — use its measured size
                // (0 before the first traversal).
                let s = rc.borrow_mut().view_mut().measured;
                (s.w, s.h)
            };
            if w <= 0 || hh <= 0 {
                continue;
            }
            let mut captured_raw = None;
            let mut captured_seq = -1;
            let ops = if compose_view {
                let (existing, existing_seq) = {
                    let g = rc.borrow();
                    (g.view().raw_custom_ops.clone(), g.view().raw_custom_ops_seq)
                };
                let valid = existing.is_some() && existing_seq == vm.view_invalid_seq(h);
                let Some(raw) = (if valid { existing } else { None }).or_else(|| {
                    let captured = vm.guest_draw_ops_raw(h, w, hh);
                    (!captured.is_empty()).then(|| {
                        let raw = Rc::new(captured);
                        captured_seq = vm.view_invalid_seq(h);
                        captured_raw = Some((raw.clone(), captured_seq));
                        raw
                    })
                }) else {
                    continue;
                };
                vm.expand_display_list_ops(&raw)
            } else {
                vm.guest_draw_ops(h, w, hh)
            };
            if std::env::var_os("OVELO_OPSDBG").is_some() && !ops.is_empty() {
                use std::collections::BTreeMap;
                let mut hist: BTreeMap<&'static str, usize> = BTreeMap::new();
                let mut texts = 0usize;
                for o in &ops {
                    *hist
                        .entry(match o {
                            ovelo_dexrt::CanvasOp::Text { .. } => {
                                texts += 1;
                                "Text"
                            }
                            ovelo_dexrt::CanvasOp::Bitmap { .. } => "Bitmap",
                            ovelo_dexrt::CanvasOp::Rect { .. } => "Rect",
                            ovelo_dexrt::CanvasOp::Circle { .. } => "Circle",
                            ovelo_dexrt::CanvasOp::Line { .. } => "Line",
                            _ => "other",
                        })
                        .or_default() += 1;
                }
                let details: Vec<String> = ops
                    .iter()
                    .filter_map(|o| match o {
                        ovelo_dexrt::CanvasOp::Text { text, x, y, .. } => {
                            Some(format!("T({x:.0},{y:.0},{text})"))
                        }
                        ovelo_dexrt::CanvasOp::Rect { l, t, r, b, .. } => {
                            Some(format!("R({l:.0},{t:.0},{r:.0},{b:.0})"))
                        }
                        _ => None,
                    })
                    .take(12)
                    .collect();
                eprintln!(
                    "[opsdbg] h{h} {w}x{hh} ops={} hist={hist:?} texts={texts} {details:?}",
                    ops.len()
                );
            }
            let mut g = rc.borrow_mut();
            let v = g.view_mut();
            if let Some((raw, seq)) = captured_raw {
                v.raw_custom_ops = Some(raw);
                v.raw_custom_ops_seq = seq;
            }
            v.custom_ops = if ops.is_empty() {
                None
            } else {
                Some(Rc::new(ops))
            };
            // New GL content must mark the view dirty or the next
            // traversal skips the draw entirely.
            v.invalidate();
        }
    }

    fn materialize_dex(&mut self, vm: &mut Vm, h: u32) -> Option<Rc<RefCell<GuestView>>> {
        self.dex_live.insert(h);
        let mut info = vm.dex_view(h)?;
        if viewdbg() {
            eprintln!(
                "[viewdbg] node {} h{h} vis={} kids={} text={:?} frame={:?} tc={:?} bg={:?} lp={}x{}",
                info.desc,
                info.visible as u8,
                info.children.len(),
                info.text,
                info.frame,
                info.text_color,
                info.bg_color,
                info.lp_w,
                info.lp_h,
            );
        }
        // First attach to the window: AOSP's dispatchAttachedToWindow
        // fires for every view in the hierarchy — including GONE/
        // INVISIBLE ones (visibility is orthogonal to attachment), so
        // a hidden view that loads data on attach and then reveals
        // itself still gets its init. Queued: guest code re-enters
        // bridged calls that borrow GuestUi, which materialize holds.
        // Skip views addView already notified (P_ATTACHED): dispatching
        // attach again double-runs onAttachedToWindow — AbstractComposeView
        // would register its observer twice.
        if self.dex_attached.insert(h) && !vm.view_is_attached(h) {
            self.attach_pending.push(h);
        }
        if !info.visible {
            return None;
        }
        let fonts = self.fonts.clone();
        // Reuse the bridge-bound instance when the inflated widget's
        // ctor already made one — the bridge setters (`setText` etc.)
        // update `views`, and clicks/text must reach the drawn view.
        let rc = match self.views.get(&h) {
            Some(rc) => rc.clone(),
            None => self
                .dex_views
                .entry(h)
                .or_insert_with(|| Rc::new(RefCell::new(make_dex_view(vm, &info, &fonts))))
                .clone(),
        };
        // Guest `onDraw`: a dex view class that overrides onDraw paints
        // through Canvas — record its ops for the host draw pass. Done
        // before `rc.borrow_mut` since it runs guest code.
        let (ow, oh) = match info.frame {
            Some((l, t, r, b)) => (r - l, b - t),
            None => (0, 0),
        };
        // Capture the guest's onDraw only at a real size: a 0×0
        // canvas makes size-derived paint degenerate (every text at
        // x=0 — an overlapping pile at the parent origin), so keep
        // the last good ops until a frame lands.
        vm.dump_node_tree(h);
        let capture = ow > 0 && oh > 0 && !vm.compose_measure_active(h);
        let ops = capture.then(|| {
            let raw = vm.guest_draw_ops_raw(h, ow, oh);
            (raw.clone(), vm.expand_display_list_ops(&raw))
        });
        // Guest onDraw may itself call setters such as setTextColor.
        // `info` was snapshotted before that guest code ran; refresh it
        // or this materialize writes the pre-draw color back and the
        // runtime setter is effectively lost.
        if capture {
            if let Some(fresh) = vm.dex_view(h) {
                info = fresh;
            }
        }
        let mut g = rc.borrow_mut();
        // The widget kind is chosen once, but `is_group` can arrive
        // late: ViewPager2 builds its page container FrameLayout
        // empty and adds the page inside onMeasure/populate, so a
        // view materialized as Plain before children existed must be
        // rebuilt as a Group or its kids stay dropped forever.
        if info.is_group && !matches!(&*g, GuestView::Group(_)) {
            *g = make_dex_view(vm, &info, &fonts);
        }
        // `g` borrows the view object while `self.materialize_dex`
        // recurses into children — different cells, no aliasing.
        update_dex_view(&mut g, &info);
        if let Some((raw_ops, ops)) = ops {
            g.view_mut().raw_custom_ops = if raw_ops.is_empty() {
                None
            } else {
                Some(Rc::new(raw_ops))
            };
            g.view_mut().raw_custom_ops_seq = vm.view_invalid_seq(h);
            g.view_mut().custom_ops = if ops.is_empty() {
                None
            } else {
                Some(Rc::new(ops))
            };
            // Ops captured after the last paint (a dex view's first
            // onDraw runs during materialize, mid-traversal) must mark
            // the view dirty or the frame that carries them never
            // repaints — the window stays at its clear color.
            g.view_mut().invalidate();
        }
        if let GuestView::Group(gr) = &mut *g {
            let mut kids: Vec<Box<dyn Drawable>> = Vec::new();
            let mut kid_handles: Vec<KidEntry> = Vec::new();
            for ch in &info.children {
                if let Some(crc) = self.materialize_dex(vm, *ch) {
                    // Dex frames are relative to the parent — position
                    // the child exactly where its `setFrame` put it.
                    let frame = vm.dex_view(*ch).and_then(|i| i.frame);
                    // The frame is part of the key: dex `onLayout`
                    // often runs after the first materialize, and a
                    // late-arriving frame must re-wrap the child into
                    // AbsView AND re-layout — tracking only the handle
                    // leaves the stale pile drawn forever.
                    kid_handles.push((*ch, frame));
                    let view: Box<dyn Drawable> = match frame {
                        Some(rect) => {
                            if viewdbg() {
                                eprintln!("[viewdbg] frame h{ch} = {rect:?}");
                            }
                            Box::new(AbsView {
                                inner: SharedView(crc),
                                rect,
                            })
                        }
                        None => Box::new(SharedView(crc)),
                    };
                    kids.push(view);
                }
            }
            // A removed/hidden child leaves stale pixels without an
            // invalidation — the swap below only re-renders when the
            // child set actually changed.
            if self.dex_kids.get(&h) != Some(&kid_handles) {
                self.dex_kids.insert(h, kid_handles);
                gr.invalidate();
            }
            gr.children = kids;
        } else if !info.children.is_empty() && viewdbg() {
            eprintln!(
                "[viewdbg] KIDS DROPPED h{h} {} kids={} not-a-group",
                info.desc,
                info.children.len()
            );
        }
        if info.listener != 0 {
            let listener = info.listener;
            self.click_listeners.insert(h, listener);
            let q = self.clicks.clone();
            g.view_mut().on_click = Some(Box::new(move || {
                q.borrow_mut().push_back((listener, h));
            }));
        }
        drop(g);
        Some(rc)
    }

    /// `<init>` host stub: bind the receiver handle to a fresh view of
    /// the constructor's widget kind — unless the handle is already
    /// bound (a subclass `super.<init>` must not downgrade it).
    fn ctor(
        ui: &Rc<RefCell<Self>>,
        vm: &mut Vm,
        class: &str,
        make: impl Fn(&GuestUi) -> GuestView + 'static,
    ) {
        let ui2 = ui.clone();
        let _ = vm.register_host(
            class,
            "<init>",
            "(Landroid/content/Context;)V",
            Rc::new(RefCell::new(
                move |_vm: &mut Vm, a: &[V]| -> Result<Option<V>> {
                    let this = ref_arg(a, 0)?;
                    let Ok(mut g) = ui2.try_borrow_mut() else {
                        // Guest draw/measure callbacks can construct views
                        // while the UI bridge is already borrowed. The dex
                        // tree materialization path can recreate the host
                        // object later; crashing the process here loses the
                        // whole frame instead.
                        return Ok(None);
                    };
                    // A subclass super-call can fire a LESS specific ctor
                    // first (`View.<init>` binding Plain before
                    // `ViewGroup.<init>` would bind Group) — Plain is the
                    // weakest kind, so a later specific ctor upgrades it.
                    let upgrade = g
                        .views
                        .get(&this)
                        .map(|rc| matches!(*rc.borrow(), GuestView::Plain(_)))
                        .unwrap_or(false);
                    if upgrade || !g.views.contains_key(&this) {
                        let gv = make(&g);
                        g.views.insert(this, Rc::new(RefCell::new(gv)));
                    }
                    Ok(None)
                },
            )) as HostCall,
        );
    }

    /// `on` variant for handlers that dispatch guest code (attach and
    /// detach bookkeeping invokes dex listeners like `onViewAdded`,
    /// `setContentView` can inflate): the closure gets the `Rc` and
    /// borrows only around view mutation, so guest code re-entering a
    /// bridged method can't double-borrow the `GuestUi` RefCell.
    fn on_rc(
        ui: &Rc<RefCell<Self>>,
        vm: &mut Vm,
        class: &str,
        name: &str,
        proto: &str,
        mut f: impl FnMut(&Rc<RefCell<Self>>, &mut Vm, &[V]) -> Result<Option<V>> + 'static,
    ) {
        let ui2 = ui.clone();
        let class2 = class.to_string();
        let name2 = name.to_string();
        let _ = vm.register_host(
            class,
            name,
            proto,
            Rc::new(RefCell::new(move |vm2: &mut Vm, a: &[V]| {
                f(&ui2, vm2, a).map_err(|e| e.with_detail(format!("{class2}.{name2}")))
            })) as HostCall,
        );
    }

    /// Register `class.name(proto)` to a `&mut GuestUi`-taking handler.
    fn on(
        ui: &Rc<RefCell<Self>>,
        vm: &mut Vm,
        class: &str,
        name: &str,
        proto: &str,
        mut f: impl FnMut(&mut GuestUi, &mut Vm, &[V]) -> Result<Option<V>> + 'static,
    ) {
        let ui2 = ui.clone();
        let class2 = class.to_string();
        let name2 = name.to_string();
        let _ = vm.register_host(
            class,
            name,
            proto,
            Rc::new(RefCell::new(move |vm2: &mut Vm, a: &[V]| {
                let mut g = ui2.borrow_mut();
                f(&mut g, vm2, a).map_err(|e| e.with_detail(format!("{class2}.{name2}")))
            })) as HostCall,
        );
    }
}

/// Drive the dex `onMeasure`/`onLayout` of every guest view group —
/// real Android runs these on the whole tree during layout, and
/// adapter containers (`ViewPager`, `RecyclerView`, …) only create
/// their child pages from inside them (`ViewPager.onMeasure` calls
/// `populate()`). No host caller invokes these methods, so without
/// this pass the containers render empty (the adapter is never asked
/// for its pages). Obfuscation renames the androidx classes, so
/// ancestry checks can't spot them — run it on every group instead.
/// Runs OUTSIDE the `GuestUi` borrow: the guest code it dispatches
/// re-enters bridged view methods that borrow the same RefCell.
///
fn viewdbg() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| std::env::var_os("OVELO_VIEWDBG").is_some())
}

/// Cached env flag — per-frame/per-view checks must not hit getenv.
fn dbg_flag(name: &'static str) -> bool {
    static CACHE: std::sync::OnceLock<std::sync::RwLock<HashMap<&'static str, bool>>> =
        std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| std::sync::RwLock::new(HashMap::new()));
    if let Some(v) = cache.read().unwrap().get(name) {
        return *v;
    }
    let v = std::env::var_os(name).is_some();
    cache.write().unwrap().insert(name, v);
    v
}

/// Absolute (window) origin of a view handle: a bridged view answers
/// with its host layout coords; a dex-only ancestor contributes its
/// own parent-relative frame and the walk continues upward.
fn abs_origin(vm: &Vm, views: &HashMap<u32, Rc<RefCell<GuestView>>>, mut cur: u32) -> (i32, i32) {
    let mut acc = (0, 0);
    for _ in 0..64 {
        if cur == 0 {
            return acc;
        }
        if let Some(rc) = views.get(&cur) {
            let g = rc.borrow();
            let v = g.view();
            return (acc.0 + v.left, acc.1 + v.top);
        }
        let Some(info) = vm.dex_view(cur) else {
            return acc;
        };
        if let Some((l, t, _, _)) = info.frame {
            acc.0 += l;
            acc.1 += t;
        }
        cur = vm.dex_parent(cur);
    }
    acc
}

/// `frame` is a render-cycle counter — the pass runs every 30th frame
/// to bound interpreter work. `populate()` is idempotent on existing
/// pages, and adapters filling asynchronously get picked up on later
/// passes.
/// `win_w`/`win_h` — the real window size the unframed root measures
/// against: a hardcoded size lays out the content wider than the
/// buffer and clips the right/bottom columns offscreen.
pub fn drive_dex_layout_pass(vm: &mut Vm, activity: u32, frame: u32, win_w: i32, win_h: i32) {
    if !frame.is_multiple_of(30) {
        return;
    }
    let Some(root) = vm.activity_content(activity) else {
        return;
    };
    // MeasureSpec mode EXACTLY (mode << 30) — containers like
    // DrawerLayout throw unless both specs are EXACTLY, so
    // UNSPECIFIED/0 would leave them unmeasured forever.
    const MS_EXACTLY: i32 = 1 << 30;
    fn walk(vm: &mut Vm, h: u32, depth: u32, win_w: i32, win_h: i32, drove: &mut bool) {
        if depth > 64 {
            return;
        }
        let Some(info) = vm.dex_view(h) else {
            if viewdbg() {
                eprintln!("[viewdbg] walk h{h} not a dex view");
            }
            return;
        };
        if viewdbg() {
            eprintln!("[viewdbg] walk h{h} {} group={}", info.desc, info.is_group);
        }
        // Drive the real traversal: `measure`/`layout` (not bare
        // `onMeasure`/`onLayout`) — `View.layout` runs `setFrame`,
        // which is the only place `mLeft`/`mTop`/`mRight`/`mBottom`
        // get stored. Containers that size themselves in layout
        // (RecyclerView's `fill` reads `OrientationHelper` totals)
        // see a 0×0 rect and lay out nothing otherwise.
        // Drive measure/layout only at dex-subtree roots — a dex
        // parent's own traversal already measures and lays out its
        // children through normal invoke dispatch, so driving every
        // node re-ran each subtree's solver once per ancestor
        // (quadratic on real trees: minutes of interpreter time).
        // Keep the view's existing rect when it has one: re-driving
        // `layout` with a full-screen rect would overwrite
        // parent-computed bounds.
        // The walk root always drives: the activity's content view
        // reports a dex parent (the `android.R.id.content` frame the
        // decor wraps it in), so `!parent_is_dex` alone would skip
        // every node and no frame would ever be set.
        let parent_is_dex = depth != 0 && info.parent != 0 && vm.is_dex_view(info.parent);
        let root_size_changed = depth == 0
            && info
                .frame
                .is_some_and(|f| (f.2 - f.0, f.3 - f.1) != (win_w, win_h));
        let needs_layout = root_size_changed || info.frame.is_none() || info.layout_requested;
        if viewdbg() && !parent_is_dex {
            eprintln!(
                "[viewdbg] layout? h{h} {} need={} frame={:?} req={} rootchg={}",
                info.desc, needs_layout, info.frame, info.layout_requested, root_size_changed
            );
        }
        // Drive subtree roots regardless of group-ness: a leaf custom
        // view at the XML root has no dex parent to measure/layout it,
        // so without this its frame stays unset and `onDraw` ops never
        // capture (blank screen). Once a stable tree reports no layout
        // request, leave its dex-computed frames alone instead of
        // re-running the whole solver twice a second.
        if !parent_is_dex && needs_layout {
            *drove = true;
            // The root tracks the window: after a resize its stored
            // frame is stale, so re-measure at the new extent — the
            // stored frame only wins for deeper subtree roots whose
            // size genuinely came from a prior pass.
            let (w, hgt) = if depth == 0 {
                (win_w, win_h)
            } else {
                info.frame
                    .map(|f| (f.2 - f.0, f.3 - f.1))
                    .unwrap_or((win_w, win_h))
            };
            if let Err(e) = vm.call_virtual(
                h,
                "measure",
                &[V::I32(MS_EXACTLY | w), V::I32(MS_EXACTLY | hgt)],
            ) {
                if viewdbg() {
                    eprintln!("[viewdbg] measure {} err {e}", info.desc);
                }
            }
            let (l, t) = info.frame.map(|f| (f.0, f.1)).unwrap_or((0, 0));
            if let Err(e) = vm.call_virtual(
                h,
                "layout",
                &[V::I32(l), V::I32(t), V::I32(l + w), V::I32(t + hgt)],
            ) {
                if viewdbg() {
                    eprintln!("[viewdbg] layout {} err {e}", info.desc);
                }
            }
            if viewdbg() && info.desc.contains("ViewPager") {
                let fields: Vec<String> = match vm.heap().get(h) {
                    Some(ovelo_dexrt::Obj::Instance { fields, .. })
                    | Some(ovelo_dexrt::Obj::Host { fields, .. }) => fields
                        .values()
                        .filter_map(|v| match v {
                            V::Ref(x) if *x != 0 => {
                                let mut s = format!("h{x}:{}", vm.dbg_class_of(*x));
                                // One level deeper: the adapter's own
                                // ref fields + host list lengths.
                                if let Some(ovelo_dexrt::Obj::Instance { fields: f2, .. }) =
                                    vm.heap().get(*x)
                                {
                                    let inner: Vec<String> = f2
                                        .values()
                                        .filter_map(|v2| match v2 {
                                            V::Ref(y) if *y != 0 => {
                                                let mut t = format!("h{y}:{}", vm.dbg_class_of(*y));
                                                if let Some(ovelo_dexrt::Obj::Host {
                                                    state: ovelo_dexrt::HostState::List(l),
                                                    ..
                                                }) = vm.heap().get(*y)
                                                {
                                                    t.push_str(&format!("(len={})", l.len()));
                                                }
                                                Some(t)
                                            }
                                            _ => None,
                                        })
                                        .collect();
                                    s.push_str(&format!("->{inner:?}"));
                                }
                                Some(s)
                            }
                            _ => None,
                        })
                        .collect(),
                    _ => Vec::new(),
                };
                eprintln!("[viewdbg] pager {h} ref-fields: {fields:?}");
            }
            if viewdbg() && info.desc.contains("RecyclerView") {
                match vm.call_virtual(h, "getAdapter", &[]) {
                    Ok(Some(V::Ref(ad))) if ad != 0 => {
                        let n = vm
                            .call_virtual(ad, "getItemCount", &[])
                            .ok()
                            .flatten()
                            .map(|v| format!("{v:?}"))
                            .unwrap_or("?".into());
                        eprintln!("[viewdbg] recycler {h} adapter=h{ad} count={n}");
                        let afields: Vec<String> = match vm.heap().get(ad) {
                            Some(ovelo_dexrt::Obj::Instance { fields, .. }) => fields
                                .values()
                                .filter_map(|v| match v {
                                    V::Ref(y) if *y != 0 => {
                                        let mut t = format!("h{y}:{}", vm.dbg_class_of(*y));
                                        if let Some(ovelo_dexrt::Obj::Host {
                                            state: ovelo_dexrt::HostState::List(l),
                                            ..
                                        }) = vm.heap().get(*y)
                                        {
                                            t.push_str(&format!("(len={})", l.len()));
                                        }
                                        Some(t)
                                    }
                                    _ => None,
                                })
                                .collect(),
                            _ => Vec::new(),
                        };
                        eprintln!("[viewdbg] adapter {ad} fields: {afields:?}");
                    }
                    other => eprintln!("[viewdbg] recycler {h} getAdapter -> {other:?}"),
                }
                let fields: Vec<String> = match vm.heap().get(h) {
                    Some(ovelo_dexrt::Obj::Instance { fields, .. })
                    | Some(ovelo_dexrt::Obj::Host { fields, .. }) => fields
                        .iter()
                        .map(|(k, v)| {
                            let vs = match v {
                                V::Ref(x) => format!("h{x}"),
                                V::I32(x) => format!("{x}"),
                                other => format!("{other:?}"),
                            };
                            format!("{}={vs}", vm.dbg_field_name(*k))
                        })
                        .collect(),
                    _ => Vec::new(),
                };
                eprintln!("[viewdbg] recycler {h} fields: {fields:?}");
            }
        }
        // Re-snapshot: the layout pass may have attached children.
        let kids = vm.dex_view(h).map(|i| i.children).unwrap_or_default();
        for ch in kids {
            walk(vm, ch, depth + 1, win_w, win_h, drove);
        }
    }
    let mut drove = false;
    walk(vm, root, 0, win_w, win_h, &mut drove);
    // Android fires OnGlobalLayoutListener after an actual layout pass.
    // Firing it for a skipped clean-tree walk just re-enters guest code
    // every frame; adapter fills still get the real pass that lays them out.
    ovelo_dexrt::fire_pre_draw(vm, root);
    if drove {
        ovelo_dexrt::fire_global_layout(vm, root);
    }
    vm.layout_gen_done.set(vm.view_gen);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ovelo_dex::testdex::{build, C, M};

    fn cls(name: &'static str, methods: Vec<M>) -> C {
        C {
            ext: false,
            class: name,
            sup: None,
            ifaces: &[],
            sfields: &[],
            ifields: &[],
            methods,
        }
    }
    /// A framework-stub class: contributes method_ids/types, no class_def.
    fn ext(name: &'static str, methods: Vec<M>) -> C {
        C {
            ext: true,
            ..cls(name, methods)
        }
    }
    fn virt(name: &'static str, params: &'static [&'static str], insns: Vec<u16>) -> M {
        M {
            name,
            ret: "V",
            params,
            is_virtual: true,
            regs: 6,
            ins: 0,
            outs: 2,
            insns,
            tries: vec![],
            lits: &[],
        }
    }
    fn dir(name: &'static str, params: &'static [&'static str], insns: Vec<u16>) -> M {
        M {
            is_virtual: false,
            ..virt(name, params, insns)
        }
    }
    fn fonts() -> Rc<FontSet> {
        let mut fs = FontSet::new();
        fs.push(Box::new(
            ovelo_ui::TtfFont::from_bytes(include_bytes!("../tests/geneva.ttf"))
                .expect("font parses"),
        ));
        Rc::new(fs)
    }

    /// Patch every placeholder u16 `from` with `to` (sentinel-based — no
    /// positional guessing like the dexrt tests).
    fn patch(b: &mut [u8], from: u16, to: u16) {
        for i in (0..b.len() - 1).step_by(2) {
            if u16::from_le_bytes([b[i], b[i + 1]]) == from {
                b[i..i + 2].copy_from_slice(&to.to_le_bytes());
            }
        }
    }

    // invoke-virtual {vC..vG},m: [0x6e|count<<12|G<<8, midx, F|E<<4|D<<8|C]
    // — C is the first arg reg, then D,E,F nibbles.
    // Sentinels: type 0xEExx, method 0xEDxx, string 0xE5xx.
    const T_BTN: u16 = 0xEE01;
    const T_LN: u16 = 0xEE02;
    const M_BINIT: u16 = 0xED01;
    const M_OINIT: u16 = 0xED02;
    const M_BTEXT: u16 = 0xED03;
    const M_BCLICK: u16 = 0xED04;
    const M_ACV: u16 = 0xED05;
    const M_TTEXT: u16 = 0xED06;
    const M_BSIZE: u16 = 0xED07;
    const M_SCV2: u16 = 0xED08;
    const M_DINIT: u16 = 0xED09;
    const M_DCV: u16 = 0xED0A;
    const T_VIEW: u16 = 0xEE03;
    const T_DLG: u16 = 0xEE04;
    const S_HELLO: u16 = 0xE501;
    const S_CLICKED: u16 = 0xE502;

    /// DEX: guest `LAct` Activity builds a Button in onCreate; `LL`
    /// implements OnClickListener and calls `view.setText("Clicked")`.
    fn guest_dex() -> Vec<u8> {
        let stubs = vec![
            ext("Landroid/content/Context;", vec![]),
            ext("Landroid/view/View;", vec![]),
            ext("Landroid/view/View$OnClickListener;", vec![]),
            ext("Ljava/lang/CharSequence;", vec![]),
            ext("Ljava/lang/Object;", vec![dir("<init>", &[], vec![0x000e])]),
            ext(
                "Landroid/app/Activity;",
                vec![virt(
                    "setContentView",
                    &["Landroid/view/View;"],
                    vec![0x000e],
                )],
            ),
            // A subclass descriptor naming the method — the DEX may
            // reference setContentView through it instead of Activity.
            ext(
                "LSubAct;",
                vec![virt(
                    "setContentView",
                    &["Landroid/view/View;"],
                    vec![0x000e],
                )],
            ),
            // An UNRELATED class with the same name+sig — its call must
            // not reach Activity's host stub.
            ext(
                "Landroid/app/Dialog;",
                vec![
                    dir("<init>", &["Landroid/content/Context;"], vec![0x000e]),
                    virt("setContentView", &["Landroid/view/View;"], vec![0x000e]),
                ],
            ),
            ext(
                "Landroid/widget/TextView;",
                vec![
                    dir("<init>", &["Landroid/content/Context;"], vec![0x000e]),
                    virt("setText", &["Ljava/lang/CharSequence;"], vec![0x000e]),
                    virt("setTextSize", &["F"], vec![0x000e]),
                ],
            ),
            ext(
                "Landroid/widget/Button;",
                vec![
                    dir("<init>", &["Landroid/content/Context;"], vec![0x000e]),
                    virt("setText", &["Ljava/lang/CharSequence;"], vec![0x000e]),
                    virt("setTextSize", &["F"], vec![0x000e]),
                    virt(
                        "setOnClickListener",
                        &["Landroid/view/View$OnClickListener;"],
                        vec![0x000e],
                    ),
                ],
            ),
        ];
        // LL.onClick(View): this=v2, view=v3. setText(v3,"Clicked").
        let listener = C {
            ifaces: &["Landroid/view/View$OnClickListener;"],
            ..cls(
                "LL;",
                vec![M {
                    name: "onClick",
                    lits: &["Clicked"],
                    ins: 2,
                    regs: 4,
                    ..virt(
                        "onClick",
                        &["Landroid/view/View;"],
                        vec![
                            0x001a, S_CLICKED, // const-string v0,"Clicked"
                            0x206e, M_TTEXT,
                            0x0003, // invoke-virtual {v3,v0} TextView.setText
                            0x000e,
                        ],
                    )
                }],
            )
        };
        // LSubAct subclasses Activity — so a setContentView reference
        // via it is a legit subclass ref (unlike Dialog's).
        let sub_act = C {
            sup: Some("Landroid/app/Activity;"),
            ..cls("LSubAct;", vec![])
        };
        // LAct.onCreate(Bundle): this=v4, savedState=v5.
        let act = C {
            sup: Some("Landroid/app/Activity;"),
            ..cls(
                "LAct;",
                vec![M {
                    name: "onCreate",
                    lits: &["Hello"],
                    ins: 2,
                    regs: 6,
                    ..virt(
                        "onCreate",
                        &["Landroid/os/Bundle;"],
                        vec![
                            0x0022, T_BTN, // new-instance v0,Button
                            0x2070, M_BINIT, 0x0040, // invoke-direct {v0,v4} Button.<init>
                            0x001f, T_VIEW, // check-cast v0,View (Button IS-A View)
                            0x0314, 0x0000, 0x41A0, // const v3,#0x41A00000 (20f as I32)
                            0x206e, M_BSIZE,
                            0x0030, // invoke-virtual {v0,v3} Button.setTextSize
                            0x0122, T_LN, // new-instance v1,LL
                            0x1070, M_OINIT, 0x0001, // invoke-direct {v1} Object.<init>
                            0x021a, S_HELLO, // const-string v2,"Hello"
                            0x206e, M_BTEXT, 0x0020, // invoke-virtual {v0,v2} Button.setText
                            0x206e, M_BCLICK,
                            0x0010, // invoke-virtual {v0,v1} Button.setOnClickListener
                            0x206e, M_BCLICK,
                            0x0050, // invoke-virtual {v0,v5} — null clears it
                            0x206e, M_BCLICK, 0x0010, // re-set the listener
                            0x0322, T_DLG, // new-instance v3,Dialog
                            0x2070, M_DINIT, 0x0043, // invoke-direct {v3,v4} Dialog.<init>
                            // Dialog.setContentView must NOT reach Activity's stub
                            // (v1 is a listener, not a view — a wrong dispatch errors).
                            0x206e, M_DCV, 0x0013, 0x206e, M_SCV2,
                            0x0004, // invoke-virtual {v4,v0} LSubAct.setContentView
                            0x000e,
                        ],
                    )
                }],
            )
        };
        let mut classes = stubs;
        classes.push(listener);
        classes.push(sub_act);
        classes.push(act);
        let (mut b, ix) = build(&classes);
        patch(&mut b, T_BTN, ix.t("Landroid/widget/Button;") as u16);
        patch(&mut b, T_LN, ix.t("LL;") as u16);
        patch(
            &mut b,
            M_BINIT,
            ix.m("Landroid/widget/Button;", "<init>") as u16,
        );
        patch(&mut b, M_OINIT, ix.m("Ljava/lang/Object;", "<init>") as u16);
        patch(
            &mut b,
            M_BTEXT,
            ix.m("Landroid/widget/Button;", "setText") as u16,
        );
        patch(
            &mut b,
            M_BCLICK,
            ix.m("Landroid/widget/Button;", "setOnClickListener") as u16,
        );
        patch(
            &mut b,
            M_ACV,
            ix.m("Landroid/app/Activity;", "setContentView") as u16,
        );
        patch(
            &mut b,
            M_TTEXT,
            ix.m("Landroid/widget/TextView;", "setText") as u16,
        );
        patch(
            &mut b,
            M_BSIZE,
            ix.m("Landroid/widget/Button;", "setTextSize") as u16,
        );
        patch(&mut b, M_SCV2, ix.m("LSubAct;", "setContentView") as u16);
        patch(
            &mut b,
            M_DINIT,
            ix.m("Landroid/app/Dialog;", "<init>") as u16,
        );
        patch(
            &mut b,
            M_DCV,
            ix.m("Landroid/app/Dialog;", "setContentView") as u16,
        );
        patch(&mut b, T_VIEW, ix.t("Landroid/view/View;") as u16);
        patch(&mut b, T_DLG, ix.t("Landroid/app/Dialog;") as u16);
        patch(&mut b, S_HELLO, ix.s_idx("Hello") as u16);
        patch(&mut b, S_CLICKED, ix.s_idx("Clicked") as u16);
        b
    }

    #[test]
    fn guest_builds_view_and_click_roundtrips() {
        let ui = GuestUi::new(200, 100, fonts());
        let mut vm = Vm::new(ovelo_dex::DexFile::open(guest_dex()).unwrap()).unwrap();
        GuestUi::register(&ui, &mut vm);
        let act = vm.instantiate("LAct;").expect("LAct instantiates");
        vm.call_virtual(act, "onCreate", &[V::Ref(0)])
            .expect("onCreate runs");

        // The guest Button is the content view and renders text.
        {
            let mut s = ovelo_ui::Surface::new(200, 100).unwrap();
            ui.borrow_mut().root.draw_to(&mut s);
            let (r, _, _, _) = s.pixel(2, 12).straight();
            assert_eq!(r, 0xDD, "button bg drawn"); // Button.bg default
        }
        // Tap it: down+up inside → one (listener, view) click queued.
        {
            let mut u = ui.borrow_mut();
            assert!(u.root.dispatch_touch(true, 5, 5));
            assert!(u.root.dispatch_touch(false, 5, 5));
            let pending = u.drain_clicks();
            assert_eq!(pending.len(), 1);
            let (listener, view) = pending[0];
            drop(u);
            // Pump: guest onClick → setText("Clicked") on the Button.
            vm.call_virtual(listener, "onClick", &[V::Ref(view)])
                .expect("onClick runs");
        }
        let u = ui.borrow();
        // Find the bound Button handle via the queue's view arg replay:
        // any bound view that is a Btn must now hold "Clicked".
        let mut found = false;
        for h in 0..16u32 {
            if let Some(rc) = u.view(h) {
                if let GuestView::Btn(b) = &*rc.borrow() {
                    assert_eq!(b.tv.text, "Clicked");
                    found = true;
                }
            }
        }
        assert!(found, "button handle bound");
    }

    #[test]
    fn resetting_a_clean_view_still_draws() {
        let ui = GuestUi::new(64, 64, fonts());
        let rc = Rc::new(RefCell::new(GuestView::Plain(crate::view::View::new(
            Length::WrapContent,
            Length::WrapContent,
            Size { w: 4, h: 4 },
        ))));
        {
            let mut u = ui.borrow_mut();
            u.root.set_content_view(Box::new(SharedView(rc.clone())));
            let mut s = ovelo_ui::Surface::new(64, 64).unwrap();
            u.root.draw_to(&mut s);
            assert!(!u.root.is_dirty());
            // Re-attach the same (clean) view — must redraw.
            u.root.set_content_view(Box::new(SharedView(rc)));
            assert!(u.root.is_dirty());
        }
    }
}

//! `android.view` builtins: an inflated view tree held on pseudo fields.
//!
//! `LayoutInflater.inflate` parses the real layout XML from the installed
//! APK (`res.rs`) into a tree of host objects — each carries its
//! `android:id`, children, parent and tag as pseudo fields — so
//! `findViewById`, `getChildAt`/`getChildCount`, and ViewBinding's
//! `findChildViewById` walk real structure instead of nulls.

use super::*;
use crate::{GIdx, HostCall};
use ovelo_apk::manifest::{Attribute, Element, Value};

const VIEW: &str = "Landroid/view/View;";
const VIEWGROUP: &str = "Landroid/view/ViewGroup;";
const FRAMELAYOUT: &str = "Landroid/widget/FrameLayout;";
const WINDOW: &str = "Landroid/view/Window;";
const INFLATER: &str = "Landroid/view/LayoutInflater;";
const CHOR: &str = "Landroid/view/Choreographer;";

// Pseudo keys for view-tree state (see mod.rs for the other P_* slots).
const P_VIEW_ID: u32 = u32::MAX - 7;
const P_CHILDREN: u32 = u32::MAX - 8;
pub(crate) const P_PARENT: u32 = u32::MAX - 9;
const P_TAG: u32 = u32::MAX - 10;
/// Bumped by `invalidate`/`postInvalidate` — the value is never read;
/// the write exists to move `view_gen` so draw-only changes repaint.
const P_INVALID_SEQ: u32 = u32::MAX - 49;
/// Set while the view is attached to a window — `isAttachedToWindow`
/// must report false before `onAttachedToWindow` actually fires
/// (AbstractComposeView defers `ensureCompositionCreated` until it
/// does; a hardcoded true composes a parentless view).
pub(crate) const P_ATTACHED: u32 = u32::MAX - 48;
const P_ATTACH_QUEUED: u32 = u32::MAX - 70;
/// Live `setText`/`setHint` argument — read by `dex_view` before the
/// inflated `android:text` attribute. `pub`: the guest bridge's setter
/// overrides write it for dex-inflated (non-GuestView) receivers.
pub const P_TEXT: u32 = u32::MAX - 11;
/// `setContentDescription` value — accessibility label, never drawn.
pub const P_CDESC: u32 = u32::MAX - 28;
/// Pseudo key: `android:hint` / `setHint` text — kept separate from
/// P_TEXT so `getText` on a search box returns the user's input (empty)
/// rather than the hint.
const P_HINT: u32 = u32::MAX - 26;
const P_CONTENT: u32 = u32::MAX - 12;
const P_WINDOW: u32 = u32::MAX - 13;
/// The SurfaceHolder pseudo object handed out by `getHolder` — held
/// per SurfaceView so callbacks registered on it survive.
const P_HOLDER: u32 = u32::MAX - 46;
/// Callbacks registered via `SurfaceHolder.addCallback` — Array of Refs
/// plus a "created" flag so surfaceCreated/surfaceChanged fire once.
const P_HOLDER_CBS: u32 = u32::MAX - 47;
// Marker keys are pub(crate): `is_instance` consults them so inflated
// views pass `instanceof View`/`ViewGroup` checks.
pub(crate) const P_IS_VIEW: u32 = u32::MAX - 14;
pub(crate) const P_IS_GROUP: u32 = u32::MAX - 15;
const P_DECOR: u32 = u32::MAX - 16;
const P_LISTENER: u32 = u32::MAX - 17;
/// The `Context` a view was constructed with — read by `getContext`.
/// pub(crate): `call_builtin` sets it for host-registered ctors too.
pub(crate) const P_CONTEXT: u32 = u32::MAX - 18;
const P_VIS: u32 = u32::MAX - 19;
/// Pseudo key: root element was `<merge>` — splice children on attach.
const P_MERGE: u32 = u32::MAX - 21;
/// `setClipChildren(bool)` / `android:clipChildren` — I32 1 clips
/// (default), 0 lets children draw outside their bounds.
const P_CLIP_CHILDREN: u32 = u32::MAX - 29;
/// Pseudo keys: drawables given to `setIndeterminateDrawable` /
/// `setProgressDrawable` — ProgressBar getters must hand them back (not
/// a zero-init stub) or their dex fields read null downstream.
const P_IND_DRAW: u32 = u32::MAX - 22;
const P_PROG_DRAW: u32 = u32::MAX - 23;
/// The window target (activity/dialog handle) owning a decor root.
const P_WIN_OWNER: u32 = u32::MAX - 24;
/// View -> its lazily minted ViewTreeObserver object.
const P_VTO: u32 = u32::MAX - 25;
/// `ImageView`'s current image: a `Bitmap` handle, a `Drawable`
/// handle, or a `Uri`/res-id the renderer resolves.
const P_IMAGE: u32 = u32::MAX - 27;
/// Stable placeholder returned by `getDrawable()` before any image is
/// set. Kept separate from P_IMAGE so it does not suppress XML src.
const P_IMAGE_PLACEHOLDER: u32 = u32::MAX - 30;
/// Internal popup state: root view attached over the activity and the
/// ListPopupWindow/row position carried by synthetic row listeners.
const P_POPUP_ROOT: u32 = u32::MAX - 31;
const P_POPUP_OWNER: u32 = u32::MAX - 32;
const P_POPUP_POS: u32 = u32::MAX - 33;
/// Overflow popup whose rows were compressed to fit the available height.
const P_POPUP_COMPACT: u32 = u32::MAX - 53;
/// Synthetic dialog button state: target dialog, button which, and marker.
const P_DIALOG_BUTTON_DIALOG: u32 = u32::MAX - 54;
const P_DIALOG_BUTTON_WHICH: u32 = u32::MAX - 55;
const P_DIALOG_HAS_BUTTONS: u32 = u32::MAX - 56;
const P_DIALOG_POS_LISTENER: u32 = u32::MAX - 57;
const P_DIALOG_NEG_LISTENER: u32 = u32::MAX - 58;
const P_DIALOG_CONTENT: u32 = u32::MAX - 59;
/// Dialog cancelable flag (`setCancelable(false)` stores 0).
const P_DIALOG_CANCELABLE: u32 = u32::MAX - 34;
/// Synthetic AlertDialog panel wrapping custom content.
const P_DIALOG_PANEL: u32 = u32::MAX - 35;
/// Pseudo key: a view's lazily-minted `Menu` (Toolbar.getMenu() etc.) /
/// the menuish marker on the Menu itself (`I32(1)`).
const P_MENU: u32 = 0xFFFD_FFF8;
/// Pseudo key: a MenuItem's `android:id`.
const P_MENU_ID: u32 = 0xFFFD_FFF7;
/// Pseudo key: a MenuItem's visible flag.
const P_MENU_VIS: u32 = 0xFFFD_FFF6;
/// Pseudo key: a MenuItem's enabled/checkable flag.
const P_MENU_EN: u32 = 0xFFFD_FFF5;
/// Pseudo key: a MenuItem's checked flag.
const P_MENU_CHK: u32 = 0xFFFD_FFF4;
/// Pseudo key: a MenuItem's group id.
const P_MENU_GROUP: u32 = 0xFFFD_FFF3;
/// Pseudo key: a MenuItem's checkable flag (distinct from enabled).
const P_MENU_CKABLE: u32 = 0xFFFD_FFF2;
/// Pseudo key: a MenuItem's inflated action view.
const P_MENU_ACTION_VIEW: u32 = 0xFFFD_FFEC;

/// Backing store key on a dex menu object (`Obj::Instance` — e.g.
/// androidx `MenuBuilder`): the item list lives on a lazily-minted host
/// `HostState::Map`, referenced here, so every menu op shares it.
const P_MENU_STORE: u32 = 0xFFFD_FFED;
/// Pseudo key: on a SubMenu, the MenuItem representing it in the parent.
const P_SUBMENU_ITEM: u32 = 0xFFFD_FFF1;
/// Pseudo key: on a MenuItem, the SubMenu it opens.
const P_ITEM_SUBMENU: u32 = 0xFFFD_FFF0;
/// Pseudo key: `Activity.finish()` was called — later lifecycle
/// callbacks must be skipped.
pub(crate) const P_FINISHED: u32 = 0xFFFD_FFEF;
pub(crate) const P_INTENT: u32 = 0xFFFD_FFEE;

// TypedArray backing — below the `tag_key` band (which bottoms out at
// ~u32::MAX - 0x10014).
/// Pseudo key: the int[] styleable array a `TypedArray` was created over.
pub(crate) const P_TA_ATTRS: u32 = 0xFFFD_FFFE;
/// Pseudo key: the `AttributeSet` behind a `TypedArray` (0 = theme-scoped).
pub(crate) const P_TA_SET: u32 = 0xFFFD_FFFF;
/// Pseudo key: explicit style res id on a `TypedArray`
/// (`obtainStyledAttributes(resId, attrs)`).
pub(crate) const P_TA_STYLE: u32 = 0xFFFD_FFFD;
/// Pseudo key: `defStyleAttr` res id on a `TypedArray`.
pub(crate) const P_TA_DEF_ATTR: u32 = 0xFFFD_FFFC;
/// Pseudo key: `defStyleRes` res id on a `TypedArray`.
pub(crate) const P_TA_DEF_RES: u32 = 0xFFFD_FFFB;
/// Pseudo key: the `AttributeSet` a view was inflated with (for
/// `getLayoutParams` re-parse).
const P_ATTRS: u32 = 0xFFFD_FFFA;
/// Pseudo key: `LayoutParams` object set by `setLayoutParams` or
/// constructed lazily on first `getLayoutParams`.
const P_LP: u32 = 0xFFFD_FFF9;

// View frame: `layout`/`setFrame`/`measure`/`setMeasuredDimension`
// record geometry here so `getWidth`/`getHeight`/`getMeasured*` and
// `getLeft`/`getTop`/`getRight`/`getBottom` answer with real numbers.
// Without them, dex layout code (e.g. RecyclerView's
// `OrientationHelper` totals) sees every view as 0×0 and lays out
// nothing.
const P_LAYOUT_REQ: u32 = 0xFFFD_FFEA;
const P_LEFT: u32 = 0xFFFD_FFE9;
const P_TOP: u32 = 0xFFFD_FFE8;
const P_RIGHT: u32 = 0xFFFD_FFE7;
const P_BOTTOM: u32 = 0xFFFD_FFE6;
/// Set by the real `layout()` builtin — host size pushes must not
/// overwrite frames the dex layout pass computed.
const P_LAYOUT_DONE: u32 = u32::MAX - 52;
const P_MEAS_W: u32 = 0xFFFD_FFE5;
const P_MEAS_H: u32 = 0xFFFD_FFE4;
/// `setMinimumWidth`/`setMinimumHeight` — feed `getMinimum*`/
/// `getSuggestedMinimum*`.
const P_MINW: u32 = 0xFFFD_FFDF;
const P_MINH: u32 = 0xFFFD_FFDE;
/// `LinearLayout.setOrientation` — 0=HORIZONTAL, 1=VERTICAL.
const P_ORIENTATION: u32 = 0xFFFD_FFDD;
/// CompoundButton checked state / RadioGroup's checked child id.
const P_CHECKED: u32 = 0xFFFD_FFDC;
const P_RADIO_CHECKED: u32 = 0xFFFD_FFDB;
/// CompoundButton/RadioGroup `OnCheckedChangeListener`.
const P_CHECK_LISTENER: u32 = 0xFFFD_FFD3;
/// Set while a checked-change callback is on the dex stack.
const P_CHECK_BROADCAST: u32 = 0xFFFD_FFD2;
const P_SCROLL_X: u32 = 0xFFFD_FFD9;
const P_SCROLL_Y: u32 = 0xFFFD_FFD8;
const P_PAD_L: u32 = 0xFFFD_FFD7;
const P_PAD_T: u32 = 0xFFFD_FFD6;
const P_PAD_R: u32 = 0xFFFD_FFD5;
const P_PAD_B: u32 = 0xFFFD_FFD4;
/// TextView compound drawables in start/top/end/bottom order.
const P_COMPOUND_DRAWABLES: u32 = 0xFFFD_FFDA;
/// Live `setTextColor` ARGB — written by the guest bridge's
/// fallthrough for dex-inflated views.
pub const P_TCOLOR: u32 = 0xFFFD_FFE3;
/// Live `setTextSize` px — same fallthrough path as [`P_TCOLOR`].
pub const P_TSIZE: u32 = 0xFFFD_FFE2;
/// Live `setBackgroundColor` — always a packed ARGB literal, even
/// when the alpha byte reads as a positive int (never a res id).
pub const P_BGCOLOR: u32 = 0xFFFD_FFE1;
/// Live `setBackgroundResource` res id — resolved by `color_v`.
pub const P_BGRES: u32 = 0xFFFD_FFE0;
/// Resolved layout direction stored on a `*LayoutParams` by
/// `resolveLayoutDirection` (AOSP `mLayoutDirection`).
const P_LP_DIR: u32 = u32::MAX - 29;
/// Relative margins (`startMargin`/`endMargin`) — pseudos because
/// `MarginLayoutParams.startMargin`/`endMargin` are never referenced
/// by dex, so they get no canon field key to store under.
const P_MSTART: u32 = u32::MAX - 42;
const P_MEND: u32 = u32::MAX - 43;
/// `setOnTouchListener` target — kept apart from P_LISTENER (click)
/// so a view can carry both.
const P_TOUCH_L: u32 = u32::MAX - 44;
/// MotionEvent pseudo fields (host-bridged pointer events).
const P_ME_ACTION: u32 = u32::MAX - 45;
const P_KE_ACTION: u32 = u32::MAX - 64;
const P_KE_CODE: u32 = u32::MAX - 65;
const P_KE_UNICODE: u32 = u32::MAX - 66;
const P_KE_REPEAT: u32 = u32::MAX - 67;
const P_INPUT_CONN: u32 = u32::MAX - 68;
const P_FOCUSED: u32 = u32::MAX - 69;
const P_ME_X: u32 = u32::MAX - 46;
const P_ME_Y: u32 = u32::MAX - 47;
const P_ME_DOWN_TIME: u32 = u32::MAX - 60;
const P_ME_EVENT_TIME: u32 = u32::MAX - 61;
const P_ME_RAWX: u32 = u32::MAX - 62;
const P_ME_RAWY: u32 = u32::MAX - 63;

/// Lazily mint a per-view `ViewTreeObserver` — guests register
/// `OnGlobalLayoutListener`s on it; `fire_global_layout` (called from
/// the host layout driver) invokes them.
fn vto_of(vm: &mut Vm, view: u32) -> V {
    if let Some(v) = pseudo_get(vm, view, P_VTO) {
        return v;
    }
    let V::Ref(o) = host(vm, "Landroid/view/ViewTreeObserver;", HostState::Empty) else {
        return V::Ref(0);
    };
    pseudo_set(vm, o, P_TARGET, V::Ref(view));
    pseudo_set(vm, view, P_VTO, V::Ref(o));
    V::Ref(o)
}

/// Fire `OnGlobalLayoutListener`s whose view belongs to `root`'s tree —
/// listeners on detached views or a previous screen's views must not
/// see a layout that never reached them (one-shot init would fire
/// early and consume itself without an actual layout).
fn fire_vto(vm: &mut Vm, root: u32, fire_pre: bool, fire_layout: bool) {
    // Listeners hang on ancestors of the laid-out content too (decor,
    // android.R.id.content frames) — normalize root to its topmost
    // ancestor before testing membership.
    let mut top = root;
    for _ in 0..64 {
        match pseudo_get(vm, top, P_PARENT) {
            Some(V::Ref(p)) if p != 0 => top = p,
            _ => break,
        }
    }
    let root = top;
    fn attached(vm: &Vm, mut v: u32, root: u32) -> bool {
        for _ in 0..64 {
            if v == root {
                return true;
            }
            match pseudo_get(vm, v, P_PARENT) {
                Some(V::Ref(p)) if p != 0 => v = p,
                _ => return false,
            }
        }
        false
    }
    // OnPreDraw fires before OnGlobalLayout in a real VTO pass —
    // Glide's ViewTarget derives the view size here and kicks the
    // request engine, which only then calls onSizeReady.
    let predraw: Vec<u32> = vm
        .vto_predraw
        .iter()
        .filter(|(obs, _)| match pseudo_get(vm, **obs, P_TARGET) {
            Some(V::Ref(v)) => attached(vm, v, root),
            _ => false,
        })
        .flat_map(|(_, ls)| ls.iter().copied())
        .collect();
    if fire_pre {
        for l in predraw {
            if crate::dbg_flag("OVELO_VIEWDBG") {
                eprintln!("[viewdbg] onPreDraw h{l} {}", vm.dbg_class_of(l));
            }
            let _ = vm.call_virtual(l, "onPreDraw", &[]);
        }
    }
    if !fire_layout {
        return;
    }
    let all: Vec<u32> = vm
        .vto_listeners
        .iter()
        .filter(|(obs, _)| match pseudo_get(vm, **obs, P_TARGET) {
            Some(V::Ref(v)) => attached(vm, v, root),
            _ => false,
        })
        .flat_map(|(_, ls)| ls.iter().copied())
        .collect();
    if crate::dbg_flag("OVELO_VIEWDBG") && !all.is_empty() {
        eprintln!("[viewdbg] fire_global_layout {} listeners", all.len());
    }
    for l in all {
        if crate::dbg_flag("OVELO_VIEWDBG") {
            eprintln!("[viewdbg] onGlobalLayout h{l} {}", vm.dbg_class_of(l));
        }
        if let Err(e) = vm.call_virtual(l, "onGlobalLayout", &[]) {
            if crate::dbg_flag("OVELO_VIEWDBG") {
                eprintln!("[viewdbg] onGlobalLayout {l} err {e:?}");
            }
        }
    }
}

/// Fire `OnPreDrawListener`s for `root`'s tree. Android sends this
/// before every drawn frame, including redraws that did not layout.
pub fn fire_pre_draw(vm: &mut Vm, root: u32) {
    fire_vto(vm, root, true, false);
}

/// Fire `OnGlobalLayoutListener`s for `root`'s tree after a real layout.
pub fn fire_global_layout(vm: &mut Vm, root: u32) {
    fire_vto(vm, root, false, true);
}

/// Re-exported for `context_call` — Activity.findViewById searches the
/// content tree set by setContentView.
pub(crate) fn content_root(vm: &mut Vm, h: u32) -> Option<u32> {
    match pseudo_get(vm, h, P_CONTENT) {
        Some(V::Ref(r)) if r != 0 => Some(r),
        _ => window_of(vm, h).and_then(|w| decor_of(vm, w)),
    }
}

/// What a host renderer needs from one node of the dex view tree —
/// a snapshot so the framework crate can materialize real `Drawable`s
/// without touching pseudo-field internals.
#[derive(Debug, Clone)]
pub struct DexViewInfo {
    /// Class descriptor of the view object (`Landroid/widget/TextView;`,
    /// a dex subclass like `Landroidx/appcompat/widget/AppCompatTextView;`,
    /// or a host stub class).
    pub desc: String,
    /// The node accepts children (`P_IS_GROUP` or children present).
    pub is_group: bool,
    /// Text (`setText`/`android:text`/`hint`), resolved to a string.
    pub text: Option<String>,
    /// `android:textSize` in px, when set.
    pub text_size: Option<f32>,
    /// `android:textColor` / `setTextColor` as ARGB, when set.
    pub text_color: Option<i32>,
    /// `android:background` / `setBackgroundColor` as ARGB when it is a
    /// plain color (references resolve through the arsc table).
    pub bg_color: Option<i32>,
    /// Requested width: -1 match_parent, -2 wrap_content, else px.
    pub lp_w: i32,
    /// Requested height.
    pub lp_h: i32,
    /// `View.VISIBLE` (invisible/gone nodes are skipped by the caller).
    pub visible: bool,
    /// OnClick listener handle (0 = none).
    pub listener: u32,
    /// Dex-assigned frame `(l, t, r, b)` relative to the parent, once
    /// `layout`/`setFrame` ran (None before first layout pass).
    pub frame: Option<(i32, i32, i32, i32)>,
    /// `View.requestLayout` is pending for this view.
    pub layout_requested: bool,
    /// `setImage*`-attached pixels (`w`, `h`, RGBA8) after resolving
    /// Bitmap hosts, drawable field chains, res-ids and file/Uri paths.
    pub image: Option<(u32, u32, std::rc::Rc<Vec<u8>>)>,
    /// Content padding `(left, top, right, bottom)` in px.
    pub padding: (i32, i32, i32, i32),
    /// ImageView scaleType enum (0 matrix, 1 fitXY, 3 fitCenter).
    pub scale_type: i32,
    /// Parent view handle (0 = detached/root).
    pub parent: u32,
    /// Child view handles, in z-order.
    pub children: Vec<u32>,
    /// `android:gravity` as the AOSP flag int (LEFT=3, RIGHT=5,
    /// CENTER_HORIZONTAL=1, TOP=0x30, BOTTOM=0x50, CENTER_VERTICAL=0x10).
    /// 0 = unset — treat as top|start.
    pub gravity: i32,
    /// `android:clipChildren` / `setClipChildren` — when false the
    /// parent's draw pass doesn't clip children to their bounds.
    pub clip_children: bool,
    /// `android:maxLines` — caps wrapped line count; overflowing text
    /// truncates to the first `n` lines.
    pub max_lines: Option<i32>,
    /// Horizontal scroll offset (`scrollTo`/`setScrollX`).
    pub scroll_x: i32,
    /// Vertical scroll offset (`scrollTo`/`setScrollY`).
    pub scroll_y: i32,
    /// LinearLayout orientation: true for vertical, false for horizontal.
    pub linear_vertical: bool,
    /// CompoundButton checked state, when this view is checkable.
    pub compound_checked: Option<bool>,
}

/// The activity's content view handle (what `setContentView` attached).
pub(crate) fn activity_content(vm: &Vm, activity: u32) -> Option<u32> {
    match pseudo_get(vm, activity, P_CONTENT) {
        Some(V::Ref(h)) if h != 0 => Some(h),
        // Appcompat installs content dex-side — the view attached
        // under android.R.id.content while this activity ran is the
        // real content root (owner 0 = pre-activity inflations).
        _ => vm
            .content_views
            .get(&activity)
            .or_else(|| vm.content_views.get(&0))
            .copied(),
    }
}

fn attr_map(vm: &Vm, h: u32) -> Option<&Vec<(V, V)>> {
    match pseudo_get(vm, h, P_ATTRS) {
        Some(V::Ref(a)) => match vm.heap.get(a) {
            Some(Obj::Host {
                state: HostState::Map(m),
                ..
            }) => Some(m),
            _ => None,
        },
        _ => None,
    }
}

fn attr_find(vm: &Vm, h: u32, name: &str) -> Option<V> {
    for (k, v) in attr_map(vm, h)? {
        if let V::Ref(kh) = k {
            if vm.heap.str_val(*kh) == Some(name) {
                return Some(*v);
            }
        }
    }
    None
}

fn xml_value_bool(vm: &Vm, v: &Value) -> Option<bool> {
    match v {
        Value::Int(i) => Some(*i != 0),
        Value::Bool(b) => Some(*b),
        Value::Str(t) => Some(t.trim() != "false" && t.trim() != "0"),
        Value::Reference(id) => attr_bool_ref(vm, *id),
        Value::Other { data_type, data } => match data_type {
            0x12 | 0x10 | 0x11 | 0x04 => Some(*data != 0),
            0x01 => attr_bool_ref(vm, *data),
            _ => None,
        },
        _ => None,
    }
}

/// `@bool/x` (possibly chained) → the boolean it holds.
fn attr_bool_ref(vm: &Vm, id: u32) -> Option<bool> {
    let res = vm.apk_res.as_ref()?;
    let mut ev = res.entry_value(id)?;
    for _ in 0..8 {
        match ev.data_type {
            // TYPE_INT_BOOLEAN / ints / floats — truthiness on data.
            0x12 | 0x10 | 0x11 | 0x04 => return Some(ev.data != 0),
            // TYPE_REFERENCE — follow one level.
            0x01 => ev = res.entry_value(ev.data)?,
            0x03 => {
                return ev
                    .string
                    .as_deref()
                    .map(|t| t.trim() != "false" && t.trim() != "0")
            }
            _ => return None,
        }
    }
    None
}

/// Resolve an attribute out of the explicit XML `style` reference.
/// `attr_find` only sees direct tag attributes, while Android applies
/// style items before falling back to the theme's default text color.
fn themed_attr_entry(
    res: &crate::res::ApkResources,
    theme: u32,
    overlay: Option<u32>,
    attr: u32,
    depth: u32,
) -> Option<ovelo_apk::arsc::EntryValue> {
    if depth >= 8 {
        return None;
    }
    let ev = overlay
        .and_then(|o| res.style_item(o, attr))
        .or_else(|| res.style_item(theme, attr))?
        .clone();
    if ev.data_type == 0x02 {
        themed_attr_entry(res, theme, overlay, ev.data, depth + 1)
    } else {
        Some(ev)
    }
}

fn themed_value_color(
    res: &crate::res::ApkResources,
    theme: u32,
    overlay: Option<u32>,
    v: &Value,
    depth: u32,
) -> Option<u32> {
    if depth >= 12 {
        return None;
    }
    match v {
        Value::Str(s) => vector::parse_color(s),
        Value::Int(i) => Some(*i as u32),
        Value::Other { data_type, data } if (0x1c..=0x1f).contains(data_type) => Some(*data),
        Value::Other {
            data_type: 0x02,
            data,
        } => {
            let ev = themed_attr_entry(res, theme, overlay, *data, depth + 1)?;
            themed_entry_color(res, theme, overlay, &ev, depth + 1)
        }
        Value::Reference(r) => themed_color_resource(res, theme, overlay, *r, depth + 1),
        _ => None,
    }
}

fn themed_entry_color(
    res: &crate::res::ApkResources,
    theme: u32,
    overlay: Option<u32>,
    ev: &ovelo_apk::arsc::EntryValue,
    depth: u32,
) -> Option<u32> {
    if depth >= 12 {
        return None;
    }
    match ev.data_type {
        0x1c..=0x1f | 0x10 | 0x11 | 0x04 => Some(ev.data),
        0x01 => themed_color_resource(res, theme, overlay, ev.data, depth + 1),
        0x02 => {
            let next = themed_attr_entry(res, theme, overlay, ev.data, depth + 1)?;
            themed_entry_color(res, theme, overlay, &next, depth + 1)
        }
        0x03 => ev.string.as_deref().and_then(vector::parse_color),
        _ => None,
    }
}

fn themed_selector_color(
    res: &crate::res::ApkResources,
    theme: u32,
    overlay: Option<u32>,
    res_id: u32,
    depth: u32,
) -> Option<u32> {
    let els = res.document(res_id)?;
    for el in &els {
        if el.name != "item" {
            continue;
        }
        let stateful = el.attrs.iter().any(|a| {
            if !a.name.starts_with("state_") {
                return false;
            }
            if a.name == "state_enabled" {
                matches!(&a.value, Value::Bool(false) | Value::Int(0))
            } else {
                true
            }
        });
        if stateful {
            continue;
        }
        if let Some(v) = el.android("color") {
            if let Some(c) = themed_value_color(res, theme, overlay, v, depth + 1) {
                return Some(c);
            }
        }
    }
    None
}

fn themed_color_resource(
    res: &crate::res::ApkResources,
    theme: u32,
    overlay: Option<u32>,
    res_id: u32,
    depth: u32,
) -> Option<u32> {
    if depth >= 12 {
        return None;
    }
    if let Some(ev) = res.entry_value(res_id).cloned() {
        if ev.data_type == 0x03 && ev.string.as_deref().is_some_and(|s| s.starts_with("res/")) {
            if let Some(c) = themed_selector_color(res, theme, overlay, res_id, depth + 1) {
                return Some(c);
            }
        }
        return themed_entry_color(res, theme, overlay, &ev, depth + 1).or_else(|| {
            vector::drawable_flat_color(
                vector::RCtx {
                    res,
                    theme,
                    depth: depth.min(15) as u8,
                },
                res_id,
            )
        });
    }
    crate::res::framework_color(res_id)
}

fn overlay_aware_style_color(vm: &Vm, style: u32, color_res: u32) -> Option<u32> {
    let res = vm.apk_res.as_ref()?;
    let overlay_attr = res.attr_id_named("materialThemeOverlay")?;
    let overlay = match res.style_item(style, overlay_attr) {
        Some(ev) if ev.data_type == 0x01 => Some(ev.data),
        _ => None,
    }?;
    themed_color_resource(res, vm.theme_id, Some(overlay), color_res, 0)
}

fn view_content_image(
    vm: &Vm,
    h: u32,
    icon_only: bool,
) -> Option<(u32, u32, std::rc::Rc<Vec<u8>>)> {
    pseudo_get(vm, h, P_IMAGE)
        .and_then(|v| image_pixels(vm, v))
        .or_else(|| {
            // MaterialButton's `app:icon` is a compound drawable rather
            // than an ImageView src. The host materialization has one
            // centered image slot, which is correct for the common
            // icon-only button; text+icon layout needs real compound
            // drawable support and is intentionally not approximated.
            icon_only.then(|| attr_find(vm, h, "icon").and_then(|v| image_pixels(vm, v)))?
        })
}

fn style_attr_color(vm: &Vm, style: u32, attr_id: u32) -> Option<i32> {
    let res = vm.apk_res.as_ref()?;
    let ev = res.style_item(style, attr_id)?;
    match ev.data_type {
        0x01 => overlay_aware_style_color(vm, style, ev.data)
            .map(|c| c as i32)
            .or_else(|| color_v(vm, V::I32(ev.data as i32))),
        0x02 => crate::theme_color(vm, ev.data).map(|c| c as i32),
        0x1c..=0x1f | 0x10 | 0x11 | 0x04 => Some(ev.data as i32),
        0x03 => ev
            .string
            .as_deref()
            .and_then(vector::parse_color)
            .map(|c| c as i32),
        _ => None,
    }
}

/// Resolve an ARGB color from an int literal or a color resource ref.
fn color_v(vm: &Vm, v: V) -> Option<i32> {
    match v {
        V::I32(c) if c < 0 => Some(c),
        V::I32(id) if id > 0 => {
            let res = vm.apk_res.as_ref()?;
            // `@attr` ids resolve through the bound theme; TYPE_REFERENCE
            // and TYPE_ATTRIBUTE hops chase until a literal lands. An
            // entry that resolves to an XML drawable file yields the
            // drawable's representative fill color (its `data` is the
            // string-pool index of the path — NOT a color).
            let mut cur = id as u32;
            for _ in 0..8 {
                // `android.R.color` ids (0x0106xxxx) aren't in the app
                // table; the framework palette answers for them.
                let Some(ev) = res.entry_value(cur).cloned() else {
                    return crate::res::framework_color(cur).map(|c| c as i32);
                };
                match ev.data_type {
                    0x01 => cur = ev.data,
                    0x02 => {
                        return vector::resolve_ref_color(
                            vector::RCtx {
                                res,
                                theme: vm.theme_id,
                                depth: 0,
                            },
                            ev.data,
                        )
                        .or_else(|| {
                            res.style_item(vm.theme_id, ev.data).and_then(|n| {
                                (n.data_type == 0x01).then(|| {
                                    vector::drawable_flat_color(
                                        vector::RCtx {
                                            res,
                                            theme: vm.theme_id,
                                            depth: 0,
                                        },
                                        n.data,
                                    )
                                })?
                            })
                        })
                        .map(|c| c as i32);
                    }
                    // Color ints / plain ints — literal.
                    0x1c..=0x1f | 0x10 | 0x11 | 0x04 => return Some(ev.data as i32),
                    // A string (drawable/xml path or color literal text).
                    0x03 => {
                        return ev
                            .string
                            .as_deref()
                            .and_then(vector::parse_color)
                            .or_else(|| {
                                vector::drawable_flat_color(
                                    vector::RCtx {
                                        res,
                                        theme: vm.theme_id,
                                        depth: 0,
                                    },
                                    cur,
                                )
                            })
                            .map(|c| c as i32);
                    }
                    _ => return None,
                }
            }
            None
        }
        _ => None,
    }
}

/// Int coercion for view args (dex passes I32; a few APIs accept F32).
fn vint(v: Option<&V>) -> i32 {
    match v {
        Some(V::I32(n)) => *n,
        Some(V::F32(f)) => *f as i32,
        Some(V::I64(n)) => *n as i32,
        _ => 0,
    }
}

/// `MeasureSpec` → the size the view should report (mode bits stripped;
/// UNSPECIFIED leaves 0).
fn spec_size(v: Option<V>) -> i32 {
    match v {
        Some(V::I32(s)) => {
            let mode = s >> 30;
            if mode == 0 {
                0
            } else {
                s & 0x3FFF_FFFF
            }
        }
        _ => 0,
    }
}

/// Read a pseudo int field (0 when unset).
fn pint(vm: &Vm, h: u32, key: u32) -> i32 {
    match pseudo_get(vm, h, key) {
        Some(V::I32(n)) => n,
        _ => 0,
    }
}

/// Whether the dex `layout()` builtin established this view's frame.
pub(crate) fn layout_done(vm: &Vm, h: u32) -> bool {
    pseudo_get(vm, h, P_LAYOUT_DONE).is_some()
}

/// Host-driven layout completes the same request cycle as View.layout:
/// leaving the flag set makes every later requestLayout a no-op write,
/// so host view-tree change detection never sees Compose re-layouts.
pub(crate) fn clear_layout_requested(vm: &mut Vm, h: u32) {
    pseudo_set(vm, h, P_LAYOUT_REQ, V::I32(0));
}

/// Mark a view and its ancestors as layout-requested, matching
/// `View.requestLayout`'s upward propagation through the parent chain.
pub fn mark_layout_requested(vm: &mut Vm, h: u32) {
    let mut cur = h;
    for _ in 0..64 {
        if cur == 0 || pseudo_get(vm, cur, P_IS_VIEW).is_none() {
            break;
        }
        pseudo_set(vm, cur, P_LAYOUT_REQ, V::I32(1));
        cur = match pseudo_get(vm, cur, P_PARENT) {
            Some(V::Ref(p)) => p,
            _ => 0,
        };
    }
}

pub(crate) fn invalid_seq(vm: &Vm, h: u32) -> i32 {
    pint(vm, h, P_INVALID_SEQ)
}

pub(crate) fn layout_requested(vm: &Vm, h: u32) -> bool {
    pint(vm, h, P_LAYOUT_REQ) != 0
}

/// True for a root view owned by a popup or dialog overlay.
pub fn is_popup_root(vm: &Vm, h: u32) -> bool {
    h != 0 && pseudo_get(vm, h, P_POPUP_ROOT).is_some()
}

/// Store a view frame; returns whether it changed (View.setFrame's
/// contract and `layout`'s `changed` argument).
pub(crate) fn frame_set(vm: &mut Vm, h: u32, l: i32, t: i32, r: i32, b: i32) -> bool {
    let changed = pint(vm, h, P_LEFT) != l
        || pint(vm, h, P_TOP) != t
        || pint(vm, h, P_RIGHT) != r
        || pint(vm, h, P_BOTTOM) != b
        || pseudo_get(vm, h, P_RIGHT).is_none();
    if changed {
        pseudo_set(vm, h, P_LEFT, V::I32(l));
        pseudo_set(vm, h, P_TOP, V::I32(t));
        pseudo_set(vm, h, P_RIGHT, V::I32(r));
        pseudo_set(vm, h, P_BOTTOM, V::I32(b));
    }
    changed
}

/// A layout length: `-1`/`MATCH_PARENT`, `-2`/`WRAP_CONTENT`, px otherwise.
fn len_v(vm: &Vm, v: V) -> i32 {
    match v {
        V::I32(i) if i <= 0 => i.max(-2),
        V::I32(_) => lp_px(vm, v).unwrap_or(-2),
        V::F32(f) => f as i32,
        _ => -2,
    }
}
/// Snapshot a dex view-tree node for host-side rendering.
pub(crate) fn dex_view_info(vm: &Vm, h: u32) -> Option<DexViewInfo> {
    if h == 0 || pseudo_get(vm, h, P_IS_VIEW).is_none() {
        return None;
    }
    let desc = class_of(vm, V::Ref(h));
    // Text: a setText'd jstr first (a live value wins over the xml attr),
    // then the inflated `text`/`hint` attributes. A Reference resolves
    // through the arsc string table.
    let text = match pseudo_get(vm, h, P_TEXT) {
        Some(V::Ref(th)) if th != 0 => s(vm, th),
        Some(V::I32(id)) if id > 0 => vm
            .apk_res
            .as_ref()
            .and_then(|r| r.string_of(id as u32))
            .map(|s| s.to_string()),
        _ => None,
    }
    .or_else(|| {
        attr_find(vm, h, "text")
            .or_else(|| attr_find(vm, h, "hint"))
            .and_then(|v| match v {
                V::Ref(th) => s(vm, th),
                V::I32(id) if id > 0 => vm
                    .apk_res
                    .as_ref()
                    .and_then(|r| r.string_of(id as u32))
                    .map(|s| s.to_string()),
                _ => None,
            })
    });
    let text_size = match pseudo_get(vm, h, P_TSIZE) {
        Some(V::F32(f)) => Some(f),
        Some(V::I32(b)) => Some(f32::from_bits(b as u32)),
        _ => attr_find(vm, h, "textSize").and_then(|v| match v {
            V::F32(f) => Some(f),
            V::I32(id) if id > 0 => vm.apk_res.as_ref().and_then(|r| r.dimen(id as u32)),
            _ => None,
        }),
    };
    let text_color = match pseudo_get(vm, h, P_TCOLOR) {
        // P_TCOLOR only carries setTextColor args — packed ARGB.
        Some(V::I32(c)) => Some(c),
        Some(v) => color_v(vm, v),
        _ => attr_find(vm, h, "textColor")
            .and_then(|v| color_v(vm, v))
            .or_else(|| match attr_find(vm, h, "style") {
                Some(V::I32(style)) if style > 0 => style_attr_color(vm, style as u32, 0x0101_0098),
                _ => None,
            })
            // TextView's default style takes its normal color from the
            // theme when XML/runtime didn't set one. Without this every
            // theme-colored label falls back to the renderer's black.
            .or_else(|| crate::theme_color(vm, 0x0101_0036).map(|c| c as i32)),
    };
    let bg_color = match pseudo_get(vm, h, P_BGCOLOR) {
        // P_BGCOLOR only carries setBackgroundColor args — packed
        // ARGB; a positive int is a low-alpha color, not a res id.
        Some(V::I32(c)) => Some(c),
        Some(v) => color_v(vm, v),
        _ => match pseudo_get(vm, h, P_BGRES) {
            Some(v) => color_v(vm, v),
            None => attr_find(vm, h, "background")
                .and_then(|v| color_v(vm, v))
                .or_else(|| match attr_find(vm, h, "style") {
                    Some(V::I32(style)) if style > 0 => vm
                        .apk_res
                        .as_ref()
                        .and_then(|r| r.attr_id_named("backgroundTint"))
                        .and_then(|attr| style_attr_color(vm, style as u32, attr)),
                    _ => None,
                }),
        },
    };
    let lp_w = attr_find(vm, h, "layout_width")
        .map(|v| len_v(vm, v))
        .unwrap_or(-1);
    let lp_h = attr_find(vm, h, "layout_height")
        .map(|v| len_v(vm, v))
        .unwrap_or(-2);
    // `android:visibility` is an XML enum: 0=visible, 1=invisible,
    // 2=gone — map to the View constants (0/4/8) the rest of the
    // runtime uses.
    let vis_attr = attr_find(vm, h, "visibility").and_then(|v| match v {
        V::I32(2) => Some(8),
        V::I32(1) => Some(4),
        V::I32(_) => Some(0),
        V::Ref(r) => match s(vm, r).as_deref() {
            Some("invisible") => Some(4),
            Some("gone") => Some(8),
            _ => Some(0),
        },
        _ => None,
    });
    let visible = !matches!(
        pseudo_get(vm, h, P_VIS).or(vis_attr.map(V::I32)),
        Some(V::I32(4) | V::I32(8))
    );
    let listener = match pseudo_get(vm, h, P_LISTENER) {
        Some(V::Ref(l)) => l,
        _ => 0,
    };
    let gravity = match attr_find(vm, h, "gravity") {
        Some(V::I32(g)) => g,
        Some(V::Ref(r)) => s(vm, r)
            .map(|n| {
                n.split('|').fold(0, |acc, f| {
                    acc | match f.trim() {
                        "center" => 0x11,
                        "center_horizontal" => 0x1,
                        "center_vertical" => 0x10,
                        "top" => 0x30,
                        "bottom" => 0x50,
                        "left" | "start" => 0x3,
                        "right" | "end" => 0x5,
                        "fill" => 0x77,
                        _ => 0,
                    }
                })
            })
            .unwrap_or(0),
        _ => 0,
    };
    let kids: Vec<u32> = children(vm, h)
        .iter()
        .filter_map(|c| match c {
            V::Ref(ch) if *ch != 0 => Some(*ch),
            _ => None,
        })
        .collect();
    let is_group = pseudo_get(vm, h, P_IS_GROUP).is_some() || !kids.is_empty();
    let frame = match (
        pseudo_get(vm, h, P_LEFT),
        pseudo_get(vm, h, P_TOP),
        pseudo_get(vm, h, P_RIGHT),
        pseudo_get(vm, h, P_BOTTOM),
    ) {
        (Some(V::I32(l)), Some(V::I32(t)), Some(V::I32(r)), Some(V::I32(b))) => Some((l, t, r, b)),
        _ => None,
    };
    let (pad_l, pad_t, pad_r, pad_b) = view_padding(vm, h);
    let is_compound = desc.contains("CompoundButton")
        || desc.contains("CheckBox")
        || desc.contains("Checkbox")
        || desc.contains("RadioButton")
        || desc.contains("Switch")
        || vm.extends_named(&desc, "CompoundButton");
    let icon_only = text.as_deref().is_none_or(str::is_empty);
    Some(DexViewInfo {
        desc,
        is_group,
        text,
        text_size,
        text_color,
        bg_color,
        lp_w,
        lp_h,
        visible,
        listener,
        frame,
        layout_requested: layout_requested(vm, h),
        image: view_content_image(vm, h, icon_only).map(|img| tint_image(vm, h, img)),
        padding: (pad_l, pad_t, pad_r, pad_b),
        scale_type: match attr_find(vm, h, "scaleType") {
            Some(V::I32(v)) => v,
            _ => 3,
        },
        parent: match pseudo_get(vm, h, P_PARENT) {
            Some(V::Ref(p)) => p,
            _ => 0,
        },
        children: kids,
        gravity,
        clip_children: match pseudo_get(vm, h, P_CLIP_CHILDREN) {
            Some(v) => i32v(v) != 0,
            None => match attr_find(vm, h, "clipChildren") {
                // A `@bool/x` reference arrives as its res-id — a
                // nonzero I32 that must be resolved to the real bool.
                Some(V::I32(n)) if (n as u32) >= 0x7f00_0000 => {
                    attr_bool_ref(vm, n as u32).unwrap_or(true)
                }
                Some(V::I32(n)) => n != 0,
                Some(V::Ref(r)) => s(vm, r).as_deref() != Some("false"),
                _ => true,
            },
        },
        linear_vertical: linear_vertical(vm, h),
        scroll_x: pint(vm, h, P_SCROLL_X),
        scroll_y: pint(vm, h, P_SCROLL_Y),
        max_lines: match attr_find(vm, h, "maxLines").or_else(|| attr_find(vm, h, "lines")) {
            Some(V::I32(n)) if n > 0 => Some(n),
            Some(V::Ref(r)) => s(vm, r)
                .and_then(|t| t.parse::<i32>().ok())
                .filter(|n| *n > 0),
            _ => None,
        },
        compound_checked: if is_compound {
            Some(matches!(pseudo_get(vm, h, P_CHECKED), Some(V::I32(v)) if v != 0))
        } else {
            None
        },
    })
}

/// `ImageView.getDrawable()` — the stored image source normalized to a
/// Drawable-typed object: guest Drawable instances pass through; bare
/// values (Bitmap host, res-id int, Uri/path object) get wrapped in a
/// `BitmapDrawable` that also carries them for the renderer's field walk.
fn image_drawable(vm: &mut Vm, this: u32) -> V {
    let Some(raw) = pseudo_get(vm, this, P_IMAGE) else {
        // No image was ever set (icon plumbing not wired). Android
        // allows null here, but Kotlin `!!`/mutate() chains NPE on
        // it — hand back a stable placeholder drawable instead. Keep it
        // out of P_IMAGE: constructors that probe getDrawable() before
        // inflation must not suppress the XML src/srcCompat default.
        if let Some(d) = pseudo_get(vm, this, P_IMAGE_PLACEHOLDER) {
            return d;
        }
        let d = host(
            vm,
            "Landroid/graphics/drawable/BitmapDrawable;",
            HostState::Empty,
        );
        pseudo_set(vm, this, P_IMAGE_PLACEHOLDER, d);
        return d;
    };
    if is_null(raw) || raw == V::I32(0) {
        // setImage*(null)/setImageResource(0) cleared the image:
        // getDrawable must stay null so `drawable == null` checks
        // keep working.
        return V::Ref(0);
    }
    let bare = match raw {
        V::I32(_) => true,
        V::Ref(h) => match vm.heap.get(h) {
            Some(Obj::Instance { .. }) => false,
            // A previous call's BitmapDrawable is already a proper
            // drawable — re-wrapping would nest drawable-in-drawable
            // and break getBitmap().
            Some(Obj::Host { desc, .. }) => desc != "Landroid/graphics/drawable/BitmapDrawable;",
            _ => true,
        },
        _ => false,
    };
    if !bare {
        return raw;
    }
    let d = host(
        vm,
        "Landroid/graphics/drawable/BitmapDrawable;",
        HostState::Empty,
    );
    if let V::Ref(dh) = d {
        pseudo_set(vm, dh, P_BITMAP_SLOT, raw);
    }
    // Normalize the stored value so repeated getDrawable() calls return
    // the same drawable object.
    pseudo_set(vm, this, P_IMAGE, d);
    d
}

/// Resolve an `ImageView` image value to pixels: a Bitmap host, an
/// object holding a bitmap in any ref field (BitmapDrawable, Glide
/// drawables), a res-id, or a file/Uri path the runtime can open.
fn xml_image_tint(vm: &Vm, h: u32) -> Option<u32> {
    if let Some(V::I32(raw)) = attr_find(vm, h, "tint").or_else(|| attr_find(vm, h, "drawableTint"))
    {
        let u = raw as u32;
        return if raw >= 0 && (0x0101_0000..0x0102_0000).contains(&u) {
            crate::theme_color(vm, u)
        } else if raw >= 0 && u >= 0x0100_0000 {
            let is_attr = vm
                .apk_res
                .as_ref()
                .and_then(|r| r.type_name(u).map(|t| t == "attr"))
                .unwrap_or(false);
            if is_attr {
                crate::theme_color(vm, u)
            } else {
                super::vector::rctx(vm).and_then(|r| super::vector::resolve_ref_color(r, u))
            }
        } else {
            Some(u)
        };
    }
    // MaterialButton styles carry `iconTint` rather than ImageView's
    // `android:tint`; resolve it through the same style/theme-overlay path.
    let style = match attr_find(vm, h, "style") {
        Some(V::I32(style)) if style > 0 => style as u32,
        _ => return None,
    };
    let attr = vm
        .apk_res
        .as_ref()
        .and_then(|r| r.attr_id_named("iconTint"))?;
    style_attr_color(vm, style, attr).map(|c| c as u32)
}

fn tint_image(
    vm: &Vm,
    view: u32,
    image: (u32, u32, std::rc::Rc<Vec<u8>>),
) -> (u32, u32, std::rc::Rc<Vec<u8>>) {
    let Some(tint) = xml_image_tint(vm, view) else {
        return image;
    };
    let (w, h, px) = image;
    let mut out = px.as_ref().clone();
    let (tr, tg, tb) = ((tint >> 16) & 0xff, (tint >> 8) & 0xff, tint & 0xff);
    let ta = (tint >> 24) & 0xff;
    for p in out.as_chunks_mut::<4>().0 {
        // SRC_ATOP keeps the destination alpha, but the tint color only
        // covers the source in proportion to the tint alpha. A fully
        // transparent tint therefore leaves the original color intact.
        p[0] = ((tr * ta + p[0] as u32 * (255 - ta)) / 255) as u8;
        p[1] = ((tg * ta + p[1] as u32 * (255 - ta)) / 255) as u8;
        p[2] = ((tb * ta + p[2] as u32 * (255 - ta)) / 255) as u8;
    }
    (w, h, std::rc::Rc::new(out))
}

fn image_pixels(vm: &Vm, v: V) -> Option<(u32, u32, std::rc::Rc<Vec<u8>>)> {
    image_pixels_at(vm, v, 0)
}

fn image_pixels_at(vm: &Vm, v: V, depth: u32) -> Option<(u32, u32, std::rc::Rc<Vec<u8>>)> {
    // Depth bound: drawable graphs can be cyclic.
    if depth > 8 {
        return None;
    }
    match v {
        V::I32(res) if res > 0 => {
            super::vector::rctx(vm).and_then(|r| super::vector::drawable_bitmap(r, res as u32))
        }
        V::Ref(h) => super::android::bitmap_pixels(vm, v)
            .or_else(|| {
                pseudo_get(vm, h, P_BITMAP_SLOT).and_then(|bmp| image_pixels_at(vm, bmp, depth + 1))
            })
            .or_else(|| {
                let path = image_source_path(vm, v)?;
                decode_image(&std::fs::read(io::fspath(vm, &path)).ok()?)
            })
            .or_else(|| match vm.heap.get(h) {
                Some(Obj::Instance { fields, .. }) | Some(Obj::Host { fields, .. }) => fields
                    .values()
                    .find_map(|f| image_pixels_at(vm, *f, depth + 1)),
                // LayerDrawable/TransitionDrawable keep their drawables
                // in an array field — the topmost (last) layer wins: a
                // TransitionDrawable's "to" drawable is the real image,
                // layer[0] is a (often blank) placeholder.
                Some(Obj::Array { data, .. }) => data
                    .iter()
                    .rev()
                    .find_map(|f| image_pixels_at(vm, *f, depth + 1)),
                _ => None,
            }),
        _ => None,
    }
}

/// The guest filesystem path behind a Uri/File/string image source:
/// `file:///x`, `content://media/.../<id>` (via the media id map) or a
/// raw path. None for non-path objects (Bitmap/Drawable instances).
fn image_source_path(vm: &Vm, v: V) -> Option<String> {
    let raw = match v {
        V::Ref(h) => match vm.heap.get(h) {
            Some(Obj::Str(s)) => s.clone(),
            Some(Obj::Host {
                state: HostState::StrBuf(s) | HostState::Path(s) | HostState::Regex(s),
                ..
            }) => s.clone(),
            _ => return None,
        },
        _ => return None,
    };
    super::android::media_uri_path(vm, &raw)
}

/// RGBA8 decode of packaged/encoded image bytes (PNG/JPEG).
fn decode_image(bytes: &[u8]) -> Option<(u32, u32, std::rc::Rc<Vec<u8>>)> {
    let img = image::load_from_memory(bytes).ok()?;
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    Some((w, h, std::rc::Rc::new(rgba.into_raw())))
}

/// Whether `class`'s methods route here. Any class may produce a view
/// receiver (dex views resolve inherited methods to `Landroid/view/View;`),
/// so the receiver's pseudo markers decide for unknown classes.
pub(crate) fn is_viewish(vm: &Vm, class: &str, args: &[V]) -> bool {
    if matches!(
        class,
        "Landroid/view/View;"
            | "Landroid/view/ViewGroup;"
            | "Landroid/view/Window;"
            | "Landroid/view/LayoutInflater;"
            | "Landroid/view/MenuInflater;"
            | "Landroid/view/Menu;"
            | "Landroid/view/SubMenu;"
            | "Landroid/view/MenuItem;"
            | "Landroidx/appcompat/view/SupportMenuInflater;"
            | "Landroidx/appcompat/view/menu/MenuBuilder;"
            | "Landroidx/appcompat/view/menu/MenuItemImpl;"
            | "Landroidx/appcompat/widget/ListPopupWindow;"
            | "Landroid/view/ViewTreeObserver;"
            | "Landroid/view/MotionEvent;"
            | "Landroid/view/KeyEvent;"
            | "Landroid/view/WindowManager;"
            | "Landroid/view/SurfaceHolder;"
            | "Landroid/view/WindowMetrics;"
            | "Landroid/view/WindowInsets;"
            | "Landroid/view/Choreographer;"
            | "Landroid/graphics/Insets;"
    ) || class.ends_with("LayoutParams;")
        || desc_is_a(class, VIEW)
        || class.ends_with("$DecorView;")
        // Dex View subclasses (ConstraintLayout, appcompat Toolbar):
        // static calls like the inherited
        // `ViewGroup.getChildMeasureSpec` carry an int receiver slot,
        // so the args check below can't see them — walk the dex
        // superclass chain instead.
        || vm
            .type_index_of(class)
            .zip(vm.type_index_of("Landroid/view/View;"))
            .map(|(ci, v)| vm.assignable(ci, v))
            .unwrap_or(false)
    {
        return true;
    }
    match args.first() {
        Some(V::Ref(h)) if *h != 0 => pseudo_get(vm, *h, P_IS_VIEW).is_some(),
        _ => false,
    }
}

pub(crate) fn call(
    vm: &mut Vm,
    class: &str,
    name: &str,
    sig: &str,
    args: &[V],
) -> Rt<Option<Option<V>>> {
    let this = match args.first() {
        Some(V::Ref(h)) => *h,
        _ => 0,
    };
    Ok(match name {
        "onClick" if pseudo_get(vm, this, P_POPUP_OWNER).is_some() => {
            let popup = match pseudo_get(vm, this, P_POPUP_OWNER) {
                Some(V::Ref(h)) => h,
                _ => 0,
            };
            let pos = match pseudo_get(vm, this, P_POPUP_POS) {
                Some(V::I32(i)) => i,
                _ => 0,
            };
            let root = match pseudo_get(vm, popup, P_POPUP_ROOT) {
                Some(v) => v,
                _ => V::Ref(0),
            };
            let row = args.get(1).copied().unwrap_or(V::Ref(0));
            if let V::Ref(listener) = lpw_field(vm, popup, "listener") {
                if listener != 0 {
                    let _ = vm.call_virtual(
                        listener,
                        "onItemClick",
                        &[root, row, V::I32(pos), V::I64(pos as i64)],
                    );
                }
            }
            Some(None)
        }
        // ---------- inflation ----------
        "<init>" if class.ends_with("LayoutParams;") => {
            lp_ctor(vm, class, sig, args);
            Some(None)
        }
        // ---------- MotionEvent ----------
        "<init>" if class == "Landroid/view/MotionEvent;" || class == "Landroid/view/KeyEvent;" => {
            Some(None)
        }
        // ---------- KeyEvent ----------
        "getAction" if class == "Landroid/view/KeyEvent;" => {
            Some(Some(V::I32(pint(vm, this, P_KE_ACTION))))
        }
        "getKeyCode" if class == "Landroid/view/KeyEvent;" => {
            Some(Some(V::I32(pint(vm, this, P_KE_CODE))))
        }
        "getUnicodeChar" if class == "Landroid/view/KeyEvent;" => {
            Some(Some(V::I32(pint(vm, this, P_KE_UNICODE))))
        }
        "getRepeatCount" if class == "Landroid/view/KeyEvent;" => {
            Some(Some(V::I32(pint(vm, this, P_KE_REPEAT))))
        }
        "getDownTime" | "getEventTime" if class == "Landroid/view/KeyEvent;" => {
            Some(Some(V::I64(now_ms())))
        }
        "getMetaState" | "getModifiers" | "getScanCode" | "getDeviceId"
            if class == "Landroid/view/KeyEvent;" =>
        {
            Some(Some(V::I32(0)))
        }
        "isShiftPressed" | "isCtrlPressed" | "isAltPressed" | "isMetaPressed"
            if class == "Landroid/view/KeyEvent;" =>
        {
            Some(Some(V::I32(0)))
        }
        "recycle" if class == "Landroid/view/KeyEvent;" => Some(None),
        "obtain" if class == "Landroid/view/MotionEvent;" => {
            let V::Ref(h) = host(vm, "Landroid/view/MotionEvent;", HostState::Empty) else {
                return Ok(Some(Some(V::Ref(0))));
            };
            if h != 0 {
                // Copy-overload (obtain(MotionEvent)) inherits the
                // source's fields; the canonical (JJIFF…) overload
                // takes them positionally.
                if let Some(V::Ref(src)) = args
                    .iter()
                    .find(|a| matches!(a, V::Ref(h) if *h != 0))
                    .copied()
                {
                    for k in [
                        P_ME_ACTION,
                        P_ME_X,
                        P_ME_Y,
                        P_ME_DOWN_TIME,
                        P_ME_EVENT_TIME,
                        P_ME_RAWX,
                        P_ME_RAWY,
                    ] {
                        if let Some(v) = pseudo_get(vm, src, k) {
                            pseudo_set(vm, h, k, v);
                        }
                    }
                } else {
                    if let Some(V::I64(t)) = args.first().copied() {
                        pseudo_set(vm, h, P_ME_DOWN_TIME, V::I64(t));
                    }
                    if let Some(V::I64(t)) = args.get(1).copied() {
                        pseudo_set(vm, h, P_ME_EVENT_TIME, V::I64(t));
                    }
                    if let Some(V::I32(a)) = args.get(2).copied() {
                        pseudo_set(vm, h, P_ME_ACTION, V::I32(a));
                    }
                    if let Some(V::F32(x)) = args.get(3).copied() {
                        pseudo_set(vm, h, P_ME_X, V::F32(x));
                    }
                    if let Some(V::F32(y)) = args.get(4).copied() {
                        pseudo_set(vm, h, P_ME_Y, V::F32(y));
                    }
                }
            }
            Some(Some(V::Ref(h)))
        }
        "getAction" | "getActionMasked" if class == "Landroid/view/MotionEvent;" => {
            Some(Some(V::I32(pint(vm, this, P_ME_ACTION))))
        }
        "getX" if class == "Landroid/view/MotionEvent;" => {
            Some(Some(pseudo_get(vm, this, P_ME_X).unwrap_or(V::F32(0.0))))
        }
        "getY" if class == "Landroid/view/MotionEvent;" => {
            Some(Some(pseudo_get(vm, this, P_ME_Y).unwrap_or(V::F32(0.0))))
        }
        "getRawX" if class == "Landroid/view/MotionEvent;" => Some(Some(
            pseudo_get(vm, this, P_ME_RAWX)
                .or_else(|| pseudo_get(vm, this, P_ME_X))
                .unwrap_or(V::F32(0.0)),
        )),
        "getRawY" if class == "Landroid/view/MotionEvent;" => Some(Some(
            pseudo_get(vm, this, P_ME_RAWY)
                .or_else(|| pseudo_get(vm, this, P_ME_Y))
                .unwrap_or(V::F32(0.0)),
        )),
        "getPointerCount" if class == "Landroid/view/MotionEvent;" => Some(Some(V::I32(1))),
        "getPointerId" | "findPointerIndex" | "getActionIndex"
            if class == "Landroid/view/MotionEvent;" =>
        {
            Some(Some(V::I32(0)))
        }
        "getToolType" if class == "Landroid/view/MotionEvent;" => Some(Some(V::I32(1))),
        "getSource" if class == "Landroid/view/MotionEvent;" => Some(Some(V::I32(0x1002))),
        "getButtonState" | "getHistorySize" | "getMetaState"
            if class == "Landroid/view/MotionEvent;" =>
        {
            Some(Some(V::I32(0)))
        }
        "getPressure" | "getSize" | "getTouchMajor" | "getTouchMinor"
            if class == "Landroid/view/MotionEvent;" =>
        {
            Some(Some(V::F32(if name == "getPressure" { 1.0 } else { 0.0 })))
        }
        "getDownTime" if class == "Landroid/view/MotionEvent;" => Some(Some(
            pseudo_get(vm, this, P_ME_DOWN_TIME).unwrap_or(V::I64(now_ms())),
        )),
        "getEventTime" if class == "Landroid/view/MotionEvent;" => Some(Some(
            pseudo_get(vm, this, P_ME_EVENT_TIME).unwrap_or(V::I64(now_ms())),
        )),
        "setLocation" if class == "Landroid/view/MotionEvent;" => {
            pseudo_set(
                vm,
                this,
                P_ME_X,
                args.get(1).copied().unwrap_or(V::F32(0.0)),
            );
            pseudo_set(
                vm,
                this,
                P_ME_Y,
                args.get(2).copied().unwrap_or(V::F32(0.0)),
            );
            Some(None)
        }
        "offsetLocation" if class == "Landroid/view/MotionEvent;" => {
            let x = f64v(pseudo_get(vm, this, P_ME_X).unwrap_or(V::F32(0.0)))
                + f64v(args.get(1).copied().unwrap_or(V::F32(0.0)));
            let y = f64v(pseudo_get(vm, this, P_ME_Y).unwrap_or(V::F32(0.0)))
                + f64v(args.get(2).copied().unwrap_or(V::F32(0.0)));
            pseudo_set(vm, this, P_ME_X, V::F32(x as f32));
            pseudo_set(vm, this, P_ME_Y, V::F32(y as f32));
            Some(None)
        }
        "recycle" if class == "Landroid/view/MotionEvent;" => Some(None),
        // Dex-constructed views (`new PlayerFragment(ctx, attrs)` in an
        // adapter) never see the inflater: their `View.<init>` invoke-super
        // is the only place the Context reaches us. Record it so
        // `getContext` returns the real Activity — `context as? Activity`
        // (fossify's runOnUiThread hop) fails on the stub Context.
        "<init>" if this != 0 => {
            let is_view = pseudo_get(vm, this, P_IS_VIEW).is_some()
                || vm.assignable_desc(&class_of(vm, V::Ref(this)), VIEW);
            if is_view {
                if let Some(V::Ref(c)) = args.get(1).copied() {
                    if c != 0 {
                        pseudo_set(vm, this, P_CONTEXT, V::Ref(c));
                    }
                }
                if let Some(V::Ref(attrs)) = args.get(2).copied() {
                    if attrs != 0 && pseudo_get(vm, this, P_ATTRS).is_none() {
                        pseudo_set(vm, this, P_ATTRS, V::Ref(attrs));
                    }
                }
                apply_ctor_style(vm, this, args);
            }
            Some(None)
        }
        // ---------- Choreographer ----------
        // The Recomposer's frame clock (MonotonicFrameClock on the main
        // dispatcher) and `View.postOnAnimation` ride postFrameCallback —
        // stubbing it left the first composition permanently unscheduled.
        "getInstance" if class == CHOR => Some(Some(choreographer(vm))),
        "postFrameCallback" | "postFrameCallbackDelayed" | "postVsyncCallback" if class == CHOR => {
            // (FrameCallback [, long delayMillis])
            // Choreographer callbacks wait for a vsync; they are not
            // immediate main-queue posts. Without a minimum frame
            // interval a re-posting Compose animation executes back-to-
            // back on this cooperative VM and starves background work.
            let delay = if name == "postFrameCallback" {
                16
            } else {
                args.get(2).map(|v| i64v(*v)).unwrap_or(0).max(16)
            };
            if let Some(V::Ref(cb)) = args.get(1).copied() {
                if cb != 0 {
                    if crate::dbg_flag("OVELO_DRAINDBG") {
                        eprintln!("[drain] push framecb h{cb} delay={delay}");
                    }
                    vm.frame_callbacks
                        .push((cb, (now_ms() + delay.max(0)) as u64));
                }
            }
            Some(None)
        }
        "postCallback" | "postCallbackDelayed" | "postCallbackDelayedInternal" if class == CHOR => {
            // (int callbackType, Runnable action, Object token [, long delay])
            let delay = if args.len() > 4 { i64v(args[4]) } else { 0 };
            if let Some(V::Ref(r)) = args.get(2).copied() {
                if r != 0 {
                    if delay > 0 {
                        vm.enqueue_runnable_delayed(r, true, delay);
                    } else {
                        vm.enqueue_runnable(r, true);
                    }
                }
            }
            Some(None)
        }
        "removeFrameCallback" | "removeCallbacks" if class == CHOR => {
            for v in [args.get(1), args.get(2)].into_iter().flatten() {
                if let V::Ref(r) = v {
                    if *r != 0 {
                        vm.remove_runnable(*r);
                    }
                }
            }
            Some(None)
        }
        "getFrameTime"
        | "getLastFrameTimeNanos"
        | "getFrameTimeNanos"
        | "getFrameDeadlineNanos"
            if class == CHOR =>
        {
            let t = if name == "getFrameTime" {
                now_ms()
            } else {
                now_ms() * 1_000_000
            };
            Some(Some(V::I64(t)))
        }
        "isNewPostCallbackOnMainThread" if class == CHOR => Some(Some(V::I32(1))),
        // AOSP `MarginLayoutParams.resolveLayoutDirection` — the
        // ConstraintLayout dex override resolves the start/end anchors
        // into its resolved* fields and invoke-supers here for the
        // margin fold. Store the direction (a pseudo — `mLayoutDirection`
        // has no dex field ref) and fold relative margins into left/right.
        "resolveLayoutDirection" if class.ends_with("LayoutParams;") => {
            lp_resolve_dir(vm, this, i32v(args.get(1).copied().unwrap_or(V::I32(0))));
            Some(None)
        }
        "getLayoutDirection" if class.ends_with("LayoutParams;") => {
            Some(Some(pseudo_get(vm, this, P_LP_DIR).unwrap_or(V::I32(0))))
        }
        "inflate" if class.contains("MenuInflater") => {
            // MenuInflater.inflate(menuRes, Menu)
            if let Some(V::Ref(mh)) = args.get(2).copied() {
                menu_inflate(vm, args.get(1).copied(), mh);
            }
            Some(None)
        }
        "inflate" => Some(Some(inflate_call(vm, args))),
        "from" if class == INFLATER || class == "Landroid/view/LayoutInflater$Factory2;" => {
            Some(Some(inflater(vm, args.first().copied())))
        }
        "createView" | "onCreateView" | "onCreateViewWithParent" => {
            // (String name, Context, AttributeSet [, View parent])
            let name = args
                .iter()
                .find_map(|a| sv(vm, *a))
                .unwrap_or_else(|| "View".to_string());
            let attrs = args
                .iter()
                .skip(1)
                .find(|a| matches!(a, V::Ref(h) if *h != 0))
                .copied()
                .unwrap_or(V::Ref(0));
            Some(Some(make_view_named(vm, &name, attrs, V::Ref(0))))
        }
        "cloneInContext" => Some(Some(inflater(vm, args.get(1).copied()))),
        // ---------- menu ----------
        "getMenuInflater" | "getSupportMenuInflater" => Some(Some(host(
            vm,
            "Landroid/view/MenuInflater;",
            HostState::Empty,
        ))),
        // Static View/ViewGroup.getChildMeasureSpec(parentSpec,
        // padding, childDimension) — AOSP packing rules.
        "getChildMeasureSpec" => Some(Some(V::I32(child_measure_spec(
            i32v(args.first().copied().unwrap_or(V::I32(0))),
            i32v(args.get(1).copied().unwrap_or(V::I32(0))),
            i32v(args.get(2).copied().unwrap_or(V::I32(0))),
        )))),
        // Static View size resolution — every dex container's
        // onMeasure folds its content size against the spec through
        // these; stubs returning null collapse the whole tree to 0×0.
        "getDefaultSize" | "resolveSize" => Some(Some(V::I32(resolve_size(
            i32v(args.first().copied().unwrap_or(V::I32(0))),
            i32v(args.get(1).copied().unwrap_or(V::I32(0))),
            name == "resolveSize",
        )))),
        "resolveSizeAndState" => {
            let size = i32v(args.first().copied().unwrap_or(V::I32(0)));
            let spec = i32v(args.get(1).copied().unwrap_or(V::I32(0)));
            let state = i32v(args.get(2).copied().unwrap_or(V::I32(0)));
            const AT_MOST: i32 = 2 << 30;
            const MODE_MASK: i32 = 0x3 << 30;
            const TOO_SMALL: i32 = 0x0100_0000;
            const STATE_MASK: i32 = 0xFF00_0000_u32 as i32;
            let spec_size = spec & !MODE_MASK;
            let result = match spec & MODE_MASK {
                m if m == AT_MOST && spec_size < size => spec_size | TOO_SMALL,
                m if m == AT_MOST => size,
                m if m == (1 << 30) => spec_size, // EXACTLY
                _ => size,
            };
            Some(Some(V::I32(result | (state & STATE_MASK))))
        }
        "combineMeasuredStates" => Some(Some(V::I32(
            i32v(args.first().copied().unwrap_or(V::I32(0)))
                | i32v(args.get(1).copied().unwrap_or(V::I32(0))),
        ))),
        // ViewGroup.measureChild / measureChildWithMargins — the path
        // dex parents use to measure each child. Without it guest
        // onMeasure loops run but children never get measured, so the
        // whole tree stays at 0×0.
        "measureChild" | "measureChildWithMargins" => {
            let child = match args.get(1) {
                Some(V::Ref(h)) if *h != 0 => *h,
                _ => return Ok(Some(None)),
            };
            let margins = name == "measureChildWithMargins";
            let (pw, ph, w_used, h_used) = if margins {
                (
                    i32v(args.get(2).copied().unwrap_or(V::I32(0))),
                    i32v(args.get(4).copied().unwrap_or(V::I32(0))),
                    i32v(args.get(3).copied().unwrap_or(V::I32(0))),
                    i32v(args.get(5).copied().unwrap_or(V::I32(0))),
                )
            } else {
                (
                    i32v(args.get(2).copied().unwrap_or(V::I32(0))),
                    i32v(args.get(3).copied().unwrap_or(V::I32(0))),
                    0,
                    0,
                )
            };
            measure_child(vm, this, child, pw, w_used, ph, h_used, margins);
            Some(None)
        }
        "getMenu" => Some(Some(menu_of(vm, this))),
        // Toolbar.inflateMenu(int) / NavigationView.inflateMenu(int).
        "inflateMenu" => {
            let m = menu_of(vm, this);
            if let V::Ref(mh) = m {
                menu_inflate(vm, args.get(1).copied(), mh);
            }
            Some(Some(V::Ref(0)))
        }
        "findItem" => Some(Some(menu_find(vm, this, args.get(1).copied()))),
        "add" | "addSubMenu" if is_menuish(vm, this) => {
            // Two arities: add(title|titleRes) and
            // add(groupId, itemId, order, title|titleRes). Duplicate
            // ids are legal in AOSP, so add always appends; findItem
            // walks the list for the first match.
            let (group, id, title) = if args.len() >= 5 {
                (
                    i64v(args[1]) as i32,
                    i64v(args[2]) as i32,
                    args.get(4).copied(),
                )
            } else {
                (0, 0, args.get(1).copied())
            };
            let it = if name == "addSubMenu" {
                menu_sub_add(vm, this, group, id)
            } else {
                menu_item_add(vm, this, group, id)
            };
            if let (Some(t), V::Ref(ih)) = (title, it) {
                let t = menu_title(vm, t);
                // Title lives on the representing item (the SubMenu's
                // header item for addSubMenu).
                let ih = match pseudo_get(vm, ih, P_SUBMENU_ITEM) {
                    Some(V::Ref(h)) => h,
                    _ => ih,
                };
                pseudo_set(vm, ih, P_TEXT, t);
            }
            Some(Some(it))
        }
        "size" if is_menuish(vm, this) => Some(Some(V::I32(menu_items(vm, this).len() as i32))),
        "getItem" if is_menuish(vm, this) => {
            if args.len() > 1 {
                let i = i64v(args[1]) as usize;
                let items = menu_items(vm, this);
                Some(Some(items.get(i).copied().unwrap_or(V::Ref(0))))
            } else {
                // SubMenu.getItem() — the item representing it in the
                // parent menu.
                Some(Some(
                    pseudo_get(vm, this, P_SUBMENU_ITEM).unwrap_or(V::Ref(0)),
                ))
            }
        }
        "clear" | "removeGroup" | "removeItem" if is_menuish(vm, this) => {
            let keep: Vec<(V, V)> = match name {
                "clear" => Vec::new(),
                "removeItem" => {
                    // AOSP removes only the first matching item.
                    let id = i64v(args.get(1).copied().unwrap_or(V::I32(0))) as i32;
                    let mut items = menu_items_paired(vm, this);
                    if let Some(pos) = items.iter().position(|(k, _)| *k == id) {
                        items.remove(pos);
                    }
                    items.into_iter().map(|(k, v)| (V::I32(k), v)).collect()
                }
                _ => {
                    // removeGroup: drop items whose stored group id matches.
                    let gid = i64v(args.get(1).copied().unwrap_or(V::I32(0))) as i32;
                    menu_items_paired(vm, this)
                        .into_iter()
                        .filter(|(_, v)| {
                            !matches!(v, V::Ref(ih) if pseudo_get(vm, *ih, P_MENU_GROUP) == Some(V::I32(gid)))
                        })
                        .map(|(k, v)| (V::I32(k), v))
                        .collect()
                }
            };
            if let Some(Obj::Host {
                state: HostState::Map(items),
                ..
            }) = vm.heap.get_mut(this)
            {
                *items = keep;
            }
            Some(None)
        }
        "setVisible"
        | "setEnabled"
        | "setChecked"
        | "setCheckable"
        | "setTitle"
        | "setTitleCondensed"
        | "setIcon"
        | "setShowAsAction"
        | "setShowAsActionFlags"
        | "setActionView"
        | "setIntent"
        | "setAlphabeticShortcut"
        | "setNumericShortcut"
        | "setTooltipText"
        | "setContentDescription"
        | "setGroupVisible"
            if is_menuitem(vm, this) =>
        {
            let key = match name {
                "setVisible" => P_MENU_VIS,
                "setEnabled" => P_MENU_EN,
                "setCheckable" => P_MENU_CKABLE,
                "setChecked" => P_MENU_CHK,
                "setTitle" | "setTitleCondensed" => P_TEXT,
                "setActionView" => P_MENU_ACTION_VIEW,
                _ => P_TAG,
            };
            let v = args.get(1).copied().unwrap_or(V::Ref(0));
            let v = if key == P_TEXT { menu_title(vm, v) } else { v };
            pseudo_set(vm, this, key, v);
            // AOSP item mutators return the item for chaining.
            Some(Some(V::Ref(this)))
        }
        "setOnMenuItemClickListener" | "setOnActionExpandListener" if is_menuitem(vm, this) => {
            pseudo_set(
                vm,
                this,
                P_LISTENER,
                args.get(1).copied().unwrap_or(V::Ref(0)),
            );
            Some(Some(V::Ref(this)))
        }
        "getItemId" if is_menuitem(vm, this) => {
            Some(Some(pseudo_get(vm, this, P_MENU_ID).unwrap_or(V::I32(0))))
        }
        "getGroupId" if is_menuitem(vm, this) => Some(Some(
            pseudo_get(vm, this, P_MENU_GROUP).unwrap_or(V::I32(0)),
        )),
        "isVisible" | "isEnabled" | "isChecked" | "isCheckable" if is_menuitem(vm, this) => {
            let (key, default) = match name {
                "isEnabled" => (P_MENU_EN, 1),
                "isCheckable" => (P_MENU_CKABLE, 1),
                "isChecked" => (P_MENU_CHK, 0),
                _ => (P_MENU_VIS, 1),
            };
            let v = pseudo_get(vm, this, key).unwrap_or(V::I32(default));
            Some(Some(V::I32(i64v(v) as i32)))
        }
        "getTitle" | "getTitleCondensed" if is_menuitem(vm, this) => {
            let v = pseudo_get(vm, this, P_TEXT).unwrap_or(V::Ref(0));
            let v = if matches!(v, V::I32(_)) {
                menu_title(vm, v)
            } else {
                v
            };
            Some(Some(if matches!(v, V::Ref(0)) {
                jstr(vm, "")
            } else {
                v
            }))
        }
        "hasSubMenu" if is_menuitem(vm, this) => Some(Some(V::I32(
            if pseudo_get(vm, this, P_ITEM_SUBMENU).is_some() {
                1
            } else {
                0
            },
        ))),
        "getSubMenu" if is_menuitem(vm, this) => Some(Some(
            pseudo_get(vm, this, P_ITEM_SUBMENU).unwrap_or(V::Ref(0)),
        )),
        "getActionView" if is_menuitem(vm, this) => {
            Some(Some(match pseudo_get(vm, this, P_MENU_ACTION_VIEW) {
                Some(v @ V::Ref(_)) => v,
                _ => V::Ref(0),
            }))
        }
        "isActionViewExpanded" | "expandActionView" | "collapseActionView"
            if is_menuitem(vm, this) =>
        {
            Some(Some(V::I32(0)))
        }
        "getContext" if class == INFLATER || is_marked(vm, this) => Some(Some(
            pseudo_get(vm, this, P_CONTEXT).unwrap_or_else(|| stub_ctx(vm)),
        )),
        "getFactory" | "getFactory2" | "getFilter" => Some(Some(V::Ref(0))),
        "setFactory" | "setFactory2" | "setFilter" => Some(None),

        // ---------- tree ----------
        "findViewById" | "requireViewById" => {
            let id = i32v(args.get(1).copied().unwrap_or(V::I32(0)));
            let root = if class == WINDOW {
                decor_of(vm, this).unwrap_or(0)
            } else {
                this
            };
            // AOSP: `View.findViewById` searches this view's subtree
            // only. Generated ViewBinding `bind` implementations rely
            // on that — they iterate a merged root's children calling
            // `child.findViewById(id)` and treat a miss as "try the
            // next child". Widening a miss to the whole window hands
            // back a view from a *different* page (NewPipe's feed
            // binding bound its adapter onto the kiosk page's
            // RecyclerView), so no top-of-tree retry here.
            let mut hit = find_in_tree(vm, root, id).unwrap_or(0);
            if hit == 0 && class == WINDOW {
                // Window searches span trees: appcompat content lives
                // in its own decor tree, so retry across inflated roots
                // (plain View.findViewById stays inside its own tree).
                for i in (0..vm.view_roots.len()).rev() {
                    let r = vm.view_roots[i];
                    if r == root {
                        continue;
                    }
                    if let Some(h) = find_in_tree(vm, r, id) {
                        hit = h;
                        break;
                    }
                }
            }
            if hit == 0 && crate::dbg_flag("OVELO_VIEWDBG") {
                let nm = vm
                    .apk_res
                    .as_ref()
                    .and_then(|r| r.entry_name(id as u32))
                    .unwrap_or("?");
                let kids: Vec<String> = children(vm, root)
                    .iter()
                    .map(|c| match c {
                        V::Ref(ch) => format!("h{ch}#{:x}", view_id(vm, *ch)),
                        _ => "?".into(),
                    })
                    .collect();
                eprintln!(
                    "[viewdbg] findViewById({id:#x}={nm}) miss on {class} h{root} kids={kids:?}"
                );
            }
            Some(Some(V::Ref(hit)))
        }
        "getId" => Some(Some(V::I32(view_id(vm, this)))),
        "generateViewId" => Some(Some(V::I32(generate_view_id()))),
        "setId" => {
            pseudo_set(
                vm,
                this,
                P_VIEW_ID,
                args.get(1).copied().unwrap_or(V::I32(0)),
            );
            Some(None)
        }
        // Framework-stub ViewGroups have no dex `generateLayoutParams`:
        // fabricate a host LP whose `width`/`height`/margin fields are
        // parsed from the child's attrs by lp_ctor — otherwise inflated
        // children carry an empty LP and `iget lp.width` reads 0 forever
        // (Glide's SizeDeterminer waits on it).
        "generateLayoutParams" => {
            let arg = args
                .get(1)
                .copied()
                .filter(|a| matches!(a, V::Ref(h) if *h != 0));
            let v = match arg {
                // AOSP: a null/nullable arg defers to
                // generateDefaultLayoutParams — never inherit the
                // parent's own attrs (the child would copy e.g. a
                // 0dp layout_height and vanish).
                None => default_lp(vm, this),
                Some(a) => {
                    let lp = lp_class_for(vm, this);
                    let h = vm.alloc_host(&lp, HostState::Empty);
                    if class_of(vm, a).contains("LayoutParams") {
                        // generateLayoutParams(LayoutParams): copy ctor.
                        lp_ctor(
                            vm,
                            &lp,
                            "(Landroid/view/ViewGroup$LayoutParams;)V",
                            &[V::Ref(h), a],
                        );
                    } else {
                        let ctx = pseudo_get(vm, this, P_CONTEXT).unwrap_or(V::Ref(0));
                        lp_ctor(
                            vm,
                            &lp,
                            "(Landroid/content/Context;Landroid/util/AttributeSet;)V",
                            &[V::Ref(h), ctx, a],
                        );
                    }
                    V::Ref(h)
                }
            };
            Some(Some(v))
        }
        "generateDefaultLayoutParams" => Some(Some(default_lp(vm, this))),
        // Framework groups refine ViewGroup.checkLayoutParams to their
        // own LP type (LinearLayout.LayoutParams, FrameLayout.LayoutParams,
        // …). Use the receiver's LP class rather than a bare null check:
        // addView relies on a false result to convert foreign params, while
        // dex groups such as CoordinatorLayout still reach here only after
        // their own instanceof check has accepted the object.
        "checkLayoutParams" => {
            let ok = match args.get(1).copied() {
                Some(v @ V::Ref(lph)) if lph != 0 => {
                    // An invoke-super into ViewGroup has already passed the
                    // dex override's own typed check; ViewGroup itself only
                    // null-checks. Re-checking the canonical framework LP
                    // name rejects R8-renamed inner LP classes
                    // (CoordinatorLayout's `Lej0`) and addView then copies
                    // the params, dropping their behavior. A dex subclass
                    // that supers into FrameLayout still carries that owner,
                    // so it keeps the refined type check below.
                    if class == "Landroid/view/ViewGroup;"
                        && vm.has_dex_virtual(
                            this,
                            "checkLayoutParams",
                            &["Landroid/view/ViewGroup$LayoutParams;"],
                        )
                    {
                        true
                    } else {
                        let got = class_of(vm, v);
                        let want = lp_class_for(vm, this);
                        got == want || desc_is_a(&got, &want) || vm.assignable_desc(&got, &want)
                    }
                }
                _ => false,
            };
            Some(Some(V::I32(ok as i32)))
        }
        "getChildCount" => {
            let n = children(vm, this).len();
            if crate::dbg_flag("OVELO_VIEWDBG") {
                let d = class_of(vm, V::Ref(this));
                eprintln!("[viewdbg] getChildCount {d} h{this} -> {n}");
            }
            Some(Some(V::I32(n as i32)))
        }
        "getChildAt" => {
            let got = children(vm, this)
                .get(i32v(args.get(1).copied().unwrap_or(V::I32(0))) as usize)
                .copied()
                .unwrap_or(V::Ref(0));
            if crate::dbg_flag("OVELO_VIEWDBG") {
                let d = class_of(vm, V::Ref(this));
                let cd = class_of(vm, got);
                eprintln!(
                    "[viewdbg] getChildAt {d} h{this}[{}] -> {cd}",
                    i32v(args.get(1).copied().unwrap_or(V::I32(0)))
                );
            }
            Some(Some(got))
        }
        "indexOfChild" => {
            let target = args.get(1).copied().unwrap_or(V::Ref(0));
            let i = children(vm, this)
                .iter()
                .position(|&c| value_eq(vm, c, target))
                .map(|i| i as i32)
                .unwrap_or(-1);
            Some(Some(V::I32(i)))
        }
        "detachViewFromParent" => {
            match args.get(1).copied() {
                Some(child @ V::Ref(_)) => detach(vm, this, child, false),
                Some(V::I32(i)) if i >= 0 => {
                    let child = children(vm, this).get(i as usize).copied();
                    if let Some(c) = child {
                        detach(vm, this, c, false);
                    }
                }
                _ => {}
            }
            Some(None)
        }
        "addView"
        | "addViewInLayout"
        | "attachViewToParent"
        | "attachViewToParentInLayout"
        | "addFocusables"
        | "addChildrenForAccessibility" => {
            if let Some(&child @ V::Ref(_)) = args.get(1) {
                if crate::dbg_flag("OVELO_VIEWDBG") {
                    if let V::Ref(ch) = child {
                        eprintln!(
                            "[viewdbg] {name} h{this} <- h{ch} {} (ref {class})",
                            vm.dbg_class_of(ch)
                        );
                    }
                }
                // Positional overloads (addView(v,i), attachViewToParent
                // (v,i,lp), addViewInLayout(v,i,lp)) carry a child index;
                // addView(v,w,h)'s ints are dimensions, not a position.
                // Exactly one int param ⇒ positional.
                let params = sig
                    .split('(')
                    .nth(1)
                    .and_then(|p| p.split(')').next())
                    .unwrap_or("");
                let mut ints = 0usize;
                let mut has_ref = false;
                let mut it = params.bytes().peekable();
                while let Some(b) = it.next() {
                    match b {
                        b'L' | b'[' => {
                            has_ref = true;
                            // Object/array types swallow their name.
                            if b == b'L' {
                                for c in it.by_ref() {
                                    if c == b';' {
                                        break;
                                    }
                                }
                            }
                        }
                        b'I' => ints += 1,
                        _ => {}
                    }
                }
                // The index is the int right after the child — except
                // (View,w,h) whose two ints carry no LayoutParams. A
                // (V,I,LP,Z) overload's trailing boolean isn't an index.
                let idx = if !has_ref && ints >= 2 {
                    None
                } else {
                    match args.get(2) {
                        Some(V::I32(i)) if *i >= 0 && ints >= 1 => Some(*i as usize),
                        _ => None,
                    }
                };
                let notify = !matches!(name, "attachViewToParent" | "attachViewToParentInLayout");
                // A registered bridge impl (`ViewGroup.addView` on the
                // GuestUi side) wires the child into the mounted host
                // tree when both ends are bridged `new` views — the
                // dex bookkeeping below alone leaves the real group's
                // children empty (andstatus 2048's FrameLayout).
                // The bridge is registered for the canonical
                // `addView(View)` sig only — every overload ends in
                // `addViewInner(child)` with the child at args[1], so
                // fall back to it when the exact proto isn't bridged.
                let host = if matches!(name, "addView" | "addViewInLayout" | "attachViewToParent") {
                    // RecyclerView attaches its *visible* rows through
                    // attachViewToParent too — leaving it dex-only
                    // meant bridged groups rendered empty lists.
                    vm.named_host(class, name, sig)
                        .or_else(|| vm.named_host(class, name, "(Landroid/view/View;)V"))
                } else {
                    // attachViewToParentInLayout is transient
                    // (RecyclerView scrap): dex bookkeeping only.
                    None
                };
                let mut host_ran = false;
                if let Some(f) = host {
                    if crate::dbg_flag("OVELO_VIEWDBG") {
                        eprintln!("[viewdbg] addView host-arm h{this} (ref {class})");
                    }
                    if let Ok(mut g) = f.try_borrow_mut() {
                        let _ = g(vm, args);
                        host_ran = true;
                    }
                } else if crate::dbg_flag("OVELO_VIEWDBG") {
                    eprintln!("[viewdbg] addView host-miss h{this} (ref {class}) sig={sig}");
                }
                // attachViewToParent(v,i,lp) / addView(v,lp) /
                // addViewInLayout(v,i,lp) hand a fully-built dex
                // LayoutParams — keep it on the child or later dex
                // reads (`iget lp.mDecorInsets`) hit a host
                // placeholder and see null fields.
                let lp = args[2..]
                    .iter()
                    .rev()
                    .find(|a| matches!(a, V::Ref(h) if *h != 0))
                    .copied();
                // A host bridge that ran already did the attach
                // bookkeeping itself — attaching again would fire
                // onViewAdded twice.
                if !host_ran {
                    attach_at(vm, this, child, idx, notify, lp);
                }
            }
            Some(match ret(sig) {
                "V" => None,
                _ => Some(stub_ret(vm, sig)),
            })
        }
        "removeView" | "removeViewInLayout" => {
            // Mirror addView: a bridged group gets its host-side child
            // removed by the registered arm (which also detaches); the
            // fallback covers overloads the bridge didn't register.
            let host = vm
                .named_host(class, name, sig)
                .or_else(|| vm.named_host(class, name, "(Landroid/view/View;)V"));
            let mut ran = false;
            if let Some(f) = host {
                if let Ok(mut g) = f.try_borrow_mut() {
                    let _ = g(vm, args);
                    ran = true;
                }
            }
            if !ran {
                detach(vm, this, args.get(1).copied().unwrap_or(V::Ref(0)), true);
            }
            Some(None)
        }
        "removeViewAt" => {
            let i = i32v(args.get(1).copied().unwrap_or(V::I32(0))) as usize;
            let child = children(vm, this).get(i).copied();
            if let Some(c) = child {
                detach(vm, this, c, true);
            }
            Some(None)
        }
        "removeAllViews" => {
            for c in children(vm, this) {
                detach(vm, this, c, true);
            }
            Some(None)
        }
        "bringChildToFront" => Some(None),
        // ---------- ViewPager ----------
        // ---------- GLSurfaceView ----------
        // The invoke site may name the dex subclass (GLFieldView), so
        // GL-distinctive names match unguarded; generic ones check the
        // class chain resolves through GLSurfaceView/SurfaceView.
        "setRenderer" => {
            if let Some(V::Ref(r)) = args.get(1).copied() {
                if r != 0 {
                    if crate::dbg_flag("OVELO_GLDBG") {
                        eprintln!("[gldbg] setRenderer view={this} r={r} class={class}");
                    }
                    pseudo_set(vm, this, crate::jdk::gl::P_GLES_RENDERER, V::Ref(r));
                    vm.gl_renderer_active = true;
                }
            }
            Some(None)
        }
        "setRenderMode" | "requestRender" => Some(None),
        "queueEvent" | "onPause" | "onResume"
            if class.contains("GLSurfaceView")
                || desc_is_a(class, "Landroid/opengl/GLSurfaceView;") =>
        {
            Some(None)
        }
        "getRenderMode" if class.contains("GLSurfaceView") => Some(Some(V::I32(1))),
        "getHolder" | "getSurface"
            if class.contains("SurfaceView") || desc_is_a(class, "Landroid/view/SurfaceView;") =>
        {
            eprintln!("[holder] getHolder called class={class}");
            let h = match pseudo_get(vm, this, P_HOLDER) {
                Some(V::Ref(h)) if h != 0 => h,
                _ => {
                    let V::Ref(h) = host(vm, "Landroid/view/SurfaceHolder;", HostState::Empty)
                    else {
                        return Ok(Some(Some(V::Ref(0))));
                    };
                    // Back-ref: eglCreateWindowSurface sizes the
                    // surface from the owning view's frame.
                    pseudo_set(vm, h, crate::jdk::P_TARGET, V::Ref(this));
                    pseudo_set(vm, this, P_HOLDER, V::Ref(h));
                    h
                }
            };
            Some(Some(V::Ref(h)))
        }
        // ---------- SurfaceHolder ----------
        "addCallback" | "removeCallback"
            if class.contains("SurfaceHolder")
                || desc_is_a(class, "Landroid/view/SurfaceHolder;") =>
        {
            eprintln!("[holder] {name} called class={class} nargs={}", args.len());
            if name == "addCallback" {
                if let Some(V::Ref(cb)) = args.get(1).copied() {
                    if cb != 0 {
                        pseudo_set(vm, this, P_HOLDER_CBS, V::Ref(cb));
                        // Defer to the next drain tick: Android only
                        // fires these once a real surface exists, and
                        // Godot's `init()` runs this before its
                        // `glThread` field is assigned — inline calls
                        // NPE on the null receiver. The size is resolved
                        // at delivery, not here: the view is often still
                        // unlaid-out when the callback registers (Korge
                        // saw 0x0 and NaN'd its ortho matrix).
                        vm.pending_surface.push((this, cb, 0));
                        if !vm.surface_cbs.iter().any(|&(_, c)| c == cb) {
                            vm.surface_cbs.push((this, cb));
                        }
                    }
                }
            }
            Some(None)
        }
        "isCreating"
        | "setType"
        | "setFixedSize"
        | "setFormat"
        | "setSizeFromLayout"
        | "getSurfaceFrame"
        | "lockCanvas"
        | "lockHardwareCanvas"
        | "unlockCanvasAndPost"
        | "setKeepScreenOn"
            if class.contains("SurfaceHolder")
                || desc_is_a(class, "Landroid/view/SurfaceHolder;") =>
        {
            Some(if name == "isCreating" {
                Some(V::I32(0))
            } else if name.starts_with("lock") || name == "getSurfaceFrame" {
                Some(V::Ref(0))
            } else {
                None
            })
        }
        "setAdapter" if class.contains("ViewPager") => {
            // Drive the PagerAdapter the way populate() would: count,
            // then instantiateItem per page — androidx attaches each
            // page view to the container itself, so they flow through
            // our addView arm into the rendered tree.
            if let Some(V::Ref(ad)) = args.get(1).copied() {
                if ad != 0 {
                    pseudo_set(vm, this, P_TARGET, V::Ref(ad));
                    let n = match vm.call_virtual(ad, "getCount", &[]) {
                        Ok(Some(V::I32(n))) => n.clamp(0, 8),
                        _ => 0,
                    };
                    let mut items: Vec<V> = Vec::new();
                    for i in 0..n {
                        match vm.call_virtual(ad, "instantiateItem", &[V::Ref(this), V::I32(i)]) {
                            Ok(v) => items.push(v.unwrap_or(V::I32(0))),
                            Err(e) => {
                                items.push(V::I32(0));
                                if crate::dbg_flag("OVELO_VIEWDBG") {
                                    eprintln!("[viewdbg] instantiateItem {i} err {e:?}");
                                }
                            }
                        }
                    }
                    // FragmentStatePagerAdapter only promotes a fragment to
                    // RESUMED via setPrimaryItem — instantiateItem alone caps
                    // it at STARTED, so onResume (and the fragment's data
                    // load) never runs. Drive it for the current page.
                    let cur = match vm.call_virtual(this, "getCurrentItem", &[]) {
                        Ok(Some(V::I32(i))) if n > 0 => i.clamp(0, n - 1) as usize,
                        _ => 0,
                    };
                    if let Some(item) = items.get(cur).copied() {
                        if let Err(e) = vm.call_virtual(
                            ad,
                            "setPrimaryItem",
                            &[V::Ref(this), V::I32(cur as i32), item],
                        ) {
                            if crate::dbg_flag("OVELO_VIEWDBG") {
                                eprintln!("[viewdbg] setPrimaryItem {cur} err {e:?}");
                            }
                        }
                    }
                    if crate::dbg_flag("OVELO_VIEWDBG") {
                        eprintln!("[viewdbg] setAdapter {class} adapter={ad} count={n} cur={cur}");
                    }
                }
            }
            Some(None)
        }
        "getViewTreeObserver" => Some(Some(vto_of(vm, this))),
        "addOnGlobalLayoutListener" => {
            if crate::dbg_flag("OVELO_VIEWDBG") {
                eprintln!("[viewdbg] addOnGlobalLayoutListener obs={this} args={args:?}");
            }
            if let Some(&V::Ref(l)) = args.get(1) {
                if l != 0 {
                    vm.vto_listeners.entry(this).or_default().push(l);
                }
            }
            Some(None)
        }
        "removeOnGlobalLayoutListener" | "removeGlobalOnLayoutListener" => {
            if let Some(&V::Ref(l)) = args.get(1) {
                if let Some(ls) = vm.vto_listeners.get_mut(&this) {
                    ls.retain(|x| *x != l);
                }
            }
            Some(None)
        }
        "addOnPreDrawListener" => {
            if crate::dbg_flag("OVELO_VIEWDBG") {
                eprintln!("[viewdbg] addOnPreDrawListener obs={this} args={args:?}");
            }
            if let Some(&V::Ref(l)) = args.get(1) {
                if l != 0 {
                    vm.vto_predraw.entry(this).or_default().push(l);
                }
            }
            Some(None)
        }
        "removeOnPreDrawListener" => {
            if let Some(&V::Ref(l)) = args.get(1) {
                if let Some(ls) = vm.vto_predraw.get_mut(&this) {
                    ls.retain(|x| *x != l);
                }
            }
            Some(None)
        }
        // Other observer listener kinds are accepted but never fired.
        "addOnDrawListener"
        | "addOnScrollChangedListener"
        | "addOnTouchModeChangeListener"
        | "addOnWindowAttachListener"
        | "addOnWindowFocusChangeListener"
        | "removeOnDrawListener"
        | "removeOnScrollChangedListener"
        | "removeOnTouchModeChangeListener"
        | "removeOnWindowAttachListener"
        | "removeOnWindowFocusChangeListener"
        | "addOnWindowShownListener"
        | "removeOnWindowShownListener"
        | "dispatchOnGlobalLayout"
        | "isAlive" => Some(if name == "isAlive" {
            Some(V::I32(1))
        } else {
            None
        }),
        "dispatchOnPreDraw" => {
            // AOSP contract: returns true when a listener CANCELLED the
            // draw (any onPreDraw returned false) — not "proceed".
            let ls: Vec<u32> = vm.vto_predraw.get(&this).cloned().unwrap_or_default();
            let mut cancel = false;
            for l in ls {
                if let Ok(Some(V::I32(0))) = vm.call_virtual(l, "onPreDraw", &[]) {
                    cancel = true;
                }
            }
            Some(Some(V::I32(cancel as i32)))
        }
        "getParent" => Some(Some(pseudo_get(vm, this, P_PARENT).unwrap_or(V::Ref(0)))),
        "getRootView" => {
            let mut cur = this;
            for _ in 0..64 {
                match pseudo_get(vm, cur, P_PARENT) {
                    Some(V::Ref(p)) if p != 0 => cur = p,
                    _ => break,
                }
            }
            Some(Some(V::Ref(cur)))
        }
        "getTag" => {
            let v = match args.get(1) {
                Some(&key) => pseudo_get(vm, this, tag_key(i32v(key))),
                None => pseudo_get(vm, this, P_TAG),
            }
            .unwrap_or(V::Ref(0));
            if crate::dbg_flag("OVELO_VIEWDBG") {
                eprintln!(
                    "[viewdbg] getTag h{this} key={:?} -> {v:?}",
                    args.get(1).map(|k| i32v(*k))
                );
            }
            Some(Some(v))
        }
        "setTag" => {
            if crate::dbg_flag("OVELO_VIEWDBG") {
                eprintln!(
                    "[viewdbg] setTag h{this} key={:?} v={:?}",
                    args.get(1).map(|k| i32v(*k)),
                    args.get(2)
                );
            }
            match args.len() {
                n if n >= 3 => {
                    pseudo_set(vm, this, tag_key(i32v(args[1])), args[2]);
                }
                _ => {
                    pseudo_set(vm, this, P_TAG, args.get(1).copied().unwrap_or(V::Ref(0)));
                }
            }
            Some(None)
        }
        "setOnCheckedChangeListener" => {
            pseudo_set(
                vm,
                this,
                P_CHECK_LISTENER,
                args.get(1).copied().unwrap_or(V::Ref(0)),
            );
            Some(None)
        }
        "isChecked" if is_marked(vm, this) => Some(Some(V::I32(pint(vm, this, P_CHECKED)))),
        "setChecked" if is_marked(vm, this) => {
            let checked = i32v(args.get(1).copied().unwrap_or(V::I32(0)));
            set_compound_checked(vm, this, checked);
            Some(None)
        }
        "check"
            if {
                let cls = class_of(vm, V::Ref(this));
                cls == "Landroid/widget/RadioGroup;"
                    || vm.assignable_desc(&cls, "Landroid/widget/RadioGroup;")
            } =>
        {
            let id = i32v(args.get(1).copied().unwrap_or(V::I32(0)));
            radio_group_set_checked(vm, this, id);
            Some(None)
        }
        "clearCheck"
            if {
                let cls = class_of(vm, V::Ref(this));
                cls == "Landroid/widget/RadioGroup;"
                    || vm.assignable_desc(&cls, "Landroid/widget/RadioGroup;")
            } =>
        {
            radio_group_set_checked(vm, this, -1);
            Some(None)
        }
        "getCheckedRadioButtonId"
            if {
                let cls = class_of(vm, V::Ref(this));
                cls == "Landroid/widget/RadioGroup;"
                    || vm.assignable_desc(&cls, "Landroid/widget/RadioGroup;")
            } =>
        {
            Some(Some(
                pseudo_get(vm, this, P_RADIO_CHECKED).unwrap_or(V::I32(-1)),
            ))
        }
        "setText" | "setTitle" | "setTextScaleX" => {
            if name != "setTextScaleX" {
                pseudo_set(vm, this, P_TEXT, args.get(1).copied().unwrap_or(V::Ref(0)));
                mark_layout_requested(vm, this);
                if crate::dbg_flag("OVELO_VIEWDBG") {
                    eprintln!(
                        "[viewdbg] setText h{this} arg={:?} stored={:?}",
                        args.get(1),
                        pseudo_get(vm, this, P_TEXT)
                    );
                }
            }
            Some(None)
        }
        // contentDescription is an accessibility label — real Views
        // never draw it, so it must not land in P_TEXT (it used to
        // render every day-cell label as overlapped text).
        "setContentDescription" => {
            pseudo_set(vm, this, P_CDESC, args.get(1).copied().unwrap_or(V::Ref(0)));
            Some(None)
        }
        "setHint" => {
            pseudo_set(vm, this, P_HINT, args.get(1).copied().unwrap_or(V::Ref(0)));
            Some(None)
        }
        "getHint" => Some(Some(
            pseudo_get(vm, this, P_HINT).unwrap_or_else(|| jstr(vm, "")),
        )),
        "getText" | "getTitle" => Some(Some(
            pseudo_get(vm, this, P_TEXT).unwrap_or_else(|| jstr(vm, "")),
        )),
        "getContentDescription" => Some(Some(
            pseudo_get(vm, this, P_CDESC).unwrap_or_else(|| jstr(vm, "")),
        )),
        "setTextSize" => {
            let size = match args.last() {
                Some(V::F32(f)) => *f,
                Some(V::I32(b)) => f32::from_bits(*b as u32),
                _ => 0.0,
            };
            let unit = if args.len() >= 3 {
                i32v(args.get(1).copied().unwrap_or(V::I32(2)))
            } else {
                2
            };
            let scale = match unit {
                0 => 1.0,
                1 | 2 => 2.0,
                _ => 1.0,
            };
            pseudo_set(vm, this, P_TSIZE, V::F32(size * scale));
            mark_layout_requested(vm, this);
            Some(None)
        }
        "setImageBitmap" | "setImageDrawable" | "setImageURI" | "setImageIcon"
        | "setImageResource" => {
            pseudo_set(vm, this, P_IMAGE, args.get(1).copied().unwrap_or(V::I32(0)));
            Some(None)
        }
        "getDrawable" => Some(Some(image_drawable(vm, this))),
        "setOnTouchListener" => {
            pseudo_set(
                vm,
                this,
                P_TOUCH_L,
                args.get(1).copied().unwrap_or(V::Ref(0)),
            );
            Some(match ret(sig) {
                "V" => None,
                _ => Some(V::I32(1)),
            })
        }
        "onTouchEvent" => {
            // Base View.onTouchEvent: a clickable view consumes the
            // gesture and performs the click on ACTION_UP. Dex overrides
            // that delegate to super rely on this path.
            let action = match args.get(1) {
                Some(V::Ref(ev)) => match pseudo_get(vm, *ev, P_ME_ACTION) {
                    Some(v) => i32v(v),
                    _ => -1,
                },
                _ => -1,
            };
            match pseudo_get(vm, this, P_LISTENER) {
                Some(V::Ref(l)) if l != 0 => {
                    let down_target = vm.touch_gesture_target;
                    if crate::dbg_flag("OVELO_TOUCHDBG") {
                        eprintln!(
                            "[touchdbg] base-ote h{this} action={action} listener={l} down={down_target} within={}",
                            view_or_descendant(vm, this, down_target)
                        );
                    }
                    if action == 1 && down_target != 0 && view_or_descendant(vm, this, down_target)
                    {
                        let cr = vm.call_virtual(l, "onClick", &[V::Ref(this)]);
                        if crate::dbg_flag("OVELO_TOUCHDBG") {
                            eprintln!("[touchdbg] onClick result: {cr:?}");
                        }
                        vm.touch_gesture_target = 0;
                    }
                    Some(Some(V::I32(1)))
                }
                _ => Some(Some(V::I32(0))),
            }
        }
        "dispatchTouchEvent" => {
            if let Some(V::Ref(l)) = pseudo_get(vm, this, P_TOUCH_L) {
                if l != 0
                    && matches!(
                        vm.call_virtual(l, "onTouch", &[V::Ref(this), args[1]]),
                        Ok(Some(rv)) if i32v(rv) != 0
                    )
                {
                    return Ok(Some(Some(V::I32(1))));
                }
            }
            Some(Some(V::I32(
                match vm.call_virtual(this, "onTouchEvent", &[args[1]]) {
                    Ok(Some(rv)) if i32v(rv) != 0 => 1,
                    _ => 0,
                },
            )))
        }
        "setOnClickListener"
        | "setOnLongClickListener"
        | "setOnFocusChangeListener"
        | "setOnKeyListener"
        | "setOnAttachStateChangeListener"
        | "addOnAttachStateChangeListener" => {
            pseudo_set(
                vm,
                this,
                P_LISTENER,
                args.get(1).copied().unwrap_or(V::Ref(0)),
            );
            Some(match ret(sig) {
                "V" => None,
                _ => Some(V::I32(1)),
            })
        }
        "performClick" | "callOnClick" => {
            if let (Some(V::Ref(dialog)), Some(which_v)) = (
                pseudo_get(vm, this, P_DIALOG_BUTTON_DIALOG),
                pseudo_get(vm, this, P_DIALOG_BUTTON_WHICH),
            ) {
                let which = i32v(which_v);
                let listener = if which == -1 {
                    pseudo_get(vm, dialog, P_DIALOG_POS_LISTENER)
                } else {
                    pseudo_get(vm, dialog, P_DIALOG_NEG_LISTENER)
                };
                if let Some(V::Ref(l)) = listener {
                    if l != 0 {
                        let _ = vm.call_virtual(l, "onClick", &[V::Ref(dialog), V::I32(which)]);
                    }
                }
                dialog_dismiss(vm, dialog);
                return Ok(Some(Some(V::I32(1))));
            }
            let desc = class_of(vm, V::Ref(this));
            if is_compound_desc(vm, &desc) {
                let checked = pint(vm, this, P_CHECKED) != 0;
                // RadioButton.toggle() is a no-op while already checked;
                // only CheckBox/Switch-style compounds flip both ways.
                if !(checked && is_radio_desc(vm, &desc)) {
                    set_compound_checked(vm, this, (!checked) as i32);
                }
            }
            let mut fired = match pseudo_get(vm, this, P_LISTENER) {
                // OnClickListener.onClick returns void: a normal dex
                // completion is `Ok(None)`, not a missing invocation.
                Some(V::Ref(l)) if l != 0 => vm.call_virtual(l, "onClick", &[V::Ref(this)]).is_ok(),
                _ => false,
            };
            // `android:onClick="method"` in layout XML: AOSP's
            // DeclaredOnClickListener invokes the named method on the
            // Context. No listener object exists to attach, so invoke
            // the Activity method straight from the attr.
            if !fired {
                if let Some(mname) = attr_find(vm, this, "onClick").and_then(|v| match v {
                    V::Ref(r) if r != 0 => s(vm, r),
                    _ => None,
                }) {
                    let act = vm.cur_activity;
                    if act != 0 {
                        let _ = vm.call_virtual(act, &mname, &[V::Ref(this)]);
                        fired = true;
                    }
                }
            }
            Some(Some(V::I32(fired as i32)))
        }
        "setVisibility" => {
            pseudo_set(vm, this, P_VIS, args.get(1).copied().unwrap_or(V::I32(0)));
            mark_layout_requested(vm, this);
            Some(None)
        }
        "getVisibility" => Some(Some(pseudo_get(vm, this, P_VIS).unwrap_or(V::I32(0)))),
        "setClipChildren" => {
            pseudo_set(
                vm,
                this,
                P_CLIP_CHILDREN,
                args.get(1).copied().unwrap_or(V::I32(1)),
            );
            Some(None)
        }
        "isShown"
        | "hasWindowFocus"
        | "isEnabled"
        | "isClickable"
        | "isFocusable"
        | "isFocusableInTouchMode"
        | "isFocusableOnScreen"
        | "hasFocusable"
        | "isImportantForAccessibility" => Some(Some(V::I32(1))),
        "isAttachedToWindow" => Some(Some(V::I32(pint(vm, this, P_ATTACHED)))),
        "isFocused" => Some(Some(V::I32(pint(vm, this, P_FOCUSED)))),
        "hasFocus" => {
            fn any_focused(vm: &Vm, h: u32, depth: usize) -> bool {
                depth <= 32
                    && (pint(vm, h, P_FOCUSED) != 0
                        || children(vm, h).iter().any(|c| {
                            matches!(c, V::Ref(ch) if *ch != 0 && any_focused(vm, *ch, depth + 1))
                        }))
            }
            Some(Some(V::I32(any_focused(vm, this, 0) as i32)))
        }
        "isSelected"
        | "isActivated"
        | "isPressed"
        | "isDirty"
        | "isOpaque"
        | "isInEditMode"
        | "isHardwareAccelerated" => Some(Some(V::I32(0))),
        "getVisibility0" => Some(Some(V::I32(0))),
        // Nullable view getters must hand back a real null, not a
        // stub: androidx calls getFocusedChild() during
        // onLayoutChildren's anchor pass and immediately reads
        // getLayoutParams().mViewHolder on whatever comes back — a
        // fresh host View has neither and NPEs inside dex code.
        "getFocusedChild" | "getFocus" | "getFocused" | "getHoveredChild" => Some(Some(V::Ref(0))),
        "findFocus" => {
            fn find_focus(vm: &Vm, h: u32, depth: usize) -> u32 {
                if depth > 32 {
                    return 0;
                }
                if pint(vm, h, P_FOCUSED) != 0 {
                    return h;
                }
                for c in children(vm, h) {
                    if let V::Ref(ch) = c {
                        if ch != 0 {
                            let f = find_focus(vm, ch, depth + 1);
                            if f != 0 {
                                return f;
                            }
                        }
                    }
                }
                0
            }
            Some(Some(V::Ref(find_focus(vm, this, 0))))
        }
        "getResources" => Some(Some(host(
            vm,
            "Landroid/content/res/Resources;",
            HostState::Empty,
        ))),
        "getHandler" => Some(Some(host(vm, "Landroid/os/Handler;", HostState::Empty))),
        "post" | "postOnAnimation" => {
            // AttachInfo.mRunQueue work runs on the next traversal —
            // our drain pass is the same tick; Compose's OwnerSnapshot
            // and AndroidComposeView's invalidation dispatcher post
            // here (dropping the runnable = silent blank).
            if let Some(V::Ref(r)) = args.get(1).copied() {
                if r != 0 {
                    vm.enqueue_runnable(r, true);
                }
            }
            Some(Some(V::I32(1)))
        }
        "postDelayed" | "postOnAnimationDelayed" => {
            if let Some(V::Ref(r)) = args.get(1).copied() {
                if r != 0 {
                    let d = args.get(2).map(|v| i64v(*v)).unwrap_or(0);
                    vm.enqueue_runnable_delayed(r, true, d);
                }
            }
            Some(Some(V::I32(1)))
        }
        "removeCallbacks" => {
            if let Some(V::Ref(r)) = args.get(1).copied() {
                vm.remove_runnable(r);
            }
            Some(Some(V::I32(1)))
        }
        "animate" => Some(Some(host(
            vm,
            "Landroid/view/ViewPropertyAnimator;",
            HostState::Empty,
        ))),
        "getLayoutParams" => {
            let lp = layout_params(vm, this);
            if crate::dbg_flag("OVELO_VIEWDBG") {
                let desc = class_of(vm, V::Ref(this));
                let lp_desc = match lp {
                    V::Ref(h) => class_of(vm, V::Ref(h)),
                    _ => "primitive".into(),
                };
                eprintln!("[viewdbg] {desc}.getLayoutParams h{this} -> {lp_desc}");
                if let V::Ref(lh) = lp {
                    let dump: Vec<String> = match vm.heap.get(lh) {
                        Some(Obj::Instance { fields, .. }) | Some(Obj::Host { fields, .. }) => {
                            fields.iter().map(|(g, v)| format!("{g}={v:?}")).collect()
                        }
                        _ => Vec::new(),
                    };
                    eprintln!("[viewdbg]   lp h{lh} fields {dump:?}");
                }
            }
            Some(Some(lp))
        }
        "setLayoutParams" => {
            if crate::dbg_flag("OVELO_VIEWDBG") {
                let lp_desc = match args.get(1) {
                    Some(V::Ref(h)) => class_of(vm, V::Ref(*h)),
                    _ => "primitive".into(),
                };
                eprintln!(
                    "[viewdbg] setLayoutParams h{this} {} <- {lp_desc}",
                    class_of(vm, V::Ref(this))
                );
            }
            let lp = args.get(1).copied().unwrap_or(V::Ref(0));
            pseudo_set(vm, this, P_LP, lp);
            lp_resolve_dir_dex(vm, lp);
            // AOSP View.setLayoutParams ends in requestLayout() —
            // ConstraintLayout only rebuilds its widget list while a
            // child reports isLayoutRequested.
            mark_layout_requested(vm, this);
            Some(None)
        }
        "measure" => {
            // View.measure(wSpec, hSpec): the EXACTLY/AT_MOST size is
            // what getMeasured* reports until the dex onMeasure calls
            // setMeasuredDimension (which many custom views do).
            if crate::dbg_flag("OVELO_MEASDBG") {
                let (ws, hs) = (vint(args.get(1)), vint(args.get(2)));
                eprintln!("[measdbg] measure h{this} spec={ws:#x},{hs:#x}");
            }
            pseudo_set(vm, this, P_MEAS_W, V::I32(spec_size(args.get(1).copied())));
            pseudo_set(vm, this, P_MEAS_H, V::I32(spec_size(args.get(2).copied())));
            // Guest exceptions must reach the guest's try/catch —
            // swallowing them leaves the caller believing the measure
            // succeeded with half-updated state.
            if vm.has_dex_virtual(this, "onMeasure", &["I", "I"]) {
                let r = vm.call_virtual_rt(
                    this,
                    "onMeasure",
                    &[
                        args.get(1).copied().unwrap_or(V::I32(0)),
                        args.get(2).copied().unwrap_or(V::I32(0)),
                    ],
                )?;
                if crate::dbg_flag("OVELO_VIEWDBG") {
                    eprintln!(
                        "[viewdbg] builtin measure h{this} onMeasure={:?} meas={}x{}",
                        r.is_some(),
                        pint(vm, this, P_MEAS_W) & 0x00FF_FFFF,
                        pint(vm, this, P_MEAS_H) & 0x00FF_FFFF,
                    );
                }
                // ActionMenuView's overflow accounting depends on a
                // large slice of View/LinearLayout machinery. When that
                // dex path collapses every action item to zero, fall
                // back to the ordinary horizontal LinearLayout pass so
                // presenter-created buttons remain visible/clickable.
                if class_of(vm, V::Ref(this)).contains("ActionMenuView")
                    && pint(vm, this, P_MEAS_W) & 0x00FF_FFFF == 0
                    && !children(vm, this).is_empty()
                {
                    group_measure_fallback(
                        vm,
                        this,
                        i32v(args.get(1).copied().unwrap_or(V::I32(0))),
                        i32v(args.get(2).copied().unwrap_or(V::I32(0))),
                    );
                }
            } else {
                // Framework skeleton container — no dex onMeasure
                // exists, so measure the children generically or they
                // stay 0×0 forever.
                group_measure_fallback(
                    vm,
                    this,
                    i32v(args.get(1).copied().unwrap_or(V::I32(0))),
                    i32v(args.get(2).copied().unwrap_or(V::I32(0))),
                );
            }
            Some(None)
        }
        "setMeasuredDimension" => {
            // AOSP masks the stored dims to MEASURED_SIZE_MASK
            // (0x00FF_FFFF); the state bits ride the AndState variant.
            if crate::dbg_flag("OVELO_MEASDBG") {
                let (w, h) = (vint(args.get(1)), vint(args.get(2)));
                eprintln!("[measdbg] setMeasured h{this} {w:#x}x{h:#x}");
            }
            pseudo_set(vm, this, P_MEAS_W, V::I32(vint(args.get(1)) & 0x00FF_FFFF));
            pseudo_set(vm, this, P_MEAS_H, V::I32(vint(args.get(2)) & 0x00FF_FFFF));
            Some(None)
        }
        "layout" => {
            let (l, t, r, b) = (
                vint(args.get(1)),
                vint(args.get(2)),
                vint(args.get(3)),
                vint(args.get(4)),
            );
            if crate::dbg_flag("OVELO_VIEWDBG") {
                eprintln!("[viewdbg] builtin layout h{this} {l},{t},{r},{b}");
            }
            // Keep the request flag through guest onLayout: an exception
            // here leaves the tree unlaid-out, and clearing first makes the
            // host scheduler treat the failed pass as final.
            let changed = frame_set(vm, this, l, t, r, b);
            pseudo_set(vm, this, P_LAYOUT_DONE, V::I32(1));
            if vm.has_dex_virtual(this, "onLayout", &["Z", "I", "I", "I", "I"]) {
                let out = vm.call_virtual_rt(
                    this,
                    "onLayout",
                    &[
                        V::I32(changed as i32),
                        V::I32(l),
                        V::I32(t),
                        V::I32(r),
                        V::I32(b),
                    ],
                );
                if let Err(e) = out {
                    if crate::dbg_flag("OVELO_VIEWDBG") {
                        match &e {
                            crate::E::Vm(oe) => {
                                eprintln!("[viewdbg] onLayout h{this} vmerr {oe}")
                            }
                            crate::E::Throw(th) => {
                                eprintln!("[viewdbg] onLayout h{this} throw h{th}")
                            }
                        }
                    }
                    return Err(e);
                }
                // AOSP clears PFLAG_LAYOUT_REQUESTED once the view is laid out.
                pseudo_set(vm, this, P_LAYOUT_REQ, V::I32(0));
            } else {
                // Framework skeleton container — position children
                // generically or they keep their 0×0 frames.
                group_layout_fallback(vm, this, l, t, r, b);
                pseudo_set(vm, this, P_LAYOUT_REQ, V::I32(0));
            }
            if pseudo_get(vm, this, P_POPUP_COMPACT) == Some(V::I32(1)) {
                layout_popup_rows_compact(vm, this, r - l, b - t);
            }
            if pseudo_get(vm, this, P_DIALOG_HAS_BUTTONS) == Some(V::I32(1)) {
                layout_dialog_buttons(vm, this, r - l, b - t);
            }
            Some(None)
        }
        "setFrame" => {
            let (l, t, r, b) = (
                vint(args.get(1)),
                vint(args.get(2)),
                vint(args.get(3)),
                vint(args.get(4)),
            );
            Some(Some(V::I32(frame_set(vm, this, l, t, r, b) as i32)))
        }
        // Reached when guest code calls `super.onMeasure`/`super.onLayout`
        // on a framework skeleton class — without an arm the super call
        // no-ops and children keep 0×0 frames (MonthViewWrapper).
        "onMeasure" => {
            group_measure_fallback(vm, this, vint(args.get(1)), vint(args.get(2)));
            Some(None)
        }
        "onLayout" => {
            let (l, t, r, b) = (
                vint(args.get(2)),
                vint(args.get(3)),
                vint(args.get(4)),
                vint(args.get(5)),
            );
            group_layout_fallback(vm, this, l, t, r, b);
            Some(None)
        }
        "onDraw" => Some(None),
        "offsetLeftAndRight" => {
            let d = vint(args.get(1));
            pseudo_set(vm, this, P_LEFT, V::I32(pint(vm, this, P_LEFT) + d));
            pseudo_set(vm, this, P_RIGHT, V::I32(pint(vm, this, P_RIGHT) + d));
            Some(None)
        }
        "offsetTopAndBottom" => {
            let d = vint(args.get(1));
            pseudo_set(vm, this, P_TOP, V::I32(pint(vm, this, P_TOP) + d));
            pseudo_set(vm, this, P_BOTTOM, V::I32(pint(vm, this, P_BOTTOM) + d));
            Some(None)
        }
        "getLeft" => Some(Some(V::I32(pint(vm, this, P_LEFT)))),
        "getTop" => Some(Some(V::I32(pint(vm, this, P_TOP)))),
        "getRight" => Some(Some(V::I32(pint(vm, this, P_RIGHT)))),
        "getBottom" => Some(Some(V::I32(pint(vm, this, P_BOTTOM)))),
        "setLeft" => {
            pseudo_set(vm, this, P_LEFT, V::I32(vint(args.get(1))));
            pseudo_set(vm, this, P_LAYOUT_DONE, V::I32(1));
            Some(None)
        }
        "setTop" => {
            pseudo_set(vm, this, P_TOP, V::I32(vint(args.get(1))));
            pseudo_set(vm, this, P_LAYOUT_DONE, V::I32(1));
            Some(None)
        }
        "setRight" => {
            pseudo_set(vm, this, P_RIGHT, V::I32(vint(args.get(1))));
            pseudo_set(vm, this, P_LAYOUT_DONE, V::I32(1));
            Some(None)
        }
        "setBottom" => {
            pseudo_set(vm, this, P_BOTTOM, V::I32(vint(args.get(1))));
            pseudo_set(vm, this, P_LAYOUT_DONE, V::I32(1));
            Some(None)
        }
        "getX" => Some(Some(V::F32(pint(vm, this, P_LEFT) as f32))),
        "getY" => Some(Some(V::F32(pint(vm, this, P_TOP) as f32))),
        "getWidth" => Some(Some(V::I32(
            pint(vm, this, P_RIGHT).saturating_sub(pint(vm, this, P_LEFT)),
        ))),
        "getHeight" => Some(Some(V::I32(
            pint(vm, this, P_BOTTOM).saturating_sub(pint(vm, this, P_TOP)),
        ))),
        "getMeasuredWidth" => Some(Some(V::I32(pint(vm, this, P_MEAS_W) & 0x00FF_FFFF))),
        "getMeasuredHeight" => {
            if crate::dbg_flag("OVELO_MEASDBG") {
                eprintln!(
                    "[measdbg] getMeasuredHeight h{this} -> {}",
                    pint(vm, this, P_MEAS_H) & 0x00FF_FFFF
                );
            }
            Some(Some(V::I32(pint(vm, this, P_MEAS_H) & 0x00FF_FFFF)))
        }
        "getLocalVisibleRect" | "getHitRect" | "getGlobalVisibleRect" | "getDrawingRect" => {
            // `new Rect()` arg gets (0,0,w,h) — local coords; global
            // rect adds the view's absolute origin.
            let w = pint(vm, this, P_RIGHT).saturating_sub(pint(vm, this, P_LEFT));
            let h = pint(vm, this, P_BOTTOM).saturating_sub(pint(vm, this, P_TOP));
            if let Some(V::Ref(rh)) = args.get(1) {
                if *rh != 0 {
                    let (ox, oy) = if name == "getGlobalVisibleRect" {
                        let (mut x, mut y, mut v) = (0, 0, this);
                        for _ in 0..64 {
                            x += pint(vm, v, P_LEFT);
                            y += pint(vm, v, P_TOP);
                            v = match pseudo_get(vm, v, P_PARENT) {
                                Some(V::Ref(p)) => p,
                                _ => break,
                            };
                        }
                        (x, y)
                    } else {
                        (0, 0)
                    };
                    super::android::rect_write(vm, *rh, ox, oy, ox + w, oy + h);
                }
            }
            Some(Some(V::I32(1)))
        }
        "getMeasuredWidthAndState" => Some(Some(V::I32(pint(vm, this, P_MEAS_W)))),
        "getMeasuredHeightAndState" => Some(Some(V::I32(pint(vm, this, P_MEAS_H)))),
        "getPaddingLeft" | "getPaddingStart" => Some(Some(V::I32(view_padding(vm, this).0))),
        "getPaddingTop" => Some(Some(V::I32(view_padding(vm, this).1))),
        "getPaddingRight" | "getPaddingEnd" => Some(Some(V::I32(view_padding(vm, this).2))),
        "getPaddingBottom" => Some(Some(V::I32(view_padding(vm, this).3))),
        // No host-side compound drawable insets yet; until those exist
        // the compound padding is the view padding. Material's
        // TextInputLayout copies these values straight back through
        // setPadding during measurement, so returning 0 erases padding.
        "getCompoundPaddingLeft" => Some(Some(V::I32(view_padding(vm, this).0))),
        "getCompoundPaddingTop" => Some(Some(V::I32(view_padding(vm, this).1))),
        "getCompoundPaddingRight" => Some(Some(V::I32(view_padding(vm, this).2))),
        "getCompoundPaddingBottom" => Some(Some(V::I32(view_padding(vm, this).3))),
        "setPadding" | "setPaddingRelative" => {
            pseudo_set(vm, this, P_PAD_L, V::I32(vint(args.get(1))));
            pseudo_set(vm, this, P_PAD_T, V::I32(vint(args.get(2))));
            pseudo_set(vm, this, P_PAD_R, V::I32(vint(args.get(3))));
            pseudo_set(vm, this, P_PAD_B, V::I32(vint(args.get(4))));
            mark_layout_requested(vm, this);
            Some(None)
        }
        "getScrollX" => Some(Some(V::I32(pint(vm, this, P_SCROLL_X)))),
        "getScrollY" => Some(Some(V::I32(pint(vm, this, P_SCROLL_Y)))),
        "setScrollX" => {
            pseudo_set(vm, this, P_SCROLL_X, V::I32(vint(args.get(1))));
            Some(None)
        }
        "setScrollY" => {
            pseudo_set(vm, this, P_SCROLL_Y, V::I32(vint(args.get(1))));
            Some(None)
        }
        "scrollTo" => {
            pseudo_set(vm, this, P_SCROLL_X, V::I32(vint(args.get(1))));
            pseudo_set(vm, this, P_SCROLL_Y, V::I32(vint(args.get(2))));
            Some(None)
        }
        "scrollBy" => {
            let dx = vint(args.get(1));
            let dy = vint(args.get(2));
            pseudo_set(
                vm,
                this,
                P_SCROLL_X,
                V::I32(pint(vm, this, P_SCROLL_X) + dx),
            );
            pseudo_set(
                vm,
                this,
                P_SCROLL_Y,
                V::I32(pint(vm, this, P_SCROLL_Y) + dy),
            );
            Some(None)
        }
        "getMinimumWidth" => Some(Some(V::I32(min_dim(vm, this, P_MINW, "minWidth")))),
        "getMinimumHeight" => Some(Some(V::I32(min_dim(vm, this, P_MINH, "minHeight")))),
        // AOSP: max(minDim, background minimum). Drawable minimums are
        // ~0 for colors/bitmaps, so the explicit minimum wins.
        "getSuggestedMinimumWidth" => Some(Some(V::I32(min_dim(vm, this, P_MINW, "minWidth")))),
        "getSuggestedMinimumHeight" => Some(Some(V::I32(min_dim(vm, this, P_MINH, "minHeight")))),
        "getBaseline" => Some(Some(V::I32(-1))),
        // requestLayout must be observable: ConstraintLayout re-arms
        // mDirtyHierarchy only when a child reports isLayoutRequested,
        // so a permanent 0 leaves adapter-driven re-layouts impossible.
        "isLayoutRequested" => Some(Some(V::I32(pint(vm, this, P_LAYOUT_REQ)))),
        "requestLayout" | "forceLayout" => {
            mark_layout_requested(vm, this);
            Some(None)
        }
        "setMinimumWidth" => {
            pseudo_set(vm, this, P_MINW, V::I32(vint(args.get(1))));
            Some(None)
        }
        "setMinimumHeight" => {
            pseudo_set(vm, this, P_MINH, V::I32(vint(args.get(1))));
            Some(None)
        }
        "setOrientation" => {
            pseudo_set(vm, this, P_ORIENTATION, V::I32(vint(args.get(1))));
            Some(None)
        }
        "willNotDraw" => Some(Some(V::I32(0))),
        "invalidate" | "postInvalidate" => {
            // A draw-only change touches no layout/text field, but the
            // host only re-records draw ops when `view_gen` moved —
            // a dedicated sequence slot flips on every invalidate so
            // the next traversal re-runs guest_draw_ops for this tree.
            let n = pint(vm, this, P_INVALID_SEQ);
            pseudo_set(vm, this, P_INVALID_SEQ, V::I32(n.wrapping_add(1)));
            if crate::dbg_flag("OVELO_VIEWDBG") {
                eprintln!("[viewdbg] invalidate h{this} seq={}", n + 1);
            }
            Some(None)
        }
        "setBackgroundColor" => {
            // Always a packed ARGB literal — low-alpha colors are
            // positive ints and must not be read as resource ids.
            pseudo_set(
                vm,
                this,
                P_BGCOLOR,
                args.get(1).cloned().unwrap_or(V::I32(0)),
            );
            Some(None)
        }
        "setBackgroundResource" => {
            pseudo_set(vm, this, P_BGRES, args.get(1).cloned().unwrap_or(V::I32(0)));
            Some(None)
        }
        "requestFocus" => {
            clear_window_focus(vm);
            pseudo_set(vm, this, P_FOCUSED, V::I32(1));
            Some(Some(V::I32(1)))
        }
        "clearFocus" => {
            pseudo_set(vm, this, P_FOCUSED, V::I32(0));
            Some(None)
        }
        "setCompoundDrawables"
        | "setCompoundDrawablesRelative"
        | "setCompoundDrawablesWithIntrinsicBounds"
        | "setCompoundDrawablesRelativeWithIntrinsicBounds" => {
            let vals: Vec<V> = (1..=4)
                .map(|i| args.get(i).copied().unwrap_or(V::Ref(0)))
                .collect();
            let a = arr(vm, "Landroid/graphics/drawable/Drawable;", vals);
            pseudo_set(vm, this, P_COMPOUND_DRAWABLES, a);
            Some(None)
        }
        "getCompoundDrawables" | "getCompoundDrawablesRelative" => {
            let out = match pseudo_get(vm, this, P_COMPOUND_DRAWABLES) {
                Some(v @ V::Ref(_)) => v,
                _ => arr(
                    vm,
                    "Landroid/graphics/drawable/Drawable;",
                    vec![V::Ref(0); 4],
                ),
            };
            Some(Some(out))
        }
        "setAlpha"
        | "setElevation"
        | "setShowDividers"
        | "setTranslationX"
        | "setTranslationY"
        | "setTranslationZ"
        | "setScaleX"
        | "setScaleY"
        | "setX"
        | "setY"
        | "setRotation"
        | "setSelected"
        | "setActivated"
        | "setPressed"
        | "setEnabled"
        | "setClickable"
        | "setLongClickable"
        | "setFocusable"
        | "setFocusableInTouchMode"
        | "setWillNotDraw"
        | "setDescendantFocusability"
        | "setClipToPadding"
        | "setNestedScrollingEnabled"
        | "startNestedScroll"
        | "stopNestedScroll"
        | "setSaveEnabled"
        | "setDuplicateParentStateEnabled"
        | "setLayerType"
        | "setKeepScreenOn"
        | "setFitsSystemWindows"
        | "setSystemUiVisibility"
        | "setForegroundGravity"
        | "setHorizontalGravity"
        | "setVerticalGravity"
        | "setGravity"
        | "setWeightSum"
        | "setClipBounds"
        | "setOutlineAmbientShadowColor"
        | "setOverScrollMode"
        | "setPivotX"
        | "setPivotY"
        | "setTooltipText"
        | "setTransitionName"
        | "setImportantForAccessibility"
        | "setAccessibilityLiveRegion"
        | "setBackground"
        | "setTextColor"
        | "setTypeface"
        | "setSingleLine"
        | "setMaxLines"
        | "setMinLines"
        | "setLines"
        | "setEllipsize"
        | "setInputType"
        | "setImeOptions"
        | "setError"
        | "setMovementMethod"
        | "setMarqueeRepeatLimit"
        | "setFilters"
        | "setEnabled_true"
        | "setCheckMarkDrawable"
        | "setProgress"
        | "setMax"
        | "setIndeterminate"
        | "setOnItemClickListener"
        | "setMinWidth"
        | "setMinHeight"
        | "setMaxWidth"
        | "setMaxHeight"
        | "setIndeterminateDrawableTiled"
        | "setSecondaryProgress"
        | "setMin"
        | "setInterpolator"
        | "setIndeterminateTintList"
        | "setProgressTintList"
        | "setProgressTintMode"
        | "setProgressBackgroundTintList"
        | "setProgressBackgroundTintMode"
        | "setSecondaryProgressTintList"
        | "setOnItemSelectedListener"
        | "setOnScrollListener"
        | "setOnRefreshListener"
        | "setRefreshing"
        | "setLayoutManager"
        | "setClipToOutline"
        | "setOutlineProvider"
        | "setAccessibilityDelegate"
        | "setAutofillHints"
        | "setScreenReaderFocusable"
        | "dispatchKeyEvent"
        | "dispatchGenericMotionEvent" => Some(match ret(sig) {
            "V" => None,
            "Z" => Some(V::I32(if name == "setRefreshing" { 0 } else { 1 })),
            _ => Some(stub_ret(vm, sig)),
        }),

        // ---------- Window ----------
        "getDecorView" | "peekDecorView" => Some(Some(V::Ref(decor_of(vm, this).unwrap_or(0)))),
        "setContentView" if class == WINDOW => {
            // Window.setContentView(int|View): inflate if an id, then parent
            // the result under the decor view.
            let content = match args.get(1).copied() {
                Some(V::I32(id)) if id != 0 => inflate_res(vm, id as u32, V::Ref(0)),
                Some(v @ V::Ref(_)) => v,
                _ => V::Ref(0),
            };
            if let (Some(cf), V::Ref(cv)) = (content_frame_of(vm, this), content) {
                attach(vm, cf, V::Ref(cv));
                // The renderer keys content off the owning Activity's
                // P_CONTENT — mirror it there (the window's owner was
                // stored on P_TARGET by window_of).
                if cv != 0 {
                    if let V::Ref(owner) = pseudo_get(vm, this, P_TARGET).unwrap_or(V::Ref(0)) {
                        if owner != 0 {
                            pseudo_set(vm, owner, P_CONTENT, V::Ref(cv));
                        }
                    }
                }
            }
            Some(None)
        }
        "setIndeterminateDrawable" | "setProgressDrawable" => {
            let d = args.get(1).copied().unwrap_or(V::Ref(0));
            let key = if name == "setIndeterminateDrawable" {
                P_IND_DRAW
            } else {
                P_PROG_DRAW
            };
            pseudo_set(vm, this, key, d);
            Some(None)
        }
        "getIndeterminateDrawable" | "getProgressDrawable" => {
            let key = if name == "getIndeterminateDrawable" {
                P_IND_DRAW
            } else {
                P_PROG_DRAW
            };
            Some(Some(
                pseudo_get(vm, this, key)
                    .filter(|v| matches!(v, V::Ref(r) if *r != 0))
                    .unwrap_or_else(|| stub_ret(vm, sig)),
            ))
        }
        // On real Android the Activity is the Window.Callback; the
        // window stores its owner on P_TARGET (set by window_of).
        "getCallback" => Some(Some(
            pseudo_get(vm, this, P_LISTENER)
                .or_else(|| pseudo_get(vm, this, P_TARGET))
                .unwrap_or(V::Ref(0)),
        )),
        "setCallback" => {
            pseudo_set(
                vm,
                this,
                P_LISTENER,
                args.get(1).copied().unwrap_or(V::Ref(0)),
            );
            Some(None)
        }
        "getAttributes" => Some(Some(host(
            vm,
            "Landroid/view/WindowManager$LayoutParams;",
            HostState::Empty,
        ))),
        "setAttributes"
        | "setLayout"
        | "addFlags"
        | "clearFlags"
        | "setFlags"
        | "setSoftInputMode"
        | "setFeatureInt"
        | "takeSurface"
        | "setBackgroundDrawable"
        | "setBackgroundDrawableResource"
        | "setDimAmount"
        | "setWindowAnimations"
        | "setType"
        | "setStatusBarColor"
        | "setNavigationBarColor"
        | "setFormat"
        | "setWindowManager"
        | "setColorMode"
        | "addSystemFlags"
        | "setBlurBehindRadius"
        | "setHideOverlayWindows"
        | "setDecorFitsSystemWindows" => Some(None),
        "isFloating" | "isActive" | "hasFeature" => Some(Some(V::I32(0))),
        // WindowMetrics carries a Rect of the display bounds — androidx
        // (WindowInsetsAnimationCompat$Impl30's <clinit>, BottomSheet)
        // reads it for inset math. Matches the DisplayMetrics default.
        "getCurrentWindowMetrics" | "getMaximumWindowMetrics"
            if class == "Landroid/view/WindowManager;" =>
        {
            let m = host(vm, "Landroid/view/WindowMetrics;", HostState::Empty);
            if let V::Ref(mh) = m {
                let r = host(vm, "Landroid/graphics/Rect;", HostState::Empty);
                if let V::Ref(rh) = r {
                    pseudo_set(vm, rh, P_RECT_L, V::I32(0));
                    pseudo_set(vm, rh, P_RECT_T, V::I32(0));
                    pseudo_set(vm, rh, P_RECT_R, V::I32(1080));
                    pseudo_set(vm, rh, P_RECT_B, V::I32(1920));
                }
                slot_put(vm, mh, r);
            }
            Some(Some(m))
        }
        "getBounds" if class == "Landroid/view/WindowMetrics;" => {
            Some(Some(slot_of(vm, this).unwrap_or(V::Ref(0))))
        }
        "getWindowInsets" if class == "Landroid/view/WindowMetrics;" => Some(Some(host(
            vm,
            "Landroid/view/WindowInsets;",
            HostState::Empty,
        ))),
        "getInsets" | "getInsetsIgnoringVisibility" | "isVisible"
            if class == "Landroid/view/WindowInsets;" =>
        {
            // Zero insets: no system bars are carved out of our window.
            Some(Some(match ret(sig) {
                "Z" => V::I32(0),
                "Landroid/graphics/Insets;" => {
                    host(vm, "Landroid/graphics/Insets;", HostState::Empty)
                }
                _ => V::I32(0),
            }))
        }
        "getWindowManager"
        | "getContainer"
        | "getCurrentFocus"
        | "getNavigationBarColor"
        | "getStatusBarColor"
        | "getMediaController"
        | "getLayoutInflater" => Some(Some(stub_ret(vm, sig))),
        // AOSP `TextClock.onAttachedToWindow` registers the ticker and
        // calls updateTime(); subclasses that don't override attach
        // (fossify's MyTextClock) still expect the text to appear.
        "onAttachedToWindow" => {
            let d = class_of(vm, V::Ref(this));
            if vm.assignable_desc(&d, "Landroid/widget/TextClock;") {
                textclock_update(vm, this);
                // AOSP postet den Ticker per postDelayed — bei uns
                // re-frischt die Drain-Kadenz alle angemeldeten Uhren.
                if !vm.clock_views.contains(&this) {
                    vm.clock_views.push(this);
                }
            }
            Some(None)
        }

        _ => {
            // Any other method on an inflated view/inflater/window: answer
            // by return type so setters no-op and getters don't lie.
            Some(match ret(sig) {
                "V" => None,
                _ => Some(stub_ret(vm, sig)),
            })
        }
    })
}

/// String-Attr aus dem Inflation-AttributeSet: direkter String ODER
/// `@string/`-Referenz (I32 res-id → arsc-Lookup). Zahlen/Bools liefern
/// `None` — ein Format-Attribut ist immer ein String-Typ.
fn attr_str(vm: &Vm, h: u32, name: &str) -> Option<String> {
    match attr_find(vm, h, name)? {
        v @ V::Ref(_) => sv(vm, v),
        V::I32(id) if id > 0 => vm
            .apk_res
            .as_ref()
            .and_then(|r| r.entry_value(id as u32))
            .and_then(|e| e.string.clone()),
        _ => None,
    }
}

/// `TextClock.updateTime`: format `now` through the view's
/// `format24Hour`/`format12Hour` pattern attr (a SimpleDateFormat
/// pattern; apps like fossify clock set "EEE, d MMM" for the date
/// line) and write it into the text pseudo-field. Writes only when the
/// rendered text actually changed — an unchanged string must not mark
/// the view dirty every drain tick.
pub(crate) fn textclock_update(vm: &mut Vm, this: u32) {
    let pat = attr_str(vm, this, "format24Hour")
        .or_else(|| attr_str(vm, this, "format12Hour"))
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| "H:mm:ss".into());
    let text = clock_pattern(&pat, now_ms());
    let cur = pseudo_get(vm, this, P_TEXT).and_then(|v| sv(vm, v));
    if cur.as_deref() != Some(text.as_str()) {
        let text = jdynstr(vm, &text);
        pseudo_set(vm, this, P_TEXT, text);
    }
}

/// Minimal `SimpleDateFormat` pattern renderer (UTC — the VM's
/// TimeZone stub is UTC everywhere): y/M/d/E/H/h/K/k/m/s/a/D fields,
/// quoted literals, other chars verbatim.
fn clock_pattern(pat: &str, ms: i64) -> String {
    const MONTHS: [&str; 12] = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];
    const DAYS: [&str; 7] = [
        "Sunday",
        "Monday",
        "Tuesday",
        "Wednesday",
        "Thursday",
        "Friday",
        "Saturday",
    ];
    let (y, mo, d, h24, mi, s, _) = time::ms_fields(ms);
    let dow = ((ms.div_euclid(86_400_000) + 4).rem_euclid(7)) as usize; // 0=Sun
    let chars: Vec<char> = pat.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\'' {
            // Quoted literal: '' is one quote, 'text' is verbatim.
            i += 1;
            if i < chars.len() && chars[i] == '\'' {
                out.push('\'');
                i += 1;
                continue;
            }
            while i < chars.len() && chars[i] != '\'' {
                out.push(chars[i]);
                i += 1;
            }
            i += 1;
            continue;
        }
        if !c.is_ascii_alphabetic() {
            out.push(c);
            i += 1;
            continue;
        }
        let mut n = 1;
        while i + n < chars.len() && chars[i + n] == c {
            n += 1;
        }
        let num = |v: i64| {
            if n >= 2 {
                format!("{v:02}")
            } else {
                v.to_string()
            }
        };
        out.push_str(&match c {
            'y' => {
                if n == 2 {
                    format!("{:02}", y % 100)
                } else {
                    y.to_string()
                }
            }
            'M' => match n {
                1 | 2 => num(mo),
                3 => MONTHS[(mo - 1) as usize][..3].to_string(),
                _ => MONTHS[(mo - 1) as usize].to_string(),
            },
            'd' => num(d),
            'D' => {
                let doy = ms.div_euclid(86_400_000) - time::days_from_civil(y, 1, 1) + 1;
                if n >= 3 {
                    format!("{doy:03}")
                } else {
                    doy.to_string()
                }
            }
            'E' => {
                let full = DAYS[dow];
                if n >= 4 {
                    full.to_string()
                } else {
                    full[..3].to_string()
                }
            }
            'a' => {
                if h24 < 12 {
                    "AM".into()
                } else {
                    "PM".into()
                }
            }
            'H' => num(h24),
            'h' => num(if h24 % 12 == 0 { 12 } else { h24 % 12 }),
            'K' => num(h24 % 12),
            'k' => num(if h24 == 0 { 24 } else { h24 }),
            'm' => num(mi),
            's' => num(s),
            _ => c.to_string().repeat(n),
        });
        i += n;
    }
    out
}

// ---------------------------------------------------------------------
// LayoutInflater.inflate

/// A LayoutInflater host object carrying its Context.
pub(crate) fn inflater(vm: &mut Vm, ctx: Option<V>) -> V {
    let h = vm.alloc_host(INFLATER, HostState::Empty);
    if let Some(c) = ctx {
        pseudo_set(vm, h, P_CONTEXT, c);
    }
    V::Ref(h)
}

/// The one `Choreographer.getInstance()` — dex code compares it by
/// identity (`AndroidUiDispatcher` hangs the frame clock off the
/// exact object it fetched).
fn choreographer(vm: &mut Vm) -> V {
    if vm.choreographer_h == 0 {
        vm.choreographer_h = vm.alloc_host(CHOR, HostState::Empty);
    }
    V::Ref(vm.choreographer_h)
}

fn inflate_call(vm: &mut Vm, args: &[V]) -> V {
    if crate::dbg_flag("OVELO_VIEWDBG") {
        eprintln!("[viewdbg] inflate_call args={args:?}");
    }
    // inflate(int, ViewGroup, bool) — the parser-taking overloads aren't
    // modeled yet and degrade to a bare view.
    // Static View.inflate's first parameter is already the Context;
    // LayoutInflater instance calls pass the inflater and store the
    // Context on it.
    let first = args.first().copied().unwrap_or(V::Ref(0));
    let ctx = match first {
        V::Ref(h) if class_of(vm, first).contains("LayoutInflater") => {
            pseudo_get(vm, h, P_CONTEXT).unwrap_or(first)
        }
        _ => first,
    };
    let inflated = match args.get(1).copied() {
        Some(V::I32(id)) if id != 0 => inflate_res(vm, id as u32, ctx),
        _ => empty_view(vm),
    };
    let root = args.get(2).copied().unwrap_or(V::Ref(0));
    // inflate(res, root) == inflate(res, root, root != null).
    let attach_to = args
        .get(3)
        .map(|v| i32v(*v) != 0)
        .unwrap_or(matches!(root, V::Ref(r) if r != 0));
    if let V::Ref(r) = root {
        if r != 0 && attach_to {
            // `<merge>` contributes no view of its own — splice its
            // children directly under the supplied root so the binding
            // finds them on `findChildViewById(root, …)`.
            let kids = match inflated {
                V::Ref(h) if matches!(pseudo_get(vm, h, P_MERGE), Some(V::I32(1))) => {
                    children(vm, h)
                }
                V::Ref(h) => vec![V::Ref(h)],
                _ => vec![],
            };
            for k in kids {
                attach(vm, r, k);
            }
            return root;
        }
        // attachToRoot=false: real LayoutInflater still gives the
        // inflated root its parent's LayoutParams —
        // `root.generateLayoutParams(attrs)` + `setLayoutParams` —
        // which is where dex-side params (RecyclerView's own
        // LayoutParams with `mDecorInsets`, …) come from. Skipping it
        // leaves a host placeholder that dex field reads (`iget
        // LP.mDecorInsets.top`) see as null.
        if r != 0 {
            if let V::Ref(h) = inflated {
                if h != 0 && pseudo_get(vm, h, P_LP).is_none() {
                    let attrs = pseudo_get(vm, h, P_ATTRS).unwrap_or(V::Ref(0));
                    if let Ok(Some(v @ V::Ref(lp))) = vm.call_virtual_sig(
                        r,
                        "generateLayoutParams",
                        &["Landroid/util/AttributeSet;"],
                        &[attrs],
                    ) {
                        if lp != 0 {
                            pseudo_set(vm, h, P_LP, v);
                        }
                    }
                    if pseudo_get(vm, h, P_LP).is_none() {
                        // Attr parsing may have thrown — the attr-free
                        // default still fully initializes the params.
                        if let Ok(Some(v @ V::Ref(lp))) =
                            vm.call_virtual_sig(r, "generateDefaultLayoutParams", &[], &[])
                        {
                            if lp != 0 {
                                pseudo_set(vm, h, P_LP, v);
                            }
                        }
                    }
                }
            }
        }
    }
    inflated
}

/// res_id → inflated view tree (or a bare placeholder when the APK's
/// resources aren't loaded or the id doesn't name a layout).
pub(crate) fn inflate_res(vm: &mut Vm, res_id: u32, ctx: V) -> V {
    let elems = vm.apk_res.as_ref().and_then(|r| r.layout_elements(res_id));
    if crate::dbg_flag("OVELO_VIEWDBG") {
        eprintln!(
            "[viewdbg] inflate {res_id:#x}: {}",
            match &elems {
                Some(e) => format!("{} elems", e.len()),
                None => "no layout".to_string(),
            }
        );
    }
    match elems {
        Some(e) if !e.is_empty() => {
            let v = build_tree(vm, &e, ctx);
            // Remember every inflated root: appcompat builds its own
            // decor tree dex-side, so Window/Activity.findViewById must
            // be able to reach the real content even when it was never
            // attached through our synthetic decor.
            if let V::Ref(h) = v {
                if !vm.view_roots.contains(&h) {
                    vm.view_roots.push(h);
                }
            }
            v
        }
        _ => empty_view(vm),
    }
}

fn empty_view(vm: &mut Vm) -> V {
    let h = vm.alloc_host(VIEW, HostState::Empty);
    pseudo_set(vm, h, P_IS_VIEW, V::I32(1));
    V::Ref(h)
}

/// Turn a tag name into a view object (no children attached). `attrs`
/// is the `AttributeSet` view constructors read styleable values from.
fn make_view_named(vm: &mut Vm, tag: &str, attrs: V, ctx: V) -> V {
    let (name, class_attr) = tag.split_once('\u{0}').unwrap_or((tag, ""));
    let desc = match name {
        "merge" => VIEWGROUP.to_string(),
        "fragment" => "Landroidx/fragment/app/FragmentContainerView;".to_string(),
        "include" => VIEWGROUP.to_string(),
        "view" => {
            if !class_attr.is_empty() {
                format!("L{};", class_attr.replace('.', "/"))
            } else {
                VIEW.to_string()
            }
        }
        "View" => VIEW.to_string(),
        _ if name.contains('.') => format!("L{};", name.replace('.', "/")),
        _ => format!("Landroid/widget/{name};"),
    };
    // Dex-aware ancestry: `desc_is_a` only knows host supers, so a
    // dex-defined ViewGroup subclass (MyRecyclerView → androidx
    // RecyclerView → ViewGroup) must resolve through the VM or it is
    // never marked as a group — and its onLayout never runs.
    let group =
        is_group_tag(name) || desc_is_a(&desc, VIEWGROUP) || vm.assignable_desc(&desc, VIEWGROUP);
    // Dex-defined view classes (custom views, bundled libs like
    // material's NavigationView) get a real instance whose constructor
    // runs — the standard overloads are (Context), (Context, AttributeSet)
    // and (Context, AttributeSet, int). A crashed ctor falls back to the
    // opaque host stub.
    if crate::dbg_flag("OVELO_VIEWDBG") && desc.contains("Toolbar") {
        let names: Vec<String> = match attrs {
            V::Ref(ah) => match vm.heap.get(ah) {
                Some(Obj::Host {
                    state: HostState::Map(m),
                    ..
                }) => m.iter().filter_map(|(k, _)| sv(vm, *k)).collect(),
                _ => Vec::new(),
            },
            _ => Vec::new(),
        };
        eprintln!("[viewdbg] toolbar ctor attrs: {names:?}");
    }
    let h = try_construct_view(vm, &desc, ctx, attrs).unwrap_or_else(|| {
        if crate::dbg_flag("OVELO_VIEWDBG") {
            eprintln!("[viewdbg] make_view_named {desc} -> host stub");
        }
        vm.alloc_host(&desc, HostState::Empty)
    });
    if crate::dbg_flag("OVELO_VIEWDBG") && desc.contains("Toolbar") {
        eprintln!("[viewdbg] toolbar -> {h:?}");
    }
    pseudo_set(vm, h, P_IS_VIEW, V::I32(1));
    if group {
        pseudo_set(vm, h, P_IS_GROUP, V::I32(1));
    }
    if !matches!(ctx, V::Ref(0) | V::I32(0)) {
        pseudo_set(vm, h, P_CONTEXT, ctx);
    }
    if name == "fragment" {
        if let V::Ref(_) = attrs {
            if let Some(class_name) = attrset_value(vm, attrs, "name").and_then(|v| match v {
                V::Ref(_) => sv(vm, v),
                V::I32(id) if id > 0 => vm
                    .apk_res
                    .as_ref()
                    .and_then(|r| r.entry_value(id as u32))
                    .and_then(|e| e.string.clone()),
                _ => None,
            }) {
                let id = attrset_value(vm, attrs, "id").map(i32v).unwrap_or(0);
                inflate_static_fragment(vm, &class_name, id, ctx, h, attrs);
            }
        }
    }
    V::Ref(h)
}

fn instance_refs(vm: &Vm, h: u32) -> Vec<u32> {
    match vm.heap.get(h) {
        Some(Obj::Instance { fields, .. }) | Some(Obj::Host { fields, .. }) => fields
            .values()
            .filter_map(|v| match v {
                V::Ref(r) if *r != 0 => Some(*r),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Breadth-first search through a small object graph for a class-name
/// fragment. AndroidX class merging changes the owner of the exact
/// fields, but the helper/manager object types themselves keep these
/// names in the dex.
fn find_named_object(vm: &Vm, root: u32, needle: &str) -> Option<u32> {
    let mut todo = vec![(root, 0u32)];
    let mut seen = std::collections::HashSet::new();
    while let Some((h, depth)) = todo.pop() {
        if h == 0 || depth > 4 || !seen.insert(h) || seen.len() > 128 {
            continue;
        }
        if class_of(vm, V::Ref(h)).contains(needle) {
            return Some(h);
        }
        for r in instance_refs(vm, h) {
            todo.push((r, depth + 1));
        }
    }
    None
}

fn set_named_field_all(vm: &mut Vm, h: u32, name: &str, ty: &str, v: V) {
    let keys: Vec<u64> = vm
        .field_keys_named(name, ty)
        .into_iter()
        .map(|(_, g)| g)
        .collect();
    if let Some(fields) = fields_mut(vm, h) {
        for g in keys {
            fields.insert(g, v);
        }
    }
}

fn push_fragment_unique(vm: &mut Vm, list: u32, frag: u32) -> bool {
    if let Some(items) = list_mut(vm, list) {
        if !items.contains(&V::Ref(frag)) {
            items.push(V::Ref(frag));
        }
        return true;
    }
    false
}

/// Register in the FragmentStore's added-fragments ArrayList. The exact
/// field names cover normal AndroidX dex; the one-level structural scan
/// covers R8 class merging/renaming by looking for the store shape (a
/// small object holding both a List and a HashMap-like object).
fn register_static_fragment(vm: &mut Vm, manager: u32, frag: u32) -> bool {
    if let Some(V::Ref(store)) = vm.get_object_field(
        manager,
        "Landroidx/fragment/app/FragmentManagerImpl;",
        "mFragmentStore",
    ) {
        if let Some(V::Ref(list)) = vm.get_object_field(
            store,
            "Landroidx/emoji2/text/MetadataRepo;",
            "mMetadataList",
        ) {
            if push_fragment_unique(vm, list, frag) {
                return true;
            }
        }
    }
    let stores = instance_refs(vm, manager);
    for store in stores {
        let fields = instance_refs(vm, store);
        let has_map = fields
            .iter()
            .any(|r| class_of(vm, V::Ref(*r)).contains("Map"));
        if !has_map {
            continue;
        }
        for list in fields {
            if list_ref(vm, list).is_some() && push_fragment_unique(vm, list, frag) {
                return true;
            }
        }
    }
    false
}

fn new_fragment_host_callbacks(vm: &mut Vm, activity: u32) -> Option<u32> {
    let desc = "Landroidx/fragment/app/FragmentActivity$HostCallbacks;";
    vm.type_index_of(desc)?;
    let (midx, _) = vm
        .ctor_list(desc)
        .iter()
        .find(|(_, ps)| ps.len() == 1)
        .cloned()?;
    vm.instantiate_ctor(desc, midx, &[V::Ref(activity)]).ok()
}

/// Instantiate and wire a static `<fragment>` enough for the dex
/// FragmentManager's own lookup/lifecycle code to use it. AndroidX in
/// these APKs is often class-merged or partially obfuscated, so the
/// exact field writes are best-effort; the fragment list registration is
/// the part `findFragmentById` depends on.
fn inflate_static_fragment(
    vm: &mut Vm,
    class_name: &str,
    id: i32,
    ctx: V,
    container: u32,
    attrs: V,
) {
    let desc = format!("L{};", class_name.replace('.', "/"));
    if vm.type_index_of(&desc).is_none() {
        return;
    }
    let ctors = vm.ctor_list(&desc);
    let Some((midx, _params)) = ctors.iter().find(|(_, ps)| ps.is_empty()).cloned() else {
        return;
    };
    let Ok(frag) = vm.instantiate_ctor(&desc, midx, &[]) else {
        return;
    };
    let mut host = 0u32;
    let mut manager = 0u32;
    if let V::Ref(act) = ctx {
        if let Some(V::Ref(frags)) = vm.get_object_field(
            act,
            "Landroidx/appcompat/app/AppCompatActivity;",
            "mFragments",
        ) {
            if let Some(V::Ref(hc)) =
                vm.get_object_field(frags, "Landroidx/fragment/app/Fragment$7;", "this$0")
            {
                host = hc;
                if let Some(V::Ref(m)) = vm.get_object_field(
                    hc,
                    "Landroidx/fragment/app/FragmentActivity$HostCallbacks;",
                    "mFragmentManager",
                ) {
                    manager = m;
                }
            }
        }
        if host == 0 {
            host = find_named_object(vm, act, "HostCallbacks").unwrap_or(0);
        }
        if manager == 0 {
            if let Ok(Some(V::Ref(m))) = vm.call_virtual(act, "getSupportFragmentManager", &[]) {
                manager = m;
            }
        }
        if manager == 0 {
            manager = find_named_object(vm, act, "FragmentManagerImpl").unwrap_or(0);
        }
        if manager == 0 && host != 0 {
            manager = find_named_object(vm, host, "FragmentManagerImpl").unwrap_or(0);
        }
        if host == 0 {
            if let Some(hc) = new_fragment_host_callbacks(vm, act) {
                host = hc;
                if manager == 0 {
                    manager = find_named_object(vm, host, "FragmentManagerImpl").unwrap_or(0);
                }
            }
        }
    }

    let fdesc = "Landroidx/fragment/app/Fragment;";
    set_named_field_all(vm, frag, "mFragmentId", "I", V::I32(id));
    set_named_field_all(vm, frag, "mContainerId", "I", V::I32(id));
    if host != 0 {
        set_named_field_all(
            vm,
            frag,
            "mHost",
            "Landroidx/fragment/app/FragmentActivity$HostCallbacks;",
            V::Ref(host),
        );
    }
    set_named_field_all(vm, frag, "mAdded", "Z", V::I32(1));
    set_named_field_all(vm, frag, "mRemoving", "Z", V::I32(0));
    set_named_field_all(vm, frag, "mDetached", "Z", V::I32(0));
    if manager != 0 {
        set_named_field_all(
            vm,
            frag,
            "mFragmentManager",
            "Landroidx/fragment/app/FragmentManagerImpl;",
            V::Ref(manager),
        );
        register_static_fragment(vm, manager, frag);
    }

    if let Some(V::Ref(child_mgr)) = vm.get_object_field(frag, fdesc, "mChildFragmentManager") {
        if host != 0 {
            vm.set_object_field(
                child_mgr,
                "Landroidx/fragment/app/FragmentManagerImpl;",
                "mHost",
                V::Ref(host),
            );
        }
        vm.set_object_field(
            child_mgr,
            "Landroidx/fragment/app/FragmentManagerImpl;",
            "mParent",
            V::Ref(frag),
        );
        let missing_nc = matches!(
            vm.get_object_field(
                child_mgr,
                "Landroidx/fragment/app/FragmentManagerImpl;",
                "mNonConfig",
            ),
            None | Some(V::Ref(0)) | Some(V::I32(0))
        );
        if missing_nc {
            let ncd = "Landroidx/fragment/app/FragmentManagerViewModel;";
            if let Some((ncctor, _)) = vm
                .ctor_list(ncd)
                .iter()
                .find(|(_, ps)| ps.as_slice() == ["Z"])
                .cloned()
            {
                if let Ok(nc) = vm.instantiate_ctor(ncd, ncctor, &[V::I32(0)]) {
                    vm.set_object_field(
                        child_mgr,
                        "Landroidx/fragment/app/FragmentManagerImpl;",
                        "mNonConfig",
                        V::Ref(nc),
                    );
                }
            }
        }
    }
    let _ = vm.call_virtual_sig(
        frag,
        "onInflate",
        &[
            "Landroid/content/Context;",
            "Landroid/util/AttributeSet;",
            "Landroid/os/Bundle;",
        ],
        &[ctx, attrs, V::Ref(0)],
    );
    let ra = vm.call_virtual(frag, "onAttach", &[ctx]);
    let rc = vm.call_virtual(frag, "onCreate", &[V::Ref(0)]);
    if crate::dbg_flag("OVELO_FRAGDBG") {
        eprintln!("[fragdbg] {class_name} frag h{frag} attach={ra:?} create={rc:?} host{host} mgr{manager}");
    }
    let inf = inflater(vm, Some(ctx));
    let create_args = [inf, V::Ref(container), V::Ref(0)];
    let performed = vm
        .call_virtual(frag, "performCreateView", &create_args)
        .is_ok();
    let mut fv = match vm.get_object_field(frag, fdesc, "mView") {
        Some(V::Ref(v)) if v != 0 => v,
        _ => 0,
    };
    if fv == 0 || !performed {
        if let Ok(Some(V::Ref(direct))) = vm.call_virtual(frag, "onCreateView", &create_args) {
            fv = direct;
            set_named_field_all(vm, frag, "mView", "Landroid/view/View;", V::Ref(fv));
        }
    }
    if fv != 0 {
        if pseudo_get(vm, fv, P_PARENT).is_none() {
            attach_at(vm, container, V::Ref(fv), None, false, None);
        }
        let _ = vm.call_virtual(frag, "onViewCreated", &[V::Ref(fv), V::Ref(0)]);
    }
}

/// `AttributeSet` host holding an element's XML attributes
/// (local name → decoded value), so `obtainStyledAttributes` can map
/// styleable attr ids back to their `res/` values.
pub(crate) fn attrset_of(vm: &mut Vm, el: &Element) -> V {
    let mut m: Vec<(V, V)> = Vec::with_capacity(el.attrs.len());
    for a in &el.attrs {
        let k = jstr(vm, &a.name);
        m.push((k, attr_v(vm, &a.value)));
    }
    V::Ref(vm.alloc_host("Landroid/util/AttributeSet;", HostState::Map(m)))
}

pub(crate) fn attr_v(vm: &mut Vm, v: &Value) -> V {
    match v {
        Value::Reference(r) => V::I32(*r as i32),
        Value::Int(i) => V::I32(*i as i32),
        Value::Bool(b) => V::I32(*b as i32),
        Value::Float(f) => V::F32(*f),
        Value::Str(s) => jstr(vm, s),
        Value::Other { data_type, data } => {
            // Dimension/fraction: decode to px-ish float so getDimension
            // and friends see real values, not raw complex ints.
            if *data_type == 0x05 || *data_type == 0x06 {
                V::F32(crate::res::typed_px(&ovelo_apk::arsc::EntryValue {
                    data_type: *data_type,
                    data: *data,
                    string: None,
                }))
            } else {
                V::I32(*data as i32)
            }
        }
        Value::Null => V::Ref(0),
    }
}

/// arsc `EntryValue` → runtime value (string→jstr, dimension→px float,
/// reference/int/bool/float as ints or floats).
fn entry_v(vm: &mut Vm, ev: &ovelo_apk::arsc::EntryValue) -> V {
    match ev.data_type {
        0x03 => ev
            .string
            .as_deref()
            .map(|s| jstr(vm, s))
            .unwrap_or(V::Ref(0)),
        0x04 => V::F32(f32::from_bits(ev.data)),
        0x05 | 0x06 => V::F32(crate::res::typed_px(ev)),
        _ => V::I32(ev.data as i32),
    }
}

/// A `TypedArray` bound to the `AttributeSet` + int[] styleable array
/// it was obtained over (theme-scoped when no set was passed).
pub(crate) fn typed_array(vm: &mut Vm, args: &[V]) -> V {
    let mut arr_h = 0u32;
    let mut set_h = 0u32;
    let mut arr_pos = usize::MAX;
    let mut ints_before: Vec<i64> = Vec::new();
    let mut ints_after: Vec<i64> = Vec::new();
    for (i, a) in args.iter().enumerate().skip(1) {
        match a {
            V::Ref(h) => match vm.heap.get(*h) {
                Some(Obj::Array { .. }) if arr_h == 0 => {
                    arr_h = *h;
                    arr_pos = i;
                }
                Some(Obj::Host { desc, .. })
                    if desc == "Landroid/util/AttributeSet;"
                        || desc == "Landroid/content/res/XmlResourceParser;" =>
                {
                    set_h = *h;
                }
                _ => {}
            },
            v @ (V::I32(_) | V::I64(_)) => {
                if i < arr_pos {
                    ints_before.push(i64v(*v));
                } else {
                    ints_after.push(i64v(*v));
                }
            }
            _ => {}
        }
    }
    let h = vm.alloc_host("Landroid/content/res/TypedArray;", HostState::Empty);
    pseudo_set(vm, h, P_TA_ATTRS, V::Ref(arr_h));
    pseudo_set(vm, h, P_TA_SET, V::Ref(set_h));
    // Int args before the styleable array are an explicit style res id
    // (`obtainStyledAttributes(resId, attrs)`); after it, `defStyleAttr`
    // and `defStyleRes`.
    if let Some(&s) = ints_before.first() {
        pseudo_set(vm, h, P_TA_STYLE, V::I32(s as i32));
    }
    if let Some(&a) = ints_after.first() {
        pseudo_set(vm, h, P_TA_DEF_ATTR, V::I32(a as i32));
    }
    if let Some(&r) = ints_after.get(1) {
        pseudo_set(vm, h, P_TA_DEF_RES, V::I32(r as i32));
    }
    V::Ref(h)
}

/// Styleable attr ids (`R.styleable.*`) a TypedArray indexes into.
pub(crate) fn ta_attrs(vm: &Vm, h: u32) -> Vec<i32> {
    let Some(V::Ref(ah)) = pseudo_get(vm, h, P_TA_ATTRS) else {
        return Vec::new();
    };
    match vm.heap.get(ah) {
        Some(Obj::Array { data, .. }) => data.iter().map(|v| i64v(*v) as i32).collect(),
        _ => Vec::new(),
    }
}

/// Styleable index → the value stored in the backing `AttributeSet`
/// under the attr's entry name (app `resources.arsc` or the framework
/// attr table for `android:` ids), falling back to the style/theme
/// resolution chain for theme-scoped arrays and unset attrs.
/// `None` when the value can't be resolved anywhere.
pub(crate) fn ta_lookup(vm: &mut Vm, h: u32, index: usize) -> Option<V> {
    if ta_attrs(vm, h).get(index).is_none() && crate::dbg_flag("OVELO_VIEWDBG") {
        let n = ta_attrs(vm, h).len();
        eprintln!("[viewdbg] ta_lookup[{index}] out-of-range attrs_len={n} h={h}");
    }
    let attr_id = *ta_attrs(vm, h).get(index)? as u32;
    if crate::dbg_flag("OVELO_VIEWDBG") {
        eprintln!(
            "[viewdbg] ta_lookup[{index}] attr_id={attr_id:#x} name={:?}",
            vm.apk_res
                .as_ref()
                .and_then(|r| r.attr_name(attr_id).map(|s| s.to_string()))
        );
    }
    // 1) Explicit AttributeSet: match by entry name (app attrs and
    //    framework attrs share the local-name space). An
    //    XmlResourceParser doubles as AttributeSet — query the single
    //    attr by name instead of materializing the whole list.
    let name = vm
        .apk_res
        .as_ref()?
        .attr_name(attr_id)
        .map(|s| s.to_string());
    if let Some(name) = name {
        if let Some(V::Ref(set_h)) = pseudo_get(vm, h, P_TA_SET) {
            let is_xml = matches!(
                vm.heap.get(set_h),
                Some(Obj::Host {
                    state: HostState::Xml { .. },
                    ..
                })
            );
            if let Some(v) = xml::cur_attr_value(vm, set_h, &name) {
                if crate::dbg_flag("OVELO_VIEWDBG") {
                    eprintln!("[viewdbg]   ta_hit {name} = {v:?}");
                }
                return Some(v);
            }
            if crate::dbg_flag("OVELO_VIEWDBG") {
                eprintln!("[viewdbg]   ta_miss {name} set={set_h} xml={is_xml}");
            }
        } else if crate::dbg_flag("OVELO_VIEWDBG") {
            eprintln!("[viewdbg]   ta_noset {name}");
        }
        if let Some(V::Ref(set_h)) = pseudo_get(vm, h, P_TA_SET) {
            if let Some(Obj::Host {
                state: HostState::Map(m),
                ..
            }) = vm.heap.get(set_h)
            {
                if let Some(v) = m.iter().find_map(|(k, v)| {
                    if sv(vm, *k).as_deref() == Some(name.as_str()) {
                        Some(*v)
                    } else {
                        None
                    }
                }) {
                    if crate::dbg_flag("OVELO_VIEWDBG") {
                        eprintln!("[viewdbg]   ta_map_hit {name} = {v:?}");
                    }
                    return Some(v);
                }
                if crate::dbg_flag("OVELO_VIEWDBG") {
                    let keys: Vec<String> = m.iter().filter_map(|(k, _)| sv(vm, *k)).collect();
                    eprintln!("[viewdbg]   ta_map_miss {name} set={set_h} keys={keys:?}");
                }
            }
        }
    }
    let res = vm.apk_res.as_ref()?;
    // A style hit may itself be `?attr/x` (TYPE_ATTRIBUTE) — chase it
    // through the theme chain so `indicatorColor=?attr/colorPrimary`
    // lands on the real value.
    macro_rules! style_hit {
        ($ev:expr) => {{
            let mut ev = $ev.clone();
            for _ in 0..8 {
                if ev.data_type != 0x02 {
                    break;
                }
                match res.style_item(vm.theme_id, ev.data) {
                    Some(next) => ev = next.clone(),
                    None => break,
                }
            }
            if ev.data_type != 0x02 {
                return Some(entry_v(vm, &ev));
            }
        }};
    }
    // 2) Explicit style (obtainStyledAttributes(resId, attrs)).
    if let Some(V::I32(s)) = pseudo_get(vm, h, P_TA_STYLE) {
        if let Some(ev) = res.style_item(s as u32, attr_id) {
            style_hit!(ev);
        }
    }
    // 3) defStyleAttr: an attr whose theme value is a style reference.
    if let Some(V::I32(a)) = pseudo_get(vm, h, P_TA_DEF_ATTR) {
        if let Some(style_ref) = res.style_item(vm.theme_id, a as u32).map(|ev| ev.data) {
            if let Some(ev) = res.style_item(style_ref, attr_id) {
                style_hit!(ev);
            }
        }
    }
    // 4) defStyleRes: an explicit style resource.
    if let Some(V::I32(r)) = pseudo_get(vm, h, P_TA_DEF_RES) {
        if let Some(ev) = res.style_item(r as u32, attr_id) {
            style_hit!(ev);
        }
    }
    // 5) The Activity theme (manifest `android:theme`).
    if let Some(ev) = res.style_item(vm.theme_id, attr_id) {
        style_hit!(ev);
    }
    None
}

/// `Theme.resolveAttribute` backing: look `attr_id` up in the Activity
/// theme (parents walked by `style_item`), chasing `?attr/`
/// indirections, and return the raw entry so the caller keeps the
/// exact TypedValue type (`TYPE_INT_BOOLEAN` etc.).
pub(crate) fn theme_attr_entry(vm: &Vm, attr_id: u32) -> Option<ovelo_apk::arsc::EntryValue> {
    let res = vm.apk_res.as_ref()?;
    let mut ev = res.style_item(vm.theme_id, attr_id)?.clone();
    for _ in 0..8 {
        if ev.data_type != 0x02 {
            break;
        }
        match res.style_item(vm.theme_id, ev.data) {
            Some(next) => ev = next.clone(),
            None => break,
        }
    }
    if ev.data_type == 0x02 {
        return None;
    }
    Some(ev)
}

/// Resolve a TypedArray integer: `?attr/x` values surface as the attr's
/// own res id, but the theme-binding step is supposed to rewrite them —
/// chase those through the bound theme, then follow @ref hops to the
/// final entry. Returns `(resolved res_id, final entry value)`.
pub(crate) fn resolve_ta_id(vm: &mut Vm, id: u32) -> Option<(u32, ovelo_apk::arsc::EntryValue)> {
    let mut ev = match vm.apk_res.as_ref().and_then(|r| r.type_name(id)) {
        Some("attr") => theme_attr_entry(vm, id)?,
        _ => vm.apk_res.as_ref()?.entry_value(id)?.clone(),
    };
    let mut rid = id;
    for _ in 0..8 {
        if ev.data_type != 0x01 {
            break;
        }
        rid = ev.data;
        match vm.apk_res.as_ref().and_then(|r| r.entry_value(rid)) {
            Some(next) => ev = next.clone(),
            None => break,
        }
    }
    Some((rid, ev))
}

/// Instantiate a dex-defined view class and run its best-matching
/// constructor; `None` when the class isn't in the DEX or every
/// overload dies (caller falls back to the host stub). Overloads are
/// scored by how many params we can't supply real values for —
/// `(Context, AttributeSet[, int])` beats foreign types, which get
/// typed placeholder hosts (e.g. FragmentManager in
/// FragmentContainerView's 3-arg ctor).
fn try_construct_view(vm: &mut Vm, desc: &str, ctx: V, attrs: V) -> Option<u32> {
    if vm.type_index_of(desc).is_none() {
        if crate::dbg_flag("OVELO_VIEWDBG") {
            eprintln!("[viewdbg] {desc} not in dex → host stub");
        }
        return None;
    }
    let mut ctors = vm.ctor_list(desc);
    // Score: params needing typed placeholders (fewer is better), then
    // longer signatures (more real state wired up) preferred.
    ctors.sort_by_key(|(_, ps)| (fill_count(ps), usize::MAX - ps.len()));
    for (midx, params) in ctors {
        let args: Vec<V> = params.iter().map(|t| ctor_arg(vm, t, ctx, attrs)).collect();
        match vm.instantiate_ctor(desc, midx, &args) {
            Ok(h) => return Some(h),
            Err(e) => {
                if crate::dbg_flag("OVELO_VIEWDBG") {
                    eprintln!("[viewdbg] ctor {desc}({}) failed: {e}", params.join(","));
                }
            }
        }
    }
    None
}

/// Params in a ctor proto that aren't `Context`/`AttributeSet` — the
/// score `try_construct_view` sorts by (primitive defStyle ints are
/// fine to zero-fill).
fn fill_count(params: &[String]) -> usize {
    params
        .iter()
        .filter(|t| {
            !matches!(
                t.as_str(),
                "Landroid/content/Context;" | "Landroid/util/AttributeSet;"
            ) && !t.starts_with(['I', 'J', 'F', 'D', 'Z', 'B', 'S', 'C'])
        })
        .count()
}

/// Constructor argument for one declared param type: real `ctx`/`attrs`
/// for the standard slots, zero for primitives, a typed placeholder
/// host object for foreign reference types.
fn ctor_arg(vm: &mut Vm, t: &str, ctx: V, attrs: V) -> V {
    match t {
        "Landroid/content/Context;" => ctx,
        "Landroid/util/AttributeSet;" => attrs,
        "J" => V::I64(0),
        "D" => V::F64(0.0),
        "I" | "F" | "Z" | "B" | "S" | "C" => V::I32(0),
        _ if t.starts_with('L') || t.starts_with('[') => host(vm, t, HostState::Empty),
        _ => V::I32(0),
    }
}

/// `getLayoutParams`: a params object carrying the XML layout attrs.
/// Prefers the guest-set object, then the parent's `* $LayoutParams`
/// dex class (its real ctor reads `layout_gravity` & friends from the
/// stored `AttributeSet`), then a bare host placeholder.
fn layout_params(vm: &mut Vm, this: u32) -> V {
    if let Some(v @ V::Ref(h)) = pseudo_get(vm, this, P_LP) {
        if h != 0 {
            return v;
        }
    }
    let attrs = pseudo_get(vm, this, P_ATTRS).unwrap_or(V::Ref(0));
    let ctx = pseudo_get(vm, this, P_CONTEXT).unwrap_or(V::Ref(0));
    if let Some(V::Ref(p)) = pseudo_get(vm, this, P_PARENT) {
        // The real Android path: the parent creates the child's params
        // via `generateLayoutParams(AttributeSet)` — dex-defined for
        // bundled groups like DrawerLayout, whose R8-inlined ctor body
        // is the only place `layout_gravity` is read.
        // Signature-exact: `generateLayoutParams` overloads on attrs
        // and on LayoutParams share a slot count — arity lookup can
        // hit the copy-ctor with an AttributeSet and die mid-body.
        let lp_v = vm.call_virtual_sig(
            p,
            "generateLayoutParams",
            &["Landroid/util/AttributeSet;"],
            &[attrs],
        );
        if crate::dbg_flag("OVELO_VIEWDBG") {
            match &lp_v {
                Err(e) => eprintln!("[viewdbg] generateLayoutParams h{this} -> {e}"),
                Ok(v) => eprintln!(
                    "[viewdbg] generateLayoutParams h{this} -> {v:?} (parent {})",
                    class_of(vm, V::Ref(p))
                ),
            }
        }
        if let Ok(Some(v @ V::Ref(h))) = lp_v {
            if h != 0 {
                pseudo_set(vm, this, P_LP, v);
                return v;
            }
        }
        // Attr parsing may have thrown — `generateDefaultLayoutParams`
        // is the attr-free sibling and still fully initializes the
        // params object (ConstraintLayout$LayoutParams' widget, …).
        let dlp = vm.call_virtual_sig(p, "generateDefaultLayoutParams", &[], &[]);
        if crate::dbg_flag("OVELO_VIEWDBG") {
            match &dlp {
                Err(e) => eprintln!("[viewdbg] generateDefaultLayoutParams h{this} -> {e}"),
                Ok(v) => eprintln!("[viewdbg] generateDefaultLayoutParams h{this} -> {v:?}"),
            }
        }
        if let Ok(Some(v @ V::Ref(h))) = dlp {
            if h != 0 {
                pseudo_set(vm, this, P_LP, v);
                return v;
            }
        }
        {
            let mut desc = class_of(vm, V::Ref(p));
            for _ in 0..16 {
                let lp = format!("{}$LayoutParams;", desc.trim_end_matches(';'));
                if vm.type_index_of(&lp).is_some() {
                    match vm.instantiate_with(&lp, &[ctx, attrs]) {
                        Ok(h) => {
                            pseudo_set(vm, this, P_LP, V::Ref(h));
                            return V::Ref(h);
                        }
                        Err(e) => {
                            if crate::dbg_flag("OVELO_VIEWDBG") {
                                eprintln!("[viewdbg] LP {lp} ctor failed: {e}");
                            }
                        }
                    }
                }
                match vm.super_desc(&desc) {
                    Some(s) => desc = s,
                    None => break,
                }
            }
        }
    }
    // Last resort: a host LP, but populated from the child's attrs via
    // lp_ctor — an empty placeholder leaves `iget lp.width` reading 0.
    let v = match pseudo_get(vm, this, P_PARENT) {
        Some(V::Ref(p)) if p != 0 => gen_lp(vm, p, attrs),
        _ => host(
            vm,
            "Landroid/view/ViewGroup$LayoutParams;",
            HostState::Empty,
        ),
    };
    pseudo_set(vm, this, P_LP, v);
    if crate::dbg_flag("OVELO_VIEWDBG") {
        let p = match pseudo_get(vm, this, P_PARENT) {
            Some(V::Ref(h)) if h != 0 => class_of(vm, V::Ref(h)),
            _ => "none".into(),
        };
        eprintln!(
            "[viewdbg] layout_params h{this} {} parent={p} -> host placeholder",
            class_of(vm, V::Ref(this))
        );
    }
    v
}

/// XML attr name → `LayoutParams`/`MarginLayoutParams` field names it
/// feeds (framework ctor semantics). `weight` is `F`, the rest `I`.
const LP_ATTRS: &[(&str, &[&str])] = &[
    ("layout_width", &["width"]),
    ("layout_height", &["height"]),
    ("layout_gravity", &["gravity"]),
    ("layout_weight", &["weight"]),
    (
        "layout_margin",
        &["leftMargin", "topMargin", "rightMargin", "bottomMargin"],
    ),
    ("layout_marginLeft", &["leftMargin"]),
    ("layout_marginTop", &["topMargin"]),
    ("layout_marginRight", &["rightMargin"]),
    ("layout_marginBottom", &["bottomMargin"]),
    ("layout_marginStart", &["startMargin"]),
    ("layout_marginEnd", &["endMargin"]),
    ("layout_marginHorizontal", &["leftMargin", "rightMargin"]),
    ("layout_marginVertical", &["topMargin", "bottomMargin"]),
];

/// Value on an `AttributeSet` by local attr name — Map-backed sets and
/// live `XmlResourceParser`s alike.
fn attrset_value(vm: &mut Vm, set: V, name: &str) -> Option<V> {
    let V::Ref(h) = set else { return None };
    match vm.heap.get(h) {
        Some(Obj::Host {
            state: HostState::Map(m),
            ..
        }) => {
            let v = m.iter().find_map(|(k, v)| {
                if sv(vm, *k).as_deref() == Some(name) {
                    Some(*v)
                } else {
                    None
                }
            })?;
            Some(v)
        }
        Some(Obj::Host {
            state: HostState::Xml { .. },
            ..
        }) => xml::cur_attr_value(vm, h, name),
        _ => None,
    }
}

/// `LinearLayout` orientation: `setOrientation` wins, else the
/// `android:orientation` attr (0=horizontal, 1=vertical); the AOSP
/// documented default is horizontal — except `TableLayout` and
/// `RadioGroup`, whose constructors force VERTICAL, and `TableRow`,
/// forced HORIZONTAL.
fn linear_vertical(vm: &Vm, this: u32) -> bool {
    if let Some(V::I32(o)) = pseudo_get(vm, this, P_ORIENTATION) {
        return o != 0;
    }
    // XML orientation overrides the constructor default. RadioGroup's
    // constructor always starts VERTICAL; its zero-initialized host/dex
    // field must not erase that default when the constructor body is not
    // executed through the host shim.
    if let Some(v) = attr_find(vm, this, "orientation") {
        return i32v(v) != 0;
    }
    let desc = class_of(vm, V::Ref(this));
    if desc.contains("RadioGroup") {
        return true;
    }
    // AndroidX LinearLayoutCompat implements orientation dex-side, so
    // the host setter never sees constructor-driven VERTICAL. Read the
    // field across its possible declaring owners.
    for owner in [
        "Landroidx/appcompat/widget/LinearLayoutCompat;",
        "Landroid/widget/LinearLayout;",
    ] {
        if let Some(V::I32(o)) = vm.get_object_field(this, owner, "mOrientation") {
            return o != 0;
        }
    }
    desc.contains("TableLayout")
}

/// `getMinimum*`/`getSuggestedMinimum*`: explicit `setMinimum*` wins,
/// else the inflated `android:minWidth`/`minHeight` attr.
fn min_dim(vm: &mut Vm, this: u32, key: u32, attr: &str) -> i32 {
    // A stored value wins even when it is 0 — `setMinimumWidth(0)`
    // is how guest code *clears* the inflated minimum.
    if let Some(v) = pseudo_get(vm, this, key) {
        return i32v(v);
    }
    attr_find(vm, this, attr)
        .and_then(|v| lp_px(vm, v))
        .unwrap_or(0)
}

fn theme_style_ref(res: &crate::res::ApkResources, theme_id: u32, attr_id: u32) -> Option<u32> {
    // Theme attrs often indirect through another attr (`textInputStyle`
    // = `?attr/textInputOutlinedStyle`) before landing on the style
    // reference. Chase that chain here; only the final reference is a
    // style resource id.
    let mut cur = res.style_item(theme_id, attr_id).cloned();
    for _ in 0..8 {
        let ev = cur?;
        if ev.data_type != 0x02 {
            return (ev.data != 0).then_some(ev.data);
        }
        cur = res.style_item(theme_id, ev.data).cloned();
    }
    None
}

fn ctor_style_value(
    vm: &mut Vm,
    xml_style: u32,
    def_attr: u32,
    def_res: u32,
    attr_id: u32,
) -> Option<V> {
    let res = vm.apk_res.as_ref()?;
    let mut styles = Vec::new();
    if xml_style != 0 {
        styles.push(xml_style);
    }
    if def_attr != 0 {
        if let Some(style) = theme_style_ref(res, vm.theme_id, def_attr) {
            styles.push(style);
        }
    }
    if def_res != 0 {
        styles.push(def_res);
    }
    for style in styles {
        let Some(first) = res.style_item(style, attr_id) else {
            continue;
        };
        let mut ev = first.clone();
        for _ in 0..8 {
            if ev.data_type != 0x02 {
                break;
            }
            match res.style_item(vm.theme_id, ev.data) {
                Some(next) => ev = next.clone(),
                None => break,
            }
        }
        if ev.data_type != 0x02 {
            return Some(entry_v(vm, &ev));
        }
    }
    None
}

/// Apply the constructor-default style values that the real framework
/// View/TextView constructors read from `defStyleAttr`/`defStyleRes`.
/// Appcompat widgets pass `editTextStyle`/`buttonStyle`/… down to their
/// framework super constructor; without this the XML inflation path only
/// sees attributes written literally in the layout.
fn apply_ctor_style(vm: &mut Vm, this: u32, args: &[V]) {
    if args.len() < 4 {
        return;
    }
    let xml_style_raw = attr_find(vm, this, "style")
        .map(|v| i32v(v) as u32)
        .unwrap_or(0);
    let xml_style = if xml_style_raw != 0 {
        match vm.apk_res.as_ref() {
            Some(res)
                if res.type_name(xml_style_raw) == Some("attr")
                    || crate::res_attrs::framework_attr_name(xml_style_raw).is_some() =>
            {
                theme_style_ref(res, vm.theme_id, xml_style_raw).unwrap_or(0)
            }
            _ => xml_style_raw,
        }
    } else {
        0
    };
    let def_attr = i32v(args.get(3).copied().unwrap_or(V::I32(0))) as u32;
    let def_res = i32v(args.get(4).copied().unwrap_or(V::I32(0))) as u32;
    if xml_style == 0 && def_attr == 0 && def_res == 0 {
        return;
    }
    for (attr_id, key, name) in [
        (0x0101_013f, P_MINW, "minWidth"),
        (0x0101_0140, P_MINH, "minHeight"),
    ] {
        if pseudo_get(vm, this, key).is_none() && attr_find(vm, this, name).is_none() {
            if let Some(px) = ctor_style_value(vm, xml_style, def_attr, def_res, attr_id)
                .and_then(|v| lp_px(vm, v))
            {
                pseudo_set(vm, this, key, V::I32(px));
            }
        }
    }
    let base = ctor_style_value(vm, xml_style, def_attr, def_res, 0x0101_00d5)
        .and_then(|v| lp_px(vm, v))
        .unwrap_or(0);
    let horizontal = ctor_style_value(vm, xml_style, def_attr, def_res, 0x0101_053d)
        .and_then(|v| lp_px(vm, v))
        .unwrap_or(base);
    let vertical = ctor_style_value(vm, xml_style, def_attr, def_res, 0x0101_053e)
        .and_then(|v| lp_px(vm, v))
        .unwrap_or(base);
    for (attr_id, key, name, fallback) in [
        (0x0101_00d6, P_PAD_L, "paddingLeft", horizontal),
        (0x0101_00d7, P_PAD_T, "paddingTop", vertical),
        (0x0101_00d8, P_PAD_R, "paddingRight", horizontal),
        (0x0101_00d9, P_PAD_B, "paddingBottom", vertical),
    ] {
        let axis = if name == "paddingLeft" || name == "paddingRight" {
            "paddingHorizontal"
        } else {
            "paddingVertical"
        };
        let relative = if name == "paddingLeft" {
            Some("paddingStart")
        } else if name == "paddingRight" {
            Some("paddingEnd")
        } else {
            None
        };
        let xml_has_padding = attr_find(vm, this, name).is_some()
            || attr_find(vm, this, "padding").is_some()
            || attr_find(vm, this, axis).is_some()
            || relative.is_some_and(|rel| attr_find(vm, this, rel).is_some());
        if pseudo_get(vm, this, key).is_none() && !xml_has_padding {
            let mut styled = ctor_style_value(vm, xml_style, def_attr, def_res, attr_id);
            if styled.is_none() {
                let relative_id = if key == P_PAD_L {
                    0x0101_03b3
                } else if key == P_PAD_R {
                    0x0101_03b4
                } else {
                    0
                };
                if relative_id != 0 {
                    styled = ctor_style_value(vm, xml_style, def_attr, def_res, relative_id);
                }
            }
            let px = styled.and_then(|v| lp_px(vm, v)).unwrap_or(fallback);
            if px != 0 {
                pseudo_set(vm, this, key, V::I32(px));
            }
        }
    }
    if pseudo_get(vm, this, P_TSIZE).is_none() && attr_find(vm, this, "textSize").is_none() {
        if let Some(px) = ctor_style_value(vm, xml_style, def_attr, def_res, 0x0101_0095)
            .and_then(|v| lp_px(vm, v))
        {
            pseudo_set(vm, this, P_TSIZE, V::F32(px as f32));
        }
    }
}

/// AOSP `View.resolveSize` / `View.getDefaultSize`: fold a view's
/// content size against the spec mode. `resolve=true` picks the
/// resolveSize rule (AT_MOST → min(size, specSize)); getDefaultSize
/// returns the spec size under both AT_MOST and EXACTLY.
fn resolve_size(size: i32, spec: i32, resolve: bool) -> i32 {
    const MODE_MASK: i32 = 0x3 << 30;
    const EXACTLY: i32 = 1 << 30;
    const AT_MOST: i32 = 2 << 30;
    let spec_size = spec & !MODE_MASK;
    match spec & MODE_MASK {
        m if m == EXACTLY => spec_size,
        m if m == AT_MOST => {
            if resolve {
                size.min(spec_size)
            } else {
                spec_size
            }
        }
        _ => size,
    }
}

/// A dimension-ish runtime value → px int: resource ids resolve
/// through the arsc dimen table, floats are already px.
/// AOSP `ViewGroup.getChildMeasureSpec` packing rules.
fn child_measure_spec(spec: i32, padding: i32, child_dim: i32) -> i32 {
    const MODE_MASK: i32 = 0x3 << 30;
    const EXACTLY: i32 = 1 << 30;
    const AT_MOST: i32 = 2 << 30;
    let size = (spec & !MODE_MASK) - padding;
    let size = if size < 0 { 0 } else { size };
    let (mode, out) = match spec & MODE_MASK {
        m if m == EXACTLY => match child_dim {
            d if d >= 0 => (EXACTLY, d),
            -2 => (AT_MOST, size), // WRAP_CONTENT
            _ => (EXACTLY, size),  // MATCH_PARENT (-1) / unspecified
        },
        m if m == AT_MOST => match child_dim {
            d if d >= 0 => (EXACTLY, d),
            -2 => (AT_MOST, size),
            _ => (AT_MOST, size),
        },
        // UNSPECIFIED: a fixed child dimension still measures EXACTLY
        // (AOSP); MATCH_PARENT/WRAP_CONTENT measure at UNSPECIFIED 0.
        _ => match child_dim {
            d if d >= 0 => (EXACTLY, d),
            _ => (0, 0),
        },
    };
    (out & !MODE_MASK) | (mode & MODE_MASK)
}

/// AOSP `MarginLayoutParams.resolveLayoutDirection`: record the
/// resolved direction and fold relative (start/end) margins into
/// left/right under every canon key a reader can name.
fn lp_resolve_dir(vm: &mut Vm, lp: u32, dir: i32) {
    if lp == 0 {
        return;
    }
    pseudo_set(vm, lp, P_LP_DIR, V::I32(dir));
    // DEFAULT_MARGIN_RELATIVE — "no relative margin set". Relative
    // margins arrive as pseudos (see lp_ctor); a code-side
    // setMarginStart/End may still land on canon fields, so check both.
    const UNSET: i32 = i32::MIN;
    let start = pseudo_get(vm, lp, P_MSTART)
        .or_else(|| lp_field(vm, lp, "startMargin"))
        .map(i32v)
        .unwrap_or(UNSET);
    let end = pseudo_get(vm, lp, P_MEND)
        .or_else(|| lp_field(vm, lp, "endMargin"))
        .map(i32v)
        .unwrap_or(UNSET);
    if start == UNSET && end == UNSET {
        return;
    }
    let rtl = dir == 1;
    let left = if rtl { end } else { start };
    let right = if rtl { start } else { end };
    for (name, v) in [("leftMargin", left), ("rightMargin", right)] {
        if v == UNSET {
            continue;
        }
        let keys: Vec<GIdx> = vm
            .field_keys_named(name, "I")
            .into_iter()
            .map(|(_, g)| g)
            .collect();
        if let Some(f) = fields_mut(vm, lp) {
            for g in keys {
                f.insert(g, V::I32(v));
            }
        }
    }
}

/// Run the dex `LayoutParams.resolveLayoutDirection` override when the
/// LP class has one (ConstraintLayout resolves its anchors there).
/// No-op for LP classes without the override.
fn lp_resolve_dir_dex(vm: &mut Vm, lp: V) {
    let V::Ref(h) = lp else { return };
    if h == 0 {
        return;
    }
    // Host layout direction is LTR (getLayoutDirection → 0).
    let _ = vm.call_virtual_sig(h, "resolveLayoutDirection", &["I"], &[V::I32(0)]);
}

/// Read an int field off a LayoutParams object under every canon key
/// a writer could have used (owner obfuscation, LP subclasses).
pub(crate) fn lp_field(vm: &Vm, lp: u32, name: &str) -> Option<V> {
    lp_field_t(vm, lp, name, "I")
}

/// `(width, height)` requested by a dex LayoutParams object — `-1`
/// match_parent / `-2` wrap_content pass through. For the host-side
/// view bridge, which must size bridged children from the LP the app
/// handed to `addView`/`setLayoutParams`.
pub fn lp_dims(vm: &Vm, lp: u32) -> (Option<i32>, Option<i32>) {
    (
        lp_field(vm, lp, "width").and_then(|v| lp_px(vm, v)),
        lp_field(vm, lp, "height").and_then(|v| lp_px(vm, v)),
    )
}

fn lp_field_t(vm: &Vm, lp: u32, name: &str, ty: &str) -> Option<V> {
    let fields = match vm.heap.get(lp) {
        Some(Obj::Instance { fields, .. }) | Some(Obj::Host { fields, .. }) => fields,
        _ => return None,
    };
    let keys = vm.field_keys_named(name, ty);
    // Prefer fields declared on this object's own hierarchy. The global
    // name scan also knows unrelated LayoutParams classes (and hash-map
    // order is unstable), so a first-hit fallback can keep reading the
    // initial aliased value after the real field is updated.
    let mut hier = hashbrown::HashSet::new();
    let mut d = class_of(vm, V::Ref(lp));
    for _ in 0..32 {
        if !hier.insert(d.clone()) {
            break;
        }
        let Some(next) = vm.super_desc(&d) else { break };
        d = next;
    }
    keys.iter()
        .filter(|(owner, _)| hier.contains(owner))
        .find_map(|(_, g)| fields.get(g).copied())
        .or_else(|| keys.iter().find_map(|(_, g)| fields.get(g).copied()))
}

fn lp_px(vm: &Vm, v: V) -> Option<i32> {
    let i = match v {
        V::I32(i) => i,
        V::I64(l) => l as i32,
        V::F32(f) => return Some(f as i32),
        V::F64(f) => return Some(f as i32),
        _ => return None,
    };
    if i >= 0x0100_0000 {
        // A resource id, not a px value — resolve it through the arsc
        // dimen table. An unresolved id must NOT pass through as a
        // literal px dim: it lands in `getChildMeasureSpec` as
        // `EXACTLY|0x7fxxxxxx`, producing ~1e9-px measured heights
        // that push the whole content subtree off-screen.
        let id = i as u32;
        let res = vm.apk_res.as_ref()?;
        if let Some(d) = res.dimen(id) {
            return Some(d as i32);
        }
        // `?attr/foo` (including framework attrs absent from the app's
        // type table) is theme state rather than a dimen resource. XML
        // decoding loses the TYPE_ATTRIBUTE tag, but values in this id
        // range are never literal pixels; resolving a missing theme item
        // to None is still the safe outcome.
        let mut ev = theme_attr_entry(vm, id)?.clone();
        for _ in 0..8 {
            match ev.data_type {
                0x05..=0x06 => return Some(crate::res::typed_px(&ev) as i32),
                0x04 | 0x10 | 0x11 | 0x12 => return Some(crate::res::typed_px(&ev) as i32),
                0x01 => {
                    if let Some(d) = res.dimen(ev.data) {
                        return Some(d as i32);
                    }
                    ev = res.entry_value(ev.data)?.clone();
                }
                0x02 => ev = theme_attr_entry(vm, ev.data)?.clone(),
                _ => return None,
            }
        }
        return None;
    }
    Some(i)
}

fn padding_component(vm: &Vm, h: u32, exact: &str, relative: Option<&str>, base: i32) -> i32 {
    if let Some(v) = attr_find(vm, h, exact) {
        return lp_px(vm, v).unwrap_or(0).max(0);
    }
    if let Some(rel) = relative {
        if let Some(v) = attr_find(vm, h, rel) {
            return lp_px(vm, v).unwrap_or(0).max(0);
        }
    }
    base.max(0)
}

/// View padding in physical LTR order, including uniform `android:padding`.
fn view_padding(vm: &Vm, h: u32) -> (i32, i32, i32, i32) {
    let base = lp_px(vm, attr_find(vm, h, "padding").unwrap_or(V::I32(0))).unwrap_or(0);
    let xml = (
        padding_component(vm, h, "paddingLeft", Some("paddingStart"), base),
        padding_component(vm, h, "paddingTop", None, base),
        padding_component(vm, h, "paddingRight", Some("paddingEnd"), base),
        padding_component(vm, h, "paddingBottom", None, base),
    );
    (
        pseudo_get(vm, h, P_PAD_L).map(i32v).unwrap_or(xml.0),
        pseudo_get(vm, h, P_PAD_T).map(i32v).unwrap_or(xml.1),
        pseudo_get(vm, h, P_PAD_R).map(i32v).unwrap_or(xml.2),
        pseudo_get(vm, h, P_PAD_B).map(i32v).unwrap_or(xml.3),
    )
}

/// Shared `measureChild(WithMargins)` core: reads the child's
/// LayoutParams, builds the child spec per AOSP
/// `getChildMeasureSpec`, and drives `child.measure`. Used by the
/// invoked builtins and the generic framework-group fallback —
/// framework containers (RelativeLayout, FrameLayout, LinearLayout)
/// arrive as skeleton classes with no dex `onMeasure`/`onLayout`, so
/// without a host fallback their children stay 0×0 forever.
#[allow(clippy::too_many_arguments)]
fn measure_child(
    vm: &mut Vm,
    parent: u32,
    child: u32,
    pw: i32,
    w_used: i32,
    ph: i32,
    h_used: i32,
    margins: bool,
) {
    // Child dims/margins come from its LayoutParams; AOSP's
    // default LP is (WRAP, WRAP, 0 margins). `None` = field absent;
    // -2 can be a real WRAP_CONTENT, not a miss.
    if pseudo_get(vm, child, P_POPUP_ROOT).is_some() {
        return;
    }
    let (mut lp_w, mut lp_h) = (None, None);
    let (mut ml, mut mt, mut mr, mut mb) = (0, 0, 0, 0);
    if let Some(V::Ref(lp)) = pseudo_get(vm, child, P_LP) {
        if lp != 0 {
            if let Some(f) = lp_field(vm, lp, "width") {
                lp_w = lp_px(vm, f);
            }
            if let Some(f) = lp_field(vm, lp, "height") {
                lp_h = lp_px(vm, f);
            }
            if margins {
                ml = lp_field(vm, lp, "leftMargin")
                    .and_then(|f| lp_px(vm, f))
                    .unwrap_or(0);
                mt = lp_field(vm, lp, "topMargin")
                    .and_then(|f| lp_px(vm, f))
                    .unwrap_or(0);
                mr = lp_field(vm, lp, "rightMargin")
                    .and_then(|f| lp_px(vm, f))
                    .unwrap_or(0);
                mb = lp_field(vm, lp, "bottomMargin")
                    .and_then(|f| lp_px(vm, f))
                    .unwrap_or(0);
            }
        }
    }
    // Host placeholder LPs carry no dex fields — fall back to the
    // inflated AttributeSet's layout_width/layout_height.
    if lp_w.is_none() {
        if let Some(f) = attr_find(vm, child, "layout_width") {
            lp_w = Some(len_v(vm, f));
        }
    }
    if lp_h.is_none() {
        if let Some(f) = attr_find(vm, child, "layout_height") {
            lp_h = Some(len_v(vm, f));
        }
    }
    let (lp_w, lp_h) = (lp_w.unwrap_or(-2), lp_h.unwrap_or(-2));
    // Parent padding halves the space handed to children.
    let (pl, pt, pr, pb) = view_padding(vm, parent);
    let pad_w = pl + pr;
    let pad_h = pt + pb;
    let mut ws = child_measure_spec(pw, pad_w + ml + mr + w_used, lp_w);
    let mut hs = child_measure_spec(ph, pad_h + mt + mb + h_used, lp_h);
    // ScrollView measures its content with an unbounded scroll axis:
    // the child must be allowed to become taller than the viewport and
    // scroll, rather than compressing wrap_content descendants into the
    // space left over inside the viewport.
    let parent_desc = class_of(vm, V::Ref(parent));
    if parent_desc.contains("HorizontalScrollView") {
        ws = 0;
    } else if parent_desc.contains("ScrollView") {
        hs = 0;
    }
    if crate::dbg_flag("OVELO_VIEWDBG") {
        eprintln!(
            "[viewdbg] measureChild h{child} lp={lp_w}x{lp_h} used={w_used},{h_used} in={pw:#x}/{ph:#x} -> {ws:#x}/{hs:#x}"
        );
    }
    if let Err(e) = vm.call_virtual(child, "measure", &[V::I32(ws), V::I32(hs)]) {
        if crate::dbg_flag("OVELO_VIEWDBG") {
            eprintln!("[viewdbg] measureChild h{child} err {e}");
        }
    }
}

fn view_is_gone(vm: &mut Vm, h: u32) -> bool {
    if let Some(v) = pseudo_get(vm, h, P_VIS) {
        return matches!(v, V::I32(8) | V::I32(2));
    }
    matches!(attr_find(vm, h, "visibility"), Some(V::I32(2)))
}

/// Rough proportional-text advance in em units. The runtime cannot use
/// the host font shaper from dexrt's measure fallback, but a flat wide
/// factor both clips narrow text and reserves far too much width for
/// labels made of narrow glyphs. These classes track common Latin font
/// metrics closely enough for wrap_content layout.
fn em_width(c: char) -> f32 {
    match c {
        ' ' => 0.30,
        'i' | 'j' | 'l' | 'I' | '\'' | '`' | '.' | ',' | ':' | ';' | '!' | '|' => 0.34,
        'f' | 'r' | 't' | '(' | ')' | '[' | ']' | '{' | '}' => 0.43,
        'm' | 'w' => 0.86,
        'M' | 'W' => 0.92,
        'A'..='Z' => 0.70,
        '0'..='9' => 0.60,
        'a'..='z' => 0.56,
        c if c.is_ascii_punctuation() => 0.48,
        c if c.is_ascii() => 0.56,
        // CJK and most emoji/cyrillic fallback glyphs occupy close to a
        // full em in the fonts the host renderer substitutes.
        c if !c.is_ascii() => 1.0,
        _ => 0.56,
    }
}

fn estimated_text_width(text: &str, ts: f32) -> i32 {
    (text.chars().map(em_width).sum::<f32>() * 1.15)
        .mul_add(ts, 0.0)
        .ceil() as i32
}

/// Generic `onMeasure` for framework skeleton containers: measure
/// each child with margins, then fold the measured content size
/// against the spec via `resolveSize`. The `measure` arm pre-seeds
/// P_MEAS_* with the spec size, which is right under EXACTLY but
/// reports 0 under UNSPECIFIED (e.g. RecyclerView's wrap-content
/// fill) — Framework containers must report their content extent
/// there or every ancestor's measured height collapses to padding.
fn group_measure_fallback(vm: &mut Vm, this: u32, pw: i32, ph: i32) {
    // Start/end padding resolves to physical left/right in LTR layouts;
    // XML that only uses paddingStart/End otherwise measures a padded
    // TextView as if it had no horizontal padding at all.
    let (pad_l, pad_t, pad_r, pad_b) = view_padding(vm, this);
    let pad_w = pad_l + pad_r;
    let pad_h = pad_t + pad_b;
    let kids = children(vm, this);
    if kids.is_empty() {
        // Leaf view (TextView/Button with no dex onMeasure): report
        // its intrinsic text extent or wrap_content collapses to 0.
        let text = match pseudo_get(vm, this, P_TEXT) {
            Some(V::Ref(th)) if th != 0 => s(vm, th).unwrap_or_default(),
            Some(V::I32(id)) if id > 0 => vm
                .apk_res
                .as_ref()
                .and_then(|r| r.string_of(id as u32))
                .map(|s| s.to_string())
                .unwrap_or_default(),
            _ => String::new(),
        };
        // The `android:text` attr lives in P_ATTRS until the dex ctor
        // calls setText — measure from it or XML-inflated TextViews
        // wrap to 0 wide.
        let text = if text.is_empty() {
            attr_find(vm, this, "text")
                .or_else(|| attr_find(vm, this, "hint"))
                .map(|v| match v {
                    V::Ref(th) if th != 0 => s(vm, th).unwrap_or_default(),
                    V::I32(id) if id > 0 => vm
                        .apk_res
                        .as_ref()
                        .and_then(|r| r.string_of(id as u32))
                        .map(|s| s.to_string())
                        .unwrap_or_default(),
                    _ => String::new(),
                })
                .unwrap_or_default()
        } else {
            text
        };
        let ts = match pseudo_get(vm, this, P_TSIZE) {
            Some(V::F32(f)) => Some(f),
            Some(V::I32(b)) => Some(f32::from_bits(b as u32)),
            _ => None,
        }
        .or_else(|| {
            attr_find(vm, this, "textSize").and_then(|v| match v {
                V::F32(f) => Some(f),
                V::I32(id) if id > 0 => vm.apk_res.as_ref().and_then(|r| r.dimen(id as u32)),
                _ => None,
            })
        })
        .unwrap_or(14.0)
        .abs()
        .max(1.0);
        // An ImageView's content is its drawable: AOSP ImageView.
        // onMeasure reports the drawable's intrinsic size (still folded
        // through resolveSize below), never the parent's spec size.
        let (iw, ih) = view_content_image(vm, this, text.is_empty())
            .map(|(w, h, _)| (w as i32, h as i32))
            .unwrap_or((0, 0));
        let leaf_desc = class_of(vm, V::Ref(this));
        let is_compound = leaf_desc.contains("CompoundButton")
            || leaf_desc.contains("CheckBox")
            || leaf_desc.contains("Checkbox")
            || leaf_desc.contains("RadioButton")
            || leaf_desc.contains("Switch")
            || vm.extends_named(&leaf_desc, "CompoundButton");
        let is_text_leaf = leaf_desc.contains("TextView")
            || leaf_desc.contains("EditText")
            || vm.extends_named(&leaf_desc, "TextView");
        let (cw, chh) = if text.is_empty() {
            if is_compound {
                // CompoundButton's button drawable has an intrinsic
                // affordance even when the label is empty; without it a
                // wrap_content CheckBox measures to zero width.
                (iw.max(28), ih.max(28))
            } else if is_text_leaf {
                // An empty TextView/EditText still reserves one line
                // of text height in AOSP. Inputs otherwise collapse to
                // a few pixels when their hint lives on TextInputLayout.
                (iw, ih.max((ts * 1.3) as i32))
            } else {
                (iw, ih)
            }
        } else {
            // Wrapping: when the unbroken text overflows the available
            // width it folds to `ceil(cw/avail)` lines — a single-line
            // height lets the overflow draw over the next sibling.
            // Width is estimated per-char; the renderer's real glyph
            // advance runs wider than a naive 0.6·size, so the wrap
            // check uses a fatter factor than the extent itself or a
            // frame measured "one line" renders two.
            let line_h = (ts * 1.3) as i32;
            // Measure each explicit paragraph separately: `split('\n')`
            // keeps the trailing empty paragraph, so `a\n` correctly
            // reserves two line heights even without a width constraint.
            let paragraphs: Vec<&str> = text.split('\n').collect();
            let widths: Vec<i32> = paragraphs
                .iter()
                .map(|p| estimated_text_width(p, ts))
                .collect();
            let real_w = widths.iter().copied().max().unwrap_or(0);
            let button_w = if is_compound { 28 } else { 0 };
            let avail = (pw & 0x3FFF_FFFF) - pad_w - button_w;
            let frame_w = if avail > 0 { real_w.min(avail) } else { real_w };
            let lines = if frame_w > 0 {
                widths
                    .iter()
                    .map(|w| ((*w + frame_w - 1) / frame_w).max(1))
                    .sum()
            } else {
                paragraphs.len() as i32
            };
            (frame_w + button_w, (line_h * lines).max(button_w))
        };
        // resolveSize semantics on the desired extent (content or
        // drawable intrinsic): AT_MOST → min(desired, spec). A leaf's
        // requested minimum is part of that desired extent — otherwise
        // wrap_content ignores android:minWidth/minHeight entirely.
        let desired_w = (cw + pad_w).max(min_dim(vm, this, P_MINW, "minWidth"));
        let desired_h = (chh + pad_h).max(min_dim(vm, this, P_MINH, "minHeight"));
        pseudo_set(
            vm,
            this,
            P_MEAS_W,
            V::I32(resolve_size(desired_w, pw, true)),
        );
        pseudo_set(
            vm,
            this,
            P_MEAS_H,
            V::I32(resolve_size(desired_h, ph, true)),
        );
        return;
    }
    // Stack containers by ancestry, not name — a `MyColumn extends
    // LinearLayout` stacks vertically even when its name doesn't say
    // so (matches the layout-side check below).
    let desc = class_of(vm, V::Ref(this));
    let linear = desc.contains("LinearLayout")
        || desc.contains("RadioGroup")
        || vm.extends_named(&desc, "LinearLayout");
    let linear_v = linear && linear_vertical(vm, this);
    let linear_h = linear && !linear_v;
    let relative = !linear && vm.extends_named(&desc, "RelativeLayout");
    let (mut cw, mut chh) = (0i32, 0i32);
    for c in kids.iter().copied() {
        let V::Ref(ch) = c else { continue };
        if ch == 0 || view_is_gone(vm, ch) {
            continue;
        }
        // AOSP measureChildWithMargins: `heightUsed` is the running
        // total only for vertical stacking — a horizontal LinearLayout
        // accumulates `widthUsed` instead, and FrameLayout children
        // each get the full height.
        let (w_used, h_used) = if linear_v {
            (0, chh)
        } else if linear_h {
            (cw, 0)
        } else {
            (0, 0)
        };
        measure_child(vm, this, ch, pw, w_used, ph, h_used, true);
        let w = pint(vm, ch, P_MEAS_W) & 0x00FF_FFFF;
        let hh = pint(vm, ch, P_MEAS_H) & 0x00FF_FFFF;
        // AOSP's mTotalLength / FrameLayout maxHeight fold in the
        // child's margins — omitting them under-reports the group's own
        // extent and starves the match_parent re-measure below of space
        // that was never accounted for.
        let lp_obj = match pseudo_get(vm, ch, P_LP) {
            Some(V::Ref(l)) => l,
            _ => 0,
        };
        let mv = lp_field(vm, lp_obj, "topMargin")
            .and_then(|v| lp_px(vm, v))
            .or_else(|| attr_find(vm, ch, "layout_marginTop").map(|v| len_v(vm, v)))
            .unwrap_or(0)
            + lp_field(vm, lp_obj, "bottomMargin")
                .and_then(|v| lp_px(vm, v))
                .or_else(|| attr_find(vm, ch, "layout_marginBottom").map(|v| len_v(vm, v)))
                .unwrap_or(0);
        let mh = lp_field(vm, lp_obj, "leftMargin")
            .and_then(|v| lp_px(vm, v))
            .or_else(|| attr_find(vm, ch, "layout_marginLeft").map(|v| len_v(vm, v)))
            .unwrap_or(0)
            + lp_field(vm, lp_obj, "rightMargin")
                .and_then(|v| lp_px(vm, v))
                .or_else(|| attr_find(vm, ch, "layout_marginRight").map(|v| len_v(vm, v)))
                .unwrap_or(0);
        // Vertical LinearLayout accumulates heights; horizontal
        // accumulates widths; everything else reports the largest
        // child (FrameLayout semantics).
        if linear_v {
            chh += hh + mv;
            cw = cw.max(w + mh);
        } else if linear_h {
            cw += w + mh;
            chh = chh.max(hh + mv);
        } else {
            cw = cw.max(w + mh);
            chh = chh.max(hh + mv);
        }
    }
    // AOSP LinearLayout weight pass: children with `layout_weight`
    // get a second measure that adds their share of the parent's
    // leftover axis space — the classic `0dp`+weight pattern stays
    // at zero extent without it.
    if linear_v || linear_h {
        let axis_size = (if linear_v { ph } else { pw }) & 0x3FFF_FFFF;
        let axis_total = if linear_v { chh } else { cw };
        let axis_pad = if linear_v { pad_h } else { pad_w };
        let remaining = (axis_size - axis_pad - axis_total).max(0);
        if remaining > 0 {
            let mut total_weight = 0f32;
            let mut shares: Vec<(u32, f32)> = Vec::new();
            for c in kids.iter().copied() {
                let V::Ref(ch) = c else { continue };
                if ch == 0 || view_is_gone(vm, ch) {
                    continue;
                }
                let lp_obj = match pseudo_get(vm, ch, P_LP) {
                    Some(V::Ref(l)) => l,
                    _ => 0,
                };
                // `weight` is a float LP field — try F first so a
                // runtime `lp.weight = 0` overrides the XML attr. Dex
                // registers are untyped: an F field write can land as
                // raw I32 bits — reinterpret via from_bits (1065353216
                // must read as 1.0, not a billion).
                let w = lp_field_t(vm, lp_obj, "weight", "F")
                    .map(|v| match v {
                        V::F32(f) => f,
                        V::F64(f) => f as f32,
                        V::I32(i) => f32::from_bits(i as u32),
                        V::I64(l) => f64::from_bits(l as u64) as f32,
                        _ => 0.0,
                    })
                    .or_else(|| {
                        lp_field(vm, lp_obj, "weight")
                            .or_else(|| attr_find(vm, ch, "layout_weight"))
                            .map(|v| match v {
                                V::F32(f) => f,
                                V::I32(i) => i as f32,
                                _ => 0.0,
                            })
                    })
                    .unwrap_or(0.0);
                if w > 0.0 {
                    shares.push((ch, w));
                    total_weight += w;
                }
            }
            if total_weight > 0.0 {
                let wsum = attr_find(vm, this, "weightSum")
                    .map(|v| match v {
                        V::F32(f) => f,
                        V::I32(i) => i as f32,
                        _ => 0.0,
                    })
                    .unwrap_or(0.0);
                let wsum = if wsum > 0.0 { wsum } else { total_weight };
                let mut added = 0i32;
                for (ch, w) in shares {
                    let share = (remaining as f32 * w / wsum) as i32;
                    if share <= 0 {
                        continue;
                    }
                    let lp_obj = match pseudo_get(vm, ch, P_LP) {
                        Some(V::Ref(l)) => l,
                        _ => 0,
                    };
                    let lp_dim = |field: &str, xml: &str| -> i32 {
                        lp_field(vm, lp_obj, field)
                            .and_then(|v| lp_px(vm, v))
                            .or_else(|| attr_find(vm, ch, xml).map(|v| len_v(vm, v)))
                            .unwrap_or(-2)
                    };
                    let ml = lp_field(vm, lp_obj, "leftMargin")
                        .and_then(|f| lp_px(vm, f))
                        .unwrap_or(0)
                        + lp_field(vm, lp_obj, "rightMargin")
                            .and_then(|f| lp_px(vm, f))
                            .unwrap_or(0);
                    let mv = lp_field(vm, lp_obj, "topMargin")
                        .and_then(|f| lp_px(vm, f))
                        .unwrap_or(0)
                        + lp_field(vm, lp_obj, "bottomMargin")
                            .and_then(|f| lp_px(vm, f))
                            .unwrap_or(0);
                    // AOSP: `skippedMeasure` (axis lp dim == 0) gets
                    // EXACTLY(share); a fixed axis dim wins via
                    // getChildMeasureSpec and is left alone; a
                    // wrap/match axis grows by content + share.
                    let axis_dim = if linear_v {
                        lp_dim("height", "layout_height")
                    } else {
                        lp_dim("width", "layout_width")
                    };
                    if axis_dim > 0 {
                        continue;
                    }
                    let old_axis = if linear_v {
                        pint(vm, ch, P_MEAS_H) & 0x00FF_FFFF
                    } else {
                        pint(vm, ch, P_MEAS_W) & 0x00FF_FFFF
                    };
                    let new_axis = old_axis + share;
                    let (ws, hs) = if linear_v {
                        (
                            child_measure_spec(pw, pad_w + ml, lp_dim("width", "layout_width")),
                            EXACTLY | new_axis,
                        )
                    } else {
                        (
                            EXACTLY | new_axis,
                            child_measure_spec(ph, pad_h + mv, lp_dim("height", "layout_height")),
                        )
                    };
                    if vm
                        .call_virtual(ch, "measure", &[V::I32(ws), V::I32(hs)])
                        .is_ok()
                    {
                        added += new_axis - old_axis;
                        let new_cross = if linear_v {
                            pint(vm, ch, P_MEAS_W) & 0x00FF_FFFF
                        } else {
                            pint(vm, ch, P_MEAS_H) & 0x00FF_FFFF
                        };
                        if linear_v {
                            cw = cw.max(new_cross + ml);
                        } else {
                            chh = chh.max(new_cross + mv);
                        }
                    }
                }
                if linear_v {
                    chh += added;
                } else {
                    cw += added;
                }
            }
        }
    }
    // AOSP RelativeLayout.onMeasure derives WRAP bounds from the
    // rule-resolved child positions — a `layout_below` child counts at
    // its anchor's bottom, not at the origin. Without this a relative
    // container wraps to its tallest child alone and everything stacked
    // below the anchor overflows past the measured bottom (fossify
    // home's launcher label lands under the icon, outside the cell).
    if relative {
        const MODE_MASK: i32 = 0x3 << 30;
        const EXACTLY: i32 = 1 << 30;
        const SIZE_MASK: i32 = 0x3FFF_FFFF;
        // Pass -1 on a non-EXACTLY axis: parent-edge/center/match rules
        // can't resolve against an unknown extent (AOSP's myWidth=-1).
        let aw = if pw & MODE_MASK == EXACTLY {
            pw & SIZE_MASK
        } else {
            -1
        };
        let ah = if ph & MODE_MASK == EXACTLY {
            ph & SIZE_MASK
        } else {
            -1
        };
        let pad_l = lp_px(vm, attr_find(vm, this, "paddingLeft").unwrap_or(V::I32(0))).unwrap_or(0);
        let pad_t = lp_px(vm, attr_find(vm, this, "paddingTop").unwrap_or(V::I32(0))).unwrap_or(0);
        let ks = relative_positions(vm, this, aw, ah);
        if crate::dbg_flag("OVELO_VIEWDBG") {
            eprintln!(
                "[viewdbg] relMeas h{this} aw={aw} ah={ah} cw={cw} chh={chh} kids={}",
                ks.len()
            );
            for k in &ks {
                eprintln!(
                    "[viewdbg]   kid h{} {}x{} rect={:?}",
                    k.ch, k.w, k.h, k.rect
                );
            }
        }
        for k in ks {
            let (_, _, r, b) = k.rect;
            // rect coords include this view's padding (lead = pad +
            // margin) — strip it so `+ pad` below isn't double-counted.
            cw = cw.max(r + k.mr - pad_l);
            chh = chh.max(b + k.mb - pad_t);
        }
    }
    let self_w = resolve_size(cw + pad_w, pw, true).max(0);
    let self_h = resolve_size(chh + pad_h, ph, true).max(0);
    // Negative extents (e.g. 0-height child with negative margins)
    // must not be stored — getMeasuredHeight masks to 24 bits and a
    // negative would read back as ~16M px.
    pseudo_set(vm, this, P_MEAS_W, V::I32(self_w));
    pseudo_set(vm, this, P_MEAS_H, V::I32(self_h));
    // AOSP ViewGroup#measureMatchParentChildren: under a non-EXACTLY
    // parent spec a match_parent child measures UNSPEC/0 first, then
    // gets a second pass at EXACTLY(resolved - padding - margins).
    // Without it match_parent children of a wrap/unspec container
    // stay 0×0 forever — the grid cell's thumbnail never gets a
    // frame and Glide's SizeDeterminer waits on getWidth()>0.
    const EXACTLY: i32 = 1 << 30;
    const MODE_MASK: i32 = 0x3 << 30;
    if (pw & MODE_MASK) != EXACTLY || (ph & MODE_MASK) != EXACTLY {
        let cw_in = self_w - pad_w;
        let ch_in = self_h - pad_h;
        if cw_in > 0 || ch_in > 0 {
            // AOSP LinearLayout#measureVertical re-measures each
            // match_parent child with `delta = height - mTotalLength`
            // (non-match children plus every margin) — not the full
            // resolved height, which would stack it past the bottom.
            let (nonmatch_h, match_margins_h) = if linear_v {
                kids.iter().copied().fold((0i32, 0i32), |(nm, mm), c| {
                    let V::Ref(ch) = c else {
                        return (nm, mm);
                    };
                    if ch == 0 || view_is_gone(vm, ch) {
                        return (nm, mm);
                    }
                    let lp_obj = match pseudo_get(vm, ch, P_LP) {
                        Some(V::Ref(l)) => l,
                        _ => 0,
                    };
                    let lph = lp_field(vm, lp_obj, "height")
                        .and_then(|v| lp_px(vm, v))
                        .or_else(|| attr_find(vm, ch, "layout_height").map(|v| len_v(vm, v)))
                        .unwrap_or(-2);
                    let mv = lp_field(vm, lp_obj, "topMargin")
                        .and_then(|v| lp_px(vm, v))
                        .unwrap_or(0)
                        + lp_field(vm, lp_obj, "bottomMargin")
                            .and_then(|v| lp_px(vm, v))
                            .unwrap_or(0);
                    if lph == -1 {
                        (nm, mm + mv)
                    } else {
                        (nm + (pint(vm, ch, P_MEAS_H) & 0x00FF_FFFF) + mv, mm)
                    }
                })
            } else {
                (0, 0)
            };
            let match_h = (ch_in - nonmatch_h - match_margins_h).max(0);
            // Horizontal linears need the same fold on X — otherwise a
            // match_parent-width child re-measures to the full parent
            // width and everything after it lands past the right edge.
            let (nonmatch_w, match_margins_w) = if linear_h {
                kids.iter().copied().fold((0i32, 0i32), |(nm, mm), c| {
                    let V::Ref(ch) = c else {
                        return (nm, mm);
                    };
                    if ch == 0 || view_is_gone(vm, ch) {
                        return (nm, mm);
                    }
                    let lp_obj = match pseudo_get(vm, ch, P_LP) {
                        Some(V::Ref(l)) => l,
                        _ => 0,
                    };
                    let lpw = lp_field(vm, lp_obj, "width")
                        .and_then(|v| lp_px(vm, v))
                        .or_else(|| attr_find(vm, ch, "layout_width").map(|v| len_v(vm, v)))
                        .unwrap_or(-2);
                    let mh = lp_field(vm, lp_obj, "leftMargin")
                        .and_then(|v| lp_px(vm, v))
                        .unwrap_or(0)
                        + lp_field(vm, lp_obj, "rightMargin")
                            .and_then(|v| lp_px(vm, v))
                            .unwrap_or(0);
                    if lpw == -1 {
                        (nm, mm + mh)
                    } else {
                        (nm + (pint(vm, ch, P_MEAS_W) & 0x00FF_FFFF) + mh, mm)
                    }
                })
            } else {
                (0, 0)
            };
            let match_w = (cw_in - nonmatch_w - match_margins_w).max(0);
            for c in kids.iter().copied() {
                let V::Ref(ch) = c else { continue };
                if ch == 0 || view_is_gone(vm, ch) {
                    continue;
                }
                let lp_obj = match pseudo_get(vm, ch, P_LP) {
                    Some(V::Ref(l)) => l,
                    _ => 0,
                };
                let dim = |field: &str, xml: &str| {
                    lp_field(vm, lp_obj, field)
                        .and_then(|v| lp_px(vm, v))
                        .or_else(|| attr_find(vm, ch, xml).map(|v| len_v(vm, v)))
                        .unwrap_or(-2)
                };
                let (lpw, lph) = (dim("width", "layout_width"), dim("height", "layout_height"));
                if lpw != -1 && lph != -1 {
                    continue;
                }
                let marg = |name: &str| {
                    lp_field(vm, lp_obj, name)
                        .and_then(|v| lp_px(vm, v))
                        .unwrap_or(0)
                };
                let mh = marg("leftMargin") + marg("rightMargin");
                let mv = marg("topMargin") + marg("bottomMargin");
                let ws = if lpw == -1 {
                    EXACTLY
                        | if linear_h {
                            match_w
                        } else {
                            (cw_in - mh).max(0)
                        }
                } else {
                    child_measure_spec(pw, pad_w + mh, lpw)
                };
                let hs = if lph == -1 {
                    EXACTLY
                        | if linear_v {
                            match_h
                        } else {
                            (ch_in - mv).max(0)
                        }
                } else {
                    child_measure_spec(ph, pad_h + mv, lph)
                };
                let _ = vm.call_virtual(ch, "measure", &[V::I32(ws), V::I32(hs)]);
            }
        }
    }
}

/// Generic `onLayout` for framework skeleton containers: FrameLayout
/// semantics (children at the parent's content origin plus their
/// margins, at measured size); LinearLayout stacks vertically.
fn gravity_of(vm: &Vm, view: u32) -> i32 {
    attr_find(vm, view, "layout_gravity")
        .map(i32v)
        .unwrap_or(-1)
}

fn horizontal_gravity(gravity: i32) -> i32 {
    if gravity & 0x800003 == 0x800003 {
        3
    } else if gravity & 0x800005 == 0x800005 {
        5
    } else {
        gravity & 0x7
    }
}

fn gravity_x(
    gravity: i32,
    pad_l: i32,
    pad_r: i32,
    width: i32,
    child: i32,
    ml: i32,
    mr: i32,
) -> i32 {
    match horizontal_gravity(gravity) {
        1 => pad_l + ml + ((width - pad_l - pad_r - ml - mr - child) / 2).max(0),
        5 => width - pad_r - mr - child,
        _ => pad_l + ml,
    }
}

fn gravity_y(
    gravity: i32,
    pad_t: i32,
    pad_b: i32,
    height: i32,
    child: i32,
    mt: i32,
    mb: i32,
) -> i32 {
    match gravity & 0x70 {
        0x10 => pad_t + mt + ((height - pad_t - pad_b - mt - mb - child) / 2).max(0),
        0x50 => height - pad_b - mb - child,
        _ => pad_t + mt,
    }
}

fn group_layout_fallback(vm: &mut Vm, this: u32, _l: i32, _t: i32, r: i32, b: i32) {
    let (pad_l, pad_t, pad_r, pad_b) = view_padding(vm, this);
    let desc = class_of(vm, V::Ref(this));
    let linear = desc.contains("LinearLayout")
        || desc.contains("RadioGroup")
        || vm.extends_named(&desc, "LinearLayout");
    let linear_h = linear && !linear_vertical(vm, this);
    if !linear && vm.extends_named(&desc, "RelativeLayout") {
        relative_layout_fallback(vm, this, r - _l, b - _t);
        return;
    }
    // Children are laid out in THIS view's content coordinates —
    // (0,0) is its own top-left, not the (l,t) corner of its frame
    // in the parent, or a nested container shifts its children by
    // its own position a second time.
    let mut cur_t = pad_t;
    let mut cur_l = pad_l;
    for c in children(vm, this) {
        let V::Ref(ch) = c else { continue };
        if ch == 0 || view_is_gone(vm, ch) || pseudo_get(vm, ch, P_POPUP_ROOT).is_some() {
            continue;
        }
        let (ml, mt, mr, mb) = if let Some(V::Ref(lp)) = pseudo_get(vm, ch, P_LP) {
            if lp != 0 {
                (
                    lp_field(vm, lp, "leftMargin")
                        .and_then(|f| lp_px(vm, f))
                        .unwrap_or(0),
                    lp_field(vm, lp, "topMargin")
                        .and_then(|f| lp_px(vm, f))
                        .unwrap_or(0),
                    lp_field(vm, lp, "rightMargin")
                        .and_then(|f| lp_px(vm, f))
                        .unwrap_or(0),
                    lp_field(vm, lp, "bottomMargin")
                        .and_then(|f| lp_px(vm, f))
                        .unwrap_or(0),
                )
            } else {
                (0, 0, 0, 0)
            }
        } else {
            (0, 0, 0, 0)
        };
        let cw = pint(vm, ch, P_MEAS_W) & 0x00FF_FFFF;
        let chh = pint(vm, ch, P_MEAS_H) & 0x00FF_FFFF;
        let gravity = gravity_of(vm, ch);
        let width = r - _l;
        let height = b - _t;
        let (cl, ct) = if linear_h {
            (
                cur_l + ml,
                gravity_y(gravity, pad_t, pad_b, height, chh, mt, mb),
            )
        } else if linear {
            (
                gravity_x(gravity, pad_l, pad_r, width, cw, ml, mr),
                cur_t + mt,
            )
        } else {
            (
                gravity_x(gravity, pad_l, pad_r, width, cw, ml, mr),
                gravity_y(gravity, pad_t, pad_b, height, chh, mt, mb),
            )
        };
        if crate::dbg_flag("OVELO_VIEWDBG") {
            eprintln!("[viewdbg] groupLayout h{ch} meas={cw}x{chh} at {cl},{ct}");
        }
        if let Err(e) = vm.call_virtual(
            ch,
            "layout",
            &[V::I32(cl), V::I32(ct), V::I32(cl + cw), V::I32(ct + chh)],
        ) {
            if crate::dbg_flag("OVELO_VIEWDBG") {
                eprintln!("[viewdbg] groupLayout h{ch} err {e}");
                if e.to_string().contains("ConstraintWidget") {
                    for k in children(vm, ch) {
                        if let V::Ref(kh) = k {
                            let lp = pseudo_get(vm, kh, P_LP).unwrap_or(V::Ref(0));
                            let (lc, wid) = match lp {
                                V::Ref(lh) if lh != 0 => {
                                    let w = match vm.heap.get(lh) {
                                        Some(Obj::Instance { fields, .. })
                                        | Some(Obj::Host { fields, .. }) => vm
                                            .field_keys_named(
                                                "widget",
                                                "Landroidx/constraintlayout/solver/widgets/ConstraintWidget;",
                                            )
                                            .iter()
                                            .any(|(_, g)| {
                                                fields.get(g).is_some_and(|v| *v != V::Ref(0))
                                            }),
                                        _ => false,
                                    };
                                    (class_of(vm, lp), w)
                                }
                                _ => ("none".into(), false),
                            };
                            eprintln!(
                                "[viewdbg]   clkid h{kh} {} lp={lc} widget={wid}",
                                class_of(vm, k)
                            );
                        }
                    }
                }
            }
        }
        cur_t = ct + chh + mb;
        cur_l = cl + cw + mr;
    }
}

struct RelKid {
    ch: u32,
    id: i32,
    w: i32,
    h: i32,
    ml: i32,
    mt: i32,
    mr: i32,
    mb: i32,
    match_w: bool,
    match_h: bool,
    gone: bool,
    rect: (i32, i32, i32, i32),
}

/// `RelativeLayout` position resolution: resolve the usual XML rules
/// (`layout_alignParent*`, `layout_center*`, `layout_below`/`above`,
/// `layout_toRightOf`/`toLeftOf`, `layout_align*`) against the
/// container's content rect at each child's measured size + margins.
/// `w`/`h` are the container's full size in its own coordinates; an
/// axis may pass `-1` when the extent is still unknown (a WRAP_CONTENT
/// measure) — parent-edge/center/match rules on that axis are then
/// ignored, matching AOSP's `myWidth/myHeight = -1` behavior.
/// Pure reads: usable from both measure (content bounds) and layout.
fn relative_positions(vm: &mut Vm, this: u32, w: i32, h: i32) -> Vec<RelKid> {
    let pad_l = lp_px(vm, attr_find(vm, this, "paddingLeft").unwrap_or(V::I32(0))).unwrap_or(0);
    let pad_t = lp_px(vm, attr_find(vm, this, "paddingTop").unwrap_or(V::I32(0))).unwrap_or(0);
    let pad_r = lp_px(vm, attr_find(vm, this, "paddingRight").unwrap_or(V::I32(0))).unwrap_or(0);
    let pad_b = lp_px(
        vm,
        attr_find(vm, this, "paddingBottom").unwrap_or(V::I32(0)),
    )
    .unwrap_or(0);
    let kids: Vec<u32> = children(vm, this)
        .into_iter()
        .filter_map(|v| match v {
            V::Ref(h) if h != 0 => Some(h),
            _ => None,
        })
        .collect();
    let n = kids.len();
    let margin = |vm: &Vm, ch: u32, name: &str| -> i32 {
        if let Some(V::Ref(lp)) = pseudo_get(vm, ch, P_LP) {
            if lp != 0 {
                return lp_field(vm, lp, name)
                    .and_then(|f| lp_px(vm, f))
                    .unwrap_or(0);
            }
        }
        0
    };
    let mut ks: Vec<RelKid> = Vec::with_capacity(n);
    for &ch in &kids {
        let lp_obj = match pseudo_get(vm, ch, P_LP) {
            Some(V::Ref(lp)) => lp,
            _ => 0,
        };
        // LIVE LayoutParams schlägt das XML-Attr: `setLayoutParams` kann
        // width/height nach der Inflation auf match_parent umstellen.
        let lp_dim = |field: &str, xml: &str| -> i32 {
            lp_field(vm, lp_obj, field)
                .and_then(|v| lp_px(vm, v))
                .or_else(|| attr_find(vm, ch, xml).map(|v| len_v(vm, v)))
                .unwrap_or(-2)
        };
        ks.push(RelKid {
            ch,
            id: pint(vm, ch, P_VIEW_ID),
            w: pint(vm, ch, P_MEAS_W) & 0x00FF_FFFF,
            h: pint(vm, ch, P_MEAS_H) & 0x00FF_FFFF,
            ml: margin(vm, ch, "leftMargin"),
            mt: margin(vm, ch, "topMargin"),
            mr: margin(vm, ch, "rightMargin"),
            mb: margin(vm, ch, "bottomMargin"),
            match_w: lp_dim("width", "layout_width") == -1,
            match_h: lp_dim("height", "layout_height") == -1,
            gone: view_is_gone(vm, ch),
            rect: (0, 0, 0, 0),
        });
    }
    let idx_of = |id: i32| -> Option<usize> {
        if id == 0 {
            return None;
        }
        ks.iter().position(|k| k.id == id)
    };
    let idx_visible = |vm: &Vm, id: i32, names: &[&str]| -> Option<usize> {
        // AOSP RelativeLayout resolves an anchor chain through GONE
        // intermediates: `below(goneHeader)` anchors at whatever the
        // gone header itself was below, not at the parent origin.
        let mut cur = id;
        for _ in 0..16 {
            let j = idx_of(cur)?;
            if !ks[j].gone {
                return Some(j);
            }
            cur = names
                .iter()
                .find_map(|n| match attr_find(vm, ks[j].ch, n) {
                    Some(V::I32(next)) if next > 0 => Some(next),
                    _ => None,
                })?;
        }
        None
    };
    let truthy = |v: Option<V>| matches!(v, Some(V::I32(i)) if i != 0);
    let ref_id = |v: Option<V>| -> Option<i32> {
        match v {
            Some(V::I32(i)) if i > 0 => Some(i),
            _ => None,
        }
    };
    let rule = |vm: &Vm, ch: u32, names: &[&str]| -> Option<V> {
        names.iter().find_map(|n| attr_find(vm, ch, n))
    };
    // Edges resolve per axis — AOSP's DependencyGraph does the same:
    // `toEndOf`+`toStartOf` squeeze a child between siblings without a
    // cycle when the siblings' vertical rules point back (e.g. the
    // header arrows' alignTop/alignBottom anchor on the label).
    // Each axis yields (leading, trailing) content-coord edges; a
    // missing trailing edge falls back to leading+measured.
    type Axis = Vec<Option<(i32, i32)>>;
    let resolve_edges =
        |vm: &Vm, ch: u32, k: &RelKid, placed: &Axis, horiz: bool| -> Option<(i32, i32)> {
            // Leading edge: sibling rules first (toRightOf/alignLeft,
            // below/alignTop), else the parent's near edge.
            let (lead_rules, trail_parent, lead_parent, lead_sib_rules, size, mlead, mtrail, mp) =
                if horiz {
                    (
                        &["layout_toRightOf", "layout_toEndOf"][..],
                        &["layout_alignParentRight", "layout_alignParentEnd"][..],
                        &["layout_alignParentLeft", "layout_alignParentStart"][..],
                        &["layout_alignLeft", "layout_alignStart"][..],
                        k.w,
                        k.ml,
                        k.mr,
                        k.match_w,
                    )
                } else {
                    (
                        &["layout_below"][..],
                        &["layout_alignParentBottom"][..],
                        &["layout_alignParentTop"][..],
                        &["layout_alignTop"][..],
                        k.h,
                        k.mt,
                        k.mb,
                        k.match_h,
                    )
                };
            let (extent, pleading, ptrailing) = if horiz {
                (w, pad_l, pad_r)
            } else {
                (h, pad_t, pad_b)
            };
            let sib_edge = |id: i32, trailing: bool, names: &[&str]| -> Option<Option<i32>> {
                // Some(edge) = rule present; the inner Option is the
                // sibling's resolved edge (None while unplaced).
                idx_visible(vm, id, names)
                    .map(|j| placed[j].map(|(a, b)| if trailing { b } else { a }))
            };

            let mut lead: Option<i32> = None;
            let mut trail: Option<i32> = None;
            let mut pending = false;
            // extent < 0: WRAP axis mid-measure — parent-edge anchors
            // can't resolve yet (AOSP leaves myWidth/myHeight = -1).
            if extent >= 0 && truthy(rule(vm, ch, trail_parent)) {
                trail = Some(extent - ptrailing - mtrail);
            }
            if truthy(rule(vm, ch, lead_parent)) {
                lead = Some(pleading + mlead);
            }
            if let Some(dep) = ref_id(rule(vm, ch, lead_rules)) {
                match sib_edge(dep, true, lead_rules) {
                    Some(Some(e)) => lead = Some(e + mlead),
                    Some(None) => pending = true,
                    None => {}
                }
            }
            if let Some(dep) = ref_id(rule(vm, ch, lead_sib_rules)) {
                match sib_edge(dep, false, lead_sib_rules) {
                    Some(Some(e)) => lead = Some(e + mlead),
                    Some(None) => pending = true,
                    None => {}
                }
            }
            // Trailing edge, sibling-anchored: `toLeftOf`/`toStartOf`/
            // `above` anchor the sibling's leading edge, `alignRight`/
            // `alignEnd`/`alignBottom` its trailing edge — separate rule
            // sets since they read opposite edges.
            let (trail_lead_anchor, trail_trail_anchor) = if horiz {
                (
                    &["layout_toLeftOf", "layout_toStartOf"][..],
                    &["layout_alignRight", "layout_alignEnd"][..],
                )
            } else {
                (&["layout_above"][..], &["layout_alignBottom"][..])
            };
            for (names, on_trailing) in [(trail_lead_anchor, false), (trail_trail_anchor, true)] {
                if let Some(dep) = ref_id(rule(vm, ch, names)) {
                    match sib_edge(dep, on_trailing, names) {
                        Some(Some(e)) => trail = Some(e - mtrail),
                        Some(None) => pending = true,
                        None => {}
                    }
                }
            }

            if pending {
                return None;
            }
            match (lead, trail) {
                (Some(l), Some(t)) => Some((l, t)),
                (Some(l), None) => {
                    // match_parent anchored only on the leading edge fills
                    // the rest of the parent's content box.
                    if mp && extent >= 0 {
                        Some((l, extent - ptrailing - mtrail))
                    } else {
                        Some((l, l + size))
                    }
                }
                (None, Some(t)) => {
                    // match_parent anchored only on the trailing edge
                    // (`layout_above` + match_height) stretches up to the
                    // parent's leading edge instead of overflowing above it.
                    if mp {
                        Some((pleading + mlead, t))
                    } else {
                        Some((t - size, t))
                    }
                }
                (None, None) => {
                    if extent >= 0
                        && (truthy(rule(vm, ch, &["layout_centerInParent"]))
                            || truthy(rule(
                                vm,
                                ch,
                                if horiz {
                                    &["layout_centerHorizontal"]
                                } else {
                                    &["layout_centerVertical"]
                                },
                            )))
                    {
                        let c =
                            pleading + (extent - pleading - ptrailing - size) / 2 + mlead - mtrail;
                        Some((c, c + size))
                    } else {
                        Some((pleading + mlead, pleading + mlead + size))
                    }
                }
            }
        };
    // Bounded fixpoint per axis — each pass places every child whose
    // deps resolved; cycles/missing ids leave the child at origin.
    let mut xf: Axis = vec![None; n];
    let mut yf: Axis = vec![None; n];
    for _ in 0..n {
        let mut progress = false;
        for (i, &ch) in kids.iter().enumerate() {
            if xf[i].is_none() {
                if let Some(e) = resolve_edges(vm, ch, &ks[i], &xf, true) {
                    xf[i] = Some(e);
                    progress = true;
                }
            }
            if yf[i].is_none() {
                if let Some(e) = resolve_edges(vm, ch, &ks[i], &yf, false) {
                    yf[i] = Some(e);
                    progress = true;
                }
            }
        }
        if !progress {
            break;
        }
    }
    for i in 0..n {
        let (l, r) = xf[i].unwrap_or_else(|| {
            let l = pad_l + ks[i].ml;
            (l, l + ks[i].w)
        });
        let (t, b) = yf[i].unwrap_or_else(|| {
            let t = pad_t + ks[i].mt;
            (t, t + ks[i].h)
        });
        ks[i].rect = (l, t, r, b);
    }
    ks
}

/// `RelativeLayout` subset: lay children out at the positions
/// `relative_positions` resolved. Without this every child lands on
/// the same origin — the pile of overlapped views that used to be the
/// whole month page.
fn relative_layout_fallback(vm: &mut Vm, this: u32, w: i32, h: i32) {
    let ks = relative_positions(vm, this, w, h);
    for k in &ks {
        let ch = k.ch;
        let (cl, ct, cr, cb) = k.rect;
        // AOSP RelativeLayout measures children twice — once raw, once
        // under the dependency-resolved extent. Re-measure a child
        // whose rule-resolved size differs from its first pass so its
        // own `onLayout`/descendants see the squeezed bounds instead
        // of the unconstrained measurement.
        let (rw, rh) = (cr - cl, cb - ct);
        if rw != k.w || rh != k.h {
            const EXACTLY: i32 = 1 << 30;
            // Read the LIVE LayoutParams dims — `setLayoutParams`
            // mutations beat the XML attrs. Spec per axis: a changed
            // axis gets the child's lp vs the resolved extent
            // (match_parent fills, wrap shrinks); an unchanged axis
            // keeps its measured size so a width-only rule can't
            // collapse the height.
            let lp_obj = match pseudo_get(vm, ch, P_LP) {
                Some(V::Ref(lp)) => lp,
                _ => 0,
            };
            let dim = |field: &str, xml: &str| {
                lp_field(vm, lp_obj, field)
                    .and_then(|v| lp_px(vm, v))
                    .or_else(|| attr_find(vm, ch, xml).map(|v| len_v(vm, v)))
                    .unwrap_or(-2)
            };
            let dw = dim("width", "layout_width");
            let dh = dim("height", "layout_height");
            let ws = if rw != k.w {
                child_measure_spec(EXACTLY | rw.max(0), 0, dw)
            } else {
                EXACTLY | rw.max(0)
            };
            let hs = if rh != k.h {
                child_measure_spec(EXACTLY | rh.max(0), 0, dh)
            } else {
                EXACTLY | rh.max(0)
            };
            let _ = vm.call_virtual(ch, "measure", &[V::I32(ws), V::I32(hs)]);
        }
        if crate::dbg_flag("OVELO_VIEWDBG") {
            let rules: Vec<String> = [
                "layout_centerInParent",
                "layout_centerHorizontal",
                "layout_centerVertical",
                "layout_alignParentRight",
                "layout_alignParentEnd",
                "layout_alignParentBottom",
                "layout_alignParentTop",
                "layout_alignParentLeft",
                "layout_toRightOf",
                "layout_toEndOf",
                "layout_toLeftOf",
                "layout_toStartOf",
                "layout_alignLeft",
                "layout_alignStart",
                "layout_alignRight",
                "layout_alignEnd",
                "layout_below",
                "layout_above",
                "layout_alignTop",
                "layout_alignBottom",
            ]
            .iter()
            .filter_map(|n| attr_find(vm, ch, n).map(|v| format!("{n}={v:?}")))
            .collect();
            eprintln!(
                "[viewdbg] relLayout h{ch} id={:#x} meas={}x{} at {cl},{ct} rules={:?}",
                k.id, k.w, k.h, rules
            );
        }
        if let Err(e) = vm.call_virtual(
            ch,
            "layout",
            &[V::I32(cl), V::I32(ct), V::I32(cr), V::I32(cb)],
        ) {
            if crate::dbg_flag("OVELO_VIEWDBG") {
                eprintln!("[viewdbg] relLayout h{ch} err {e}");
            }
        }
    }
}

/// Framework `*LayoutParams` ctors the dex subclasses invoke-super
/// into: `(Context, AttributeSet)` parses the XML attrs, `(int, int)`
/// takes raw dims, `(LayoutParams)` copies. Values go into the
/// instance's field map under every canon key the app's field refs
/// use — dex `iget` reads `Ly70.width`, `MarginLayoutParams.width`
/// and friends by exact canon key, and R8 names the owners freely.
fn lp_ctor(vm: &mut Vm, class: &str, sig: &str, args: &[V]) {
    let this = match args.first() {
        Some(V::Ref(h)) => *h,
        _ => return,
    };
    // Field keys are written under every owner the app's readers can
    // name — but only owners inside THIS object's own class hierarchy:
    // an unrelated class declaring its own `width` must not get a
    // LayoutParams value.
    let mut hier = hashbrown::HashSet::new();
    let mut d = class_of(vm, V::Ref(this));
    for _ in 0..32 {
        if !hier.insert(d.clone()) {
            break;
        }
        let Some(next) = vm.super_desc(&d) else { break };
        d = next;
    }
    hier.insert(class.to_string());
    let lp_keys = |vm: &Vm, name: &str, ty: &str| -> Vec<GIdx> {
        vm.field_keys_named(name, ty)
            .into_iter()
            .filter(|(owner, _)| hier.contains(owner) || owner.ends_with("LayoutParams;"))
            .map(|(_, g)| g)
            .collect()
    };
    let mut writes: Vec<(String, String, V)> = Vec::new();
    if sig.contains("AttributeSet") {
        for (attr, fields) in LP_ATTRS {
            let Some(v) = attrset_value(vm, args.get(2).copied().unwrap_or(V::Ref(0)), attr) else {
                continue;
            };
            for f in *fields {
                if *f == "weight" {
                    if let V::F32(_) | V::F64(_) | V::I32(_) = v {
                        writes.push((f.to_string(), "F".to_string(), v));
                    }
                } else if let Some(px) = lp_px(vm, v) {
                    writes.push((f.to_string(), "I".to_string(), V::I32(px)));
                }
            }
        }
        // Relative margins can't reach a canon field (dex never refs
        // `startMargin`/`endMargin`) — hold them on pseudos; the
        // resolveLayoutDirection fold writes them into left/right.
        // AOSP keeps `layout_margin` absolute-only — it must NOT feed
        // start/end, or the fold would later overwrite left/right
        // margins a caller mutated after construction.
        for (attr, key) in [
            ("layout_marginStart", P_MSTART),
            ("layout_marginEnd", P_MEND),
        ] {
            let v = attrset_value(vm, args.get(2).copied().unwrap_or(V::Ref(0)), attr)
                .and_then(|v| lp_px(vm, v));
            if let Some(px) = v {
                pseudo_set(vm, this, key, V::I32(px));
            }
        }
        let _ = class;
    } else if sig == "(II)V" {
        for (f, a) in [("width", args.get(1)), ("height", args.get(2))] {
            if let Some(v) = a.copied().and_then(|v| lp_px(vm, v)) {
                writes.push((f.to_string(), "I".to_string(), V::I32(v)));
            }
        }
    } else if sig.contains("LayoutParams") {
        // Copy ctor: carry every recognized LP field the source has.
        let src = match args.get(1).copied() {
            Some(V::Ref(h)) => h,
            _ => 0,
        };
        let src_fields = match vm.heap.get(src) {
            Some(Obj::Instance { fields, .. }) | Some(Obj::Host { fields, .. }) => {
                Some(fields.clone())
            }
            _ => None,
        };
        if let Some(src_fields) = src_fields {
            for (_, fields) in LP_ATTRS {
                for f in *fields {
                    let ty = if *f == "weight" { "F" } else { "I" };
                    // Read under every owner the SOURCE may have used —
                    // lp_keys (the receiver's hierarchy) would miss dims
                    // stored under the source's own obfuscated class.
                    if let Some(v) = vm
                        .field_keys_named(f, ty)
                        .iter()
                        .find_map(|(_, g)| src_fields.get(g).copied())
                    {
                        writes.push((f.to_string(), ty.to_string(), v));
                    }
                }
            }
            // Relative margins live on pseudos — carry them too (AOSP
            // copies startMargin/endMargin in the copy ctor).
            for key in [P_MSTART, P_MEND] {
                if let Some(v) = src_fields.get(&(key as u64)).copied() {
                    pseudo_set(vm, this, key, v);
                }
            }
        }
    }
    for (name, ty, v) in writes {
        for g in lp_keys(vm, &name, &ty) {
            if let Some(Obj::Instance { fields, .. }) | Some(Obj::Host { fields, .. }) =
                vm.heap.get_mut(this)
            {
                fields.insert(g, v);
            }
        }
    }
}

/// `Foo$LayoutParams` name for the nearest ancestor that declares one:
/// a dex subclass with no LP type of its own yields its framework
/// parent's canonical LP (`FrameLayout$LayoutParams`), not an invented
/// `MyFrame$LayoutParams` a dex `check-cast` would reject. A
/// dex-defined `Foo$LayoutParams` wins at each level, then the first
/// `Landroid/` ancestor's canonical name, else the ViewGroup base.
fn lp_class_for(vm: &Vm, parent: u32) -> String {
    let mut d = class_of(vm, V::Ref(parent));
    for _ in 0..32 {
        let cand = format!("{}$LayoutParams;", d.trim_end_matches(';'));
        let dex_def = vm
            .type_index_of(&cand)
            .and_then(|g| vm.def_loc_of(g))
            .is_some();
        if dex_def || d.starts_with("Landroid/") {
            return cand;
        }
        match vm.super_desc(&d) {
            Some(s) if s != d => d = s,
            _ => break,
        }
    }
    "Landroid/view/ViewGroup$LayoutParams;".to_string()
}

/// Default `Foo$LayoutParams` (wrap_content × wrap_content) for a
/// host-stub parent.
fn default_lp(vm: &mut Vm, parent: u32) -> V {
    let lp = lp_class_for(vm, parent);
    let h = vm.alloc_host(&lp, HostState::Empty);
    lp_ctor(vm, &lp, "(II)V", &[V::Ref(h), V::I32(-2), V::I32(-2)]);
    V::Ref(h)
}

/// Host-fabricated `ViewGroup$LayoutParams` for framework-stub parents:
/// a host object whose field map `lp_ctor` fills from the child attrs,
/// so dex `iget lp.width`/`lp.height` reads real dims (-1/-2/px).
fn gen_lp(vm: &mut Vm, parent: u32, attrs: V) -> V {
    let lp = lp_class_for(vm, parent);
    let ctx = pseudo_get(vm, parent, P_CONTEXT).unwrap_or(V::Ref(0));
    let h = vm.alloc_host(&lp, HostState::Empty);
    lp_ctor(
        vm,
        &lp,
        "(Landroid/content/Context;Landroid/util/AttributeSet;)V",
        &[V::Ref(h), ctx, attrs],
    );
    V::Ref(h)
}

fn is_group_tag(tag: &str) -> bool {
    let n = tag.rsplit('.').next().unwrap_or(tag);
    n.ends_with("Layout")
        || n.ends_with("Group")
        || matches!(
            n,
            "ScrollView"
                | "NestedScrollView"
                | "HorizontalScrollView"
                | "RecyclerView"
                | "ListView"
                | "GridView"
                | "ViewPager"
                | "ViewPager2"
                | "AdapterView"
                | "CardView"
                | "TabLayout"
                | "NavigationView"
                | "SwipeRefreshLayout"
                | "FragmentContainerView"
                | "ViewFlipper"
                | "ViewSwitcher"
                | "AppBarLayout"
                | "CoordinatorLayout"
                | "DrawerLayout"
                | "merge"
                | "fragment"
                | "include"
        )
}

/// Splice `<include layout="@layout/x">` elements: the referenced
/// layout's element list replaces the include tag, rebased to the
/// include's depth, with the include's `android:id` stamped on the
/// included root (matching LayoutInflater behavior). `<merge>` includes
/// unwrap their children without a root view.
fn expand_includes(vm: &Vm, elems: &[Element]) -> Vec<Element> {
    let mut expanded = elems.to_vec();
    for _ in 0..8 {
        if !expanded.iter().any(|e| e.name == "include") {
            break;
        }
        let mut next: Vec<Element> = Vec::with_capacity(expanded.len());
        for el in &expanded {
            if el.name != "include" {
                next.push(el.clone());
                continue;
            }
            let layout = match el.android("layout") {
                Some(Value::Reference(r)) => *r,
                _ => {
                    next.push(el.clone());
                    continue;
                }
            };
            let sub = vm.apk_res.as_ref().and_then(|r| r.layout_elements(layout));
            match sub {
                Some(sub) if !sub.is_empty() && sub[0].name == "merge" => {
                    // merge root contributes no view; its children splice
                    // in at the include's depth.
                    for s in sub.iter().skip(1) {
                        let mut c = s.clone();
                        c.depth = el.depth + s.depth.saturating_sub(1);
                        next.push(c);
                    }
                }
                Some(sub) if !sub.is_empty() => {
                    if crate::dbg_flag("OVELO_VIEWDBG") {
                        eprintln!(
                            "[viewdbg] include → {layout:#x} ({} elems, root {})",
                            sub.len(),
                            sub[0].name
                        );
                    }
                    let root_at = next.len();
                    for s in &sub {
                        let mut c = s.clone();
                        c.depth += el.depth;
                        next.push(c);
                    }
                    if let Some(id) = el_id(vm, el) {
                        set_id_attr(&mut next[root_at], id);
                    }
                    // AOSP LayoutInflater: the <include> tag's other
                    // attrs (layout_*, constraint anchors, visibility…)
                    // override the same-named ones on the included
                    // root — without them a `bottomToTopOf` anchor
                    // written only on <include> never reaches the
                    // solver and the widget stays at 0.
                    merge_include_attrs(&mut next[root_at], el);
                }
                // Unresolvable include stays as a plain view so its own
                // id remains findable.
                _ => next.push(el.clone()),
            }
        }
        expanded = next;
    }
    expanded
}

/// Merge an `<include>`'s attributes onto the included root element:
/// every attr except `layout` (the res reference itself) overrides or
/// augments the root's attributes.
fn merge_include_attrs(root: &mut Element, inc: &Element) {
    // AOSP: the include must declare *both* `layout_width` and
    // `layout_height` for either to apply; a one-sided declaration
    // falls back to the included root's own LayoutParams.
    let has_w = inc.attrs.iter().any(|a| a.name == "layout_width");
    let has_h = inc.attrs.iter().any(|a| a.name == "layout_height");
    let dims_merge = has_w && has_h;
    for a in &inc.attrs {
        if a.name == "layout"
            || (!dims_merge && (a.name == "layout_width" || a.name == "layout_height"))
        {
            continue;
        }
        match root.attrs.iter_mut().find(|ra| ra.name == a.name) {
            Some(ra) => ra.value = a.value.clone(),
            None => root.attrs.push(a.clone()),
        }
    }
}

/// Replace or add the `android:id` attribute on an element.
fn set_id_attr(el: &mut Element, id: i32) {
    for a in &mut el.attrs {
        if a.name == "id" {
            a.value = Value::Reference(id as u32);
            return;
        }
    }
    el.attrs.push(Attribute {
        name: "id".to_string(),
        ns: String::new(),
        value: Value::Reference(id as u32),
    });
}

fn build_tree(vm: &mut Vm, elems: &[Element], ctx: V) -> V {
    let expanded = expand_includes(vm, elems);
    if crate::dbg_flag("OVELO_VIEWDBG") {
        let tags: Vec<String> = expanded
            .iter()
            .map(|e| format!("d{}:{}", e.depth, e.name))
            .collect();
        eprintln!("[viewdbg] expanded: {}", tags.join(", "));
    }
    let mut stack: Vec<(usize, u32)> = Vec::new();
    let mut made: Vec<u32> = Vec::new();
    let mut root = 0u32;
    for el in &expanded {
        // `<view class="x"/>` encodes the class in an attribute.
        let name = if el.name == "view" {
            match el.android_str("class") {
                Some(c) => format!("view\u{0}{c}"),
                None => "view".to_string(),
            }
        } else {
            el.name.clone()
        };
        let attrs = attrset_of(vm, el);
        let v = make_view_named(vm, &name, attrs, ctx);
        if crate::dbg_flag("OVELO_VIEWDBG") {
            eprintln!("[viewdbg] elem d{} {} -> {v:?}", el.depth, el.name);
        }
        let V::Ref(h) = v else {
            continue;
        };
        made.push(h);
        pseudo_set(vm, h, P_ATTRS, attrs);
        if el.name == "merge" {
            pseudo_set(vm, h, P_MERGE, V::I32(1));
        }
        if let Some(id) = el_id(vm, el) {
            pseudo_set(vm, h, P_VIEW_ID, V::I32(id));
        }
        if let Some(Value::Str(t)) = el.android("tag") {
            let v = jstr(vm, t);
            pseudo_set(vm, h, P_TAG, v);
        }
        // `android:visibility` — the XML enum (0=visible, 1=invisible,
        // 2=gone) maps onto the View constants (0/4/8); AOSP honors it
        // in the ctor's AttributeSet read so getVisibility reflects it
        // before any setVisibility call.
        // A ctor-set visibility wins over the XML default — in AOSP the
        // attr is read inside the constructor, so a later setVisibility
        // call in the same ctor would override it.
        if pseudo_get(vm, h, P_VIS).is_none() {
            if let Some(v) = el.android("visibility") {
                let vis = match v {
                    Value::Int(1) => 4,
                    Value::Int(2) => 8,
                    Value::Int(_) => 0,
                    Value::Str(s) => match s.as_str() {
                        "invisible" => 4,
                        "gone" => 8,
                        _ => 0,
                    },
                    _ => 0,
                };
                pseudo_set(vm, h, P_VIS, V::I32(vis));
            }
        }
        if let Some(Value::Str(t)) = el.android("text") {
            let v = jstr(vm, t);
            pseudo_set(vm, h, P_TEXT, v);
        }
        if is_compound_desc(vm, &class_of(vm, V::Ref(h))) {
            if let Some(checked) = el.android("checked").and_then(|v| xml_value_bool(vm, v)) {
                pseudo_set(vm, h, P_CHECKED, V::I32(checked as i32));
            }
        }
        // ImageView sources declared in XML (`android:src` /
        // `app:srcCompat` — different namespaces, so match by local
        // name) seed P_IMAGE; the renderer resolves the res-id. A dex
        // constructor that already set an image wins — in AOSP the
        // XML default only applies when the ctor leaves it unset.
        let image_unset = pseudo_get(vm, h, P_IMAGE)
            .map(|v| is_null(v) || v == V::I32(0))
            .unwrap_or(true);
        if image_unset {
            if let Some(v) = el
                .attrs
                .iter()
                .find(|a| a.name == "src" || a.name == "srcCompat")
                .map(|a| attr_v(vm, &a.value))
            {
                pseudo_set(vm, h, P_IMAGE, v);
            }
        }
        while stack.last().is_some_and(|(d, _)| *d >= el.depth) {
            stack.pop();
        }
        match stack.last() {
            Some((_, p)) => {
                let p = *p;
                if crate::dbg_flag("OVELO_VIEWDBG") {
                    eprintln!("[viewdbg] attach h{h} -> parent h{p}");
                }
                attach(vm, p, V::Ref(h));
                // A parent with XML children is a container.
                pseudo_set(vm, p, P_IS_GROUP, V::I32(1));
            }
            None => root = h,
        }
        stack.push((el.depth, h));
    }
    // LayoutInflater finishes by calling `onFinishInflate()` on every
    // constructed view, children first — fossify's fragments bind their
    // `innerBinding` there.
    for h in made.iter().rev() {
        if let Some(Obj::Instance { .. }) = vm.heap.get(*h) {
            let _ = vm.call_virtual(*h, "onFinishInflate", &[]);
        }
    }
    if root == 0 {
        empty_view(vm)
    } else {
        V::Ref(root)
    }
}

/// `android:id` of an element — `@+id/name`, `@id/name`, or a raw int.
fn el_id(vm: &Vm, el: &Element) -> Option<i32> {
    match el.android("id") {
        Some(Value::Reference(r)) | Some(Value::Other { data: r, .. }) => Some(*r as i32),
        Some(Value::Int(i)) => Some(*i as i32),
        Some(Value::Str(s)) => {
            let name = s
                .trim_start_matches("@+id/")
                .trim_start_matches("@id/")
                .trim_start_matches('@');
            vm.apk_res
                .as_ref()
                .and_then(|r| r.id_named(name))
                .map(|i| i as i32)
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------
// tree storage

fn tag_key(id: i32) -> u64 {
    // Keyed tags share the pseudo-field map with host bookkeeping.
    // Keep them in a separate u64 namespace: view pseudo keys live in
    // the low u32 range, while Android tag ids are arbitrary signed
    // 32-bit values whose low bits otherwise collide with those keys.
    (1u64 << 32) | (id as u32 as u64)
}

fn children_arr(vm: &mut Vm, h: u32) -> Option<u32> {
    match pseudo_get(vm, h, P_CHILDREN) {
        Some(V::Ref(a)) if a != 0 => Some(a),
        _ => {
            let a = vm.heap.alloc(Obj::Array {
                elem: "Landroid/view/View;".to_string(),
                data: Vec::new(),
            });
            pseudo_set(vm, h, P_CHILDREN, V::Ref(a));
            Some(a)
        }
    }
}

pub(crate) fn children(vm: &Vm, h: u32) -> Vec<V> {
    match pseudo_get(vm, h, P_CHILDREN) {
        Some(V::Ref(a)) => match vm.heap.get(a) {
            Some(Obj::Array { data, .. }) => data.clone(),
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

fn generate_view_id() -> i32 {
    // AOSP: AtomicInteger starting at 1 — dex-created views and
    // RadioGroup's automatic child ids need nonzero unique values.
    static NEXT: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

fn view_id(vm: &Vm, h: u32) -> i32 {
    match pseudo_get(vm, h, P_VIEW_ID) {
        Some(V::I32(i)) => i,
        _ => -1,
    }
}

/// Route a host pointer event into the dex view tree: the deepest view
/// whose frame contains (x,y) gets a MotionEvent, propagated up the
/// parent chain until handled — dex `dispatchTouchEvent` /
/// `onTouchEvent` first, then a registered `OnTouchListener`. Without
/// this bridge, dex-materialized views (games: libGDX's input view)
/// never see host input.
fn view_or_descendant(vm: &Vm, ancestor: u32, mut view: u32) -> bool {
    for _ in 0..32 {
        if view == 0 {
            return false;
        }
        if view == ancestor {
            return true;
        }
        view = match pseudo_get(vm, view, P_PARENT) {
            Some(V::Ref(p)) => p,
            _ => return false,
        };
    }
    false
}

pub(crate) fn dispatch_touch(
    vm: &mut Vm,
    x: i32,
    y: i32,
    action: i32,
) -> ovelo_error::Result<bool> {
    // Ok(true) when the dex tree claimed the point — the caller then
    // skips its own (host-widget) dispatch for the gesture so a view
    // mirrored in both trees never fires its listener twice.
    // Child frames are stored in the PARENT's content coordinates
    // (group_layout_fallback), so the hit test must carry each
    // ancestor's absolute origin down the recursion.
    fn deepest(vm: &Vm, h: u32, x: i32, y: i32, ox: i32, oy: i32) -> Option<u32> {
        // AOSP: INVISIBLE/GONE views can't receive touches.
        if pint(vm, h, P_VIS) != 0 {
            return None;
        }
        let l = ox + pint(vm, h, P_LEFT);
        let t = oy + pint(vm, h, P_TOP);
        let r = ox + pint(vm, h, P_RIGHT);
        let b = oy + pint(vm, h, P_BOTTOM);
        if crate::dbg_flag("OVELO_TOUCHDBG") {
            eprintln!(
                "[touchdbg]   try h{h} {} {l},{t},{r},{b}",
                vm.dbg_class_of(h)
            );
        }
        let inside = x >= l && x < r && y >= t && y < b;
        if !inside && r > l && b > t {
            return None;
        }
        // Later children draw on top — hit-test them first. A node
        // with a degenerate frame (r<=l || b<=t — the layout pass
        // never ran or produced 0×0, e.g. a FragmentContainerView
        // whose fragment tree was materialized without a real layout)
        // still descends: its children can carry real frames, and
        // pruning here leaves the whole subtree untouchable.
        for c in children(vm, h).iter().rev() {
            if let V::Ref(ch) = c {
                if *ch != 0 {
                    if let Some(v) = deepest(
                        vm,
                        *ch,
                        x,
                        y,
                        l - pint(vm, h, P_SCROLL_X),
                        t - pint(vm, h, P_SCROLL_Y),
                    ) {
                        return Some(v);
                    }
                }
            }
        }
        // A degenerate-frame node claims nothing by itself.
        if !inside {
            return None;
        }
        Some(h)
    }
    fn abs_origin(vm: &Vm, v: u32) -> (i32, i32) {
        // Child frames live in the parent's content coordinates: a
        // scrolled parent shifts every descendant opposite to its
        // scroll offset. Walk root→leaf so each parent's scroll is
        // applied exactly once.
        let mut chain = vec![v];
        let mut cur = v;
        for _ in 0..64 {
            cur = match pseudo_get(vm, cur, P_PARENT) {
                Some(V::Ref(p)) if p != 0 => p,
                _ => break,
            };
            chain.push(cur);
        }
        chain.reverse();
        let mut ox = 0;
        let mut oy = 0;
        for i in 0..chain.len() {
            let h = chain[i];
            ox += pint(vm, h, P_LEFT);
            oy += pint(vm, h, P_TOP);
            if i + 1 < chain.len() {
                ox -= pint(vm, h, P_SCROLL_X);
                oy -= pint(vm, h, P_SCROLL_Y);
            }
        }
        (ox, oy)
    }
    let dbg = crate::dbg_flag("OVELO_TOUCHDBG");
    // An outside-tap dismissal consumes the whole gesture. The DOWN is
    // handled below; remember it with a sentinel so the paired UP cannot
    // fall through to the newly exposed background and fire its click.
    if (action == 1 || action == 2 || action == 3) && vm.touch_gesture_target == u32::MAX {
        if action != 2 {
            vm.touch_gesture_target = 0;
            vm.touch_gesture_down_time = -1;
        }
        return Ok(true);
    }
    // Once a dex view owns DOWN, the rest of the gesture goes to that
    // view even when the pointer leaves its bounds. Re-hit-testing each
    // MOVE/UP would deliver a move series to a neighbor that never saw
    // the original DOWN.
    let mut target = if action != 0 && vm.touch_gesture_target != 0 {
        vm.touch_gesture_target
    } else {
        0
    };
    if target == 0 {
        for r in vm.view_roots.clone().iter().rev() {
            if dbg {
                eprintln!(
                    "[touchdbg] root h{r} {} {},{},{},{}",
                    vm.dbg_class_of(*r),
                    pint(vm, *r, P_LEFT),
                    pint(vm, *r, P_TOP),
                    pint(vm, *r, P_RIGHT),
                    pint(vm, *r, P_BOTTOM)
                );
            }
            // A laid-out root only wins when the point is inside it —
            // view_roots also collects detached adapter rows/dialog
            // trees, and the last one would otherwise swallow every tap.
            let l = pint(vm, *r, P_LEFT);
            let t = pint(vm, *r, P_TOP);
            let rr = pint(vm, *r, P_RIGHT);
            let b = pint(vm, *r, P_BOTTOM);
            let inside = if rr > l || b > t {
                x >= l && x < rr && y >= t && y < b
            } else {
                // Never laid out yet — accept so early taps still land.
                true
            };
            if !inside {
                continue;
            }
            // A root's frame is already absolute (screen origin).
            if let Some(v) = deepest(vm, *r, x, y, 0, 0) {
                target = v;
                break;
            }
        }
    }
    if target == 0 {
        if dbg {
            eprintln!(
                "[touchdbg] {x},{y} a{action} no hit (roots {})",
                vm.view_roots.len()
            );
        }
        return Ok(false);
    }
    if dbg {
        eprintln!(
            "[touchdbg] {x},{y} a{action} hit h{target} {}",
            vm.dbg_class_of(target)
        );
    }
    if action == 0 && is_edit_text(vm, target) {
        clear_window_focus(vm);
        pseudo_set(vm, target, P_FOCUSED, V::I32(1));
    }
    // Modal popup semantics: ACTION_DOWN outside an open popup dismisses
    // it and is consumed instead of activating the background control.
    if action == 0 {
        let popups: Vec<(u32, u32)> = vm
            .view_roots
            .iter()
            .filter_map(|r| match pseudo_get(vm, *r, P_POPUP_ROOT) {
                Some(V::Ref(owner)) if owner != 0 => Some((owner, *r)),
                _ => None,
            })
            .collect();
        let mut consumed = false;
        let inside_overlay = popups
            .iter()
            .any(|(_, proot)| view_or_descendant(vm, *proot, target));
        for (popup, proot) in popups {
            if inside_overlay {
                break;
            }
            if !view_or_descendant(vm, proot, target) {
                let cancelable = pseudo_get(vm, popup, P_DIALOG_CANCELABLE)
                    .map(i32v)
                    .unwrap_or(1);
                if dbg {
                    eprintln!("[touchdbg] outside tap dismiss popup h{popup} root h{proot}");
                }
                if cancelable != 0 {
                    let popup_class = class_of(vm, V::Ref(popup));
                    if desc_is_a(&popup_class, "Landroid/app/Dialog;")
                        || vm.assignable_desc(&popup_class, "Landroid/app/Dialog;")
                    {
                        dialog_dismiss(vm, popup);
                    } else {
                        list_popup_dismiss(vm, popup);
                    }
                }
                // Modal overlays consume outside touches even when they
                // stay open; otherwise the tap falls through and activates
                // a button behind a non-cancelable dialog.
                consumed = true;
            }
        }
        if consumed {
            vm.touch_gesture_target = u32::MAX;
            vm.touch_gesture_down_time = now_ms();
            return Ok(true);
        }
    }
    let V::Ref(evt) = host(vm, "Landroid/view/MotionEvent;", HostState::Empty) else {
        return Ok(false);
    };
    pseudo_set(vm, evt, P_ME_ACTION, V::I32(action));
    let now = now_ms();
    if action == 0 {
        vm.touch_gesture_down_time = now;
        vm.touch_gesture_target = target;
    }
    let down_time = if vm.touch_gesture_down_time >= 0 {
        vm.touch_gesture_down_time
    } else {
        now
    };
    pseudo_set(vm, evt, P_ME_DOWN_TIME, V::I64(down_time));
    pseudo_set(vm, evt, P_ME_EVENT_TIME, V::I64(now));
    if action == 1 || action == 3 {
        vm.touch_gesture_down_time = -1;
        if action == 3 {
            vm.touch_gesture_target = 0;
        }
    }
    pseudo_set(vm, evt, P_ME_RAWX, V::F32(x as f32));
    pseudo_set(vm, evt, P_ME_RAWY, V::F32(y as f32));
    let mut v = target;
    for _ in 0..32 {
        if v == 0 {
            break;
        }
        // getX/getY report coordinates relative to the receiving
        // view (AOSP transforms on dispatch) — rewrite before each hop.
        let (ox, oy) = abs_origin(vm, v);
        pseudo_set(vm, evt, P_ME_X, V::F32((x - ox) as f32));
        pseudo_set(vm, evt, P_ME_Y, V::F32((y - oy) as f32));
        if vm.has_dex_virtual(v, "dispatchTouchEvent", &["Landroid/view/MotionEvent;"]) {
            if dbg {
                eprintln!(
                    "[touchdbg] h{v} dex dispatchTouchEvent {}",
                    vm.dbg_class_of(v)
                );
            }
            let dr = vm.call_virtual(v, "dispatchTouchEvent", &[V::Ref(evt)]);
            if matches!(dr, Ok(Some(rv)) if i32v(rv) != 0) {
                return Ok(true);
            }
            v = match pseudo_get(vm, v, P_PARENT) {
                Some(V::Ref(p)) => p,
                _ => break,
            };
            continue;
        }
        if let Some(V::Ref(l)) = pseudo_get(vm, v, P_TOUCH_L) {
            if l != 0 {
                if dbg {
                    eprintln!("[touchdbg] h{v} OnTouchListener.onTouch l{l}");
                }
                if matches!(
                    vm.call_virtual(l, "onTouch", &[V::Ref(v), V::Ref(evt)]),
                    Ok(Some(rv)) if i32v(rv) != 0
                ) {
                    return Ok(true);
                }
            }
        }
        if vm.has_dex_virtual(v, "onTouchEvent", &["Landroid/view/MotionEvent;"]) {
            if dbg {
                eprintln!("[touchdbg] h{v} dex onTouchEvent {}", vm.dbg_class_of(v));
            }
            if matches!(
                vm.call_virtual(v, "onTouchEvent", &[V::Ref(evt)]),
                Ok(Some(rv)) if i32v(rv) != 0
            ) {
                return Ok(true);
            }
        }
        // AOSP `View.onTouchEvent`: a clickable view consumes the touch
        // and `performClick` fires the OnClickListener on ACTION_UP.
        // The deepest ancestor with a listener wins.
        if action == 1 {
            let listener = match pseudo_get(vm, v, P_LISTENER) {
                Some(V::Ref(l)) if l != 0 => Some(l),
                _ => None,
            };
            let clickable = listener.is_some()
                || pseudo_get(vm, v, P_DIALOG_BUTTON_WHICH).is_some()
                || is_compound_desc(vm, &class_of(vm, V::Ref(v)));
            if clickable {
                let down_target = vm.touch_gesture_target;
                let (vx, vy) = abs_origin(vm, v);
                let vw = pint(vm, v, P_RIGHT) - pint(vm, v, P_LEFT);
                let vh = pint(vm, v, P_BOTTOM) - pint(vm, v, P_TOP);
                let up_inside = x >= vx && x < vx + vw && y >= vy && y < vy + vh;
                if down_target != 0 && view_or_descendant(vm, v, down_target) {
                    // The first clickable owner of the DOWN gesture
                    // consumes UP even when the release is outside it;
                    // otherwise cancelling a child button bubbles into
                    // the parent row and fires the row's action.
                    if up_inside {
                        if dbg {
                            eprintln!("[touchdbg] h{v} performClick listener={listener:?}");
                        }
                        if let Err(e) = vm.call_virtual(v, "performClick", &[]) {
                            if crate::dbg_flag("OVELO_TOUCHDBG") {
                                eprintln!("[touchdbg] performClick error: {e:?}");
                            }
                        }
                    }
                    vm.touch_gesture_target = 0;
                    return Ok(true);
                }
            }
            if let Some(mname) = attr_find(vm, v, "onClick").and_then(|a| match a {
                V::Ref(r) if r != 0 => s(vm, r),
                _ => None,
            }) {
                let act = vm.cur_activity;
                if act != 0 {
                    if dbg {
                        eprintln!("[touchdbg] h{v} xml onClick {mname}");
                    }
                    let _ = vm.call_virtual(act, &mname, &[V::Ref(v)]);
                    return Ok(true);
                }
            }
        }
        v = match pseudo_get(vm, v, P_PARENT) {
            Some(V::Ref(p)) => p,
            _ => break,
        };
    }
    // AOSP: touches no view consumed reach Activity.onTouchEvent —
    // fullscreen games hook input there rather than on a view.
    let act = vm.cur_activity;
    if dbg {
        eprintln!(
            "[touchdbg] act={act} ote={}",
            vm.has_dex_virtual(act, "onTouchEvent", &["Landroid/view/MotionEvent;"])
        );
    }
    if act != 0 && vm.has_dex_virtual(act, "onTouchEvent", &["Landroid/view/MotionEvent;"]) {
        pseudo_set(vm, evt, P_ME_X, V::F32(x as f32));
        pseudo_set(vm, evt, P_ME_Y, V::F32(y as f32));
        if dbg {
            eprintln!("[touchdbg] activity onTouchEvent h{act}");
        }
        let _ = vm.call_virtual(act, "onTouchEvent", &[V::Ref(evt)]);
        return Ok(true);
    }
    if dbg {
        eprintln!("[touchdbg] {x},{y} a{action} unhandled");
    }
    // The hit view had no dex dispatchTouchEvent/onTouchEvent, no
    // listener, and no activity hook — the host tree may hold the
    // handler (host-materialized views keep listeners host-side).
    Ok(false)
}

/// Record a click listener on a dex view when the high-level UI bridge
/// has no host widget for it. Dispatch/materialization both read this
/// same pseudo state.
pub(crate) fn set_click_listener(vm: &mut Vm, view: u32, listener: V) {
    pseudo_set(vm, view, P_LISTENER, listener);
}

pub(crate) fn has_input_connection(vm: &Vm, view: u32) -> bool {
    matches!(pseudo_get(vm, view, P_INPUT_CONN), Some(V::Ref(h)) if h != 0)
}

fn clear_focus_tree(vm: &mut Vm, h: u32, depth: usize) {
    if h == 0 || depth > 40 {
        return;
    }
    pseudo_set(vm, h, P_FOCUSED, V::I32(0));
    for c in children(vm, h) {
        if let V::Ref(ch) = c {
            if ch != h {
                clear_focus_tree(vm, ch, depth + 1);
            }
        }
    }
}

fn clear_window_focus(vm: &mut Vm) {
    for root in key_scan_roots(vm) {
        clear_focus_tree(vm, root, 0);
    }
}

/// Ask a view for the InputConnection the system IME would use and keep
/// it for later key/IME delivery. Android's real InputMethodManager
/// performs this handshake when soft input is shown; without it Compose's
/// text-input session exists but no connection ever reaches the runtime.
pub(crate) fn offer_input_connection(vm: &mut Vm, view: u32) -> ovelo_error::Result<bool> {
    if view == 0 {
        return Ok(false);
    }
    let editor = host(
        vm,
        "Landroid/view/inputmethod/EditorInfo;",
        HostState::Empty,
    );
    let out = vm.call_virtual_sig(
        view,
        "onCreateInputConnection",
        &["Landroid/view/inputmethod/EditorInfo;"],
        &[editor],
    )?;
    let conn = match out {
        Some(V::Ref(h)) => h,
        _ => 0,
    };
    if conn != 0 {
        pseudo_set(vm, view, P_INPUT_CONN, V::Ref(conn));
    }
    if crate::dbg_flag("OVELO_KEYDBG") {
        eprintln!("[keydbg] ime offer conn h{conn} view h{view}");
    }
    Ok(conn != 0)
}

fn is_edit_text(vm: &Vm, h: u32) -> bool {
    if h == 0 {
        return false;
    }
    let desc = class_of(vm, V::Ref(h));
    desc.contains("EditText") || vm.extends_named(&desc, "EditText")
}

fn key_scan_roots(vm: &Vm) -> Vec<u32> {
    // Decor first, then any inflated-but-detached roots. Fragment
    // materialization can leave a real editor outside the synthetic
    // decor while still rendering it as a view root; key input must not
    // become unreachable merely because attachment is incomplete.
    let mut roots: Vec<u32> = vm.decor_roots.values().copied().collect();
    for r in &vm.view_roots {
        if !roots.contains(r) {
            roots.push(*r);
        }
    }
    roots
}

fn text_value_string(vm: &Vm, v: V) -> Option<String> {
    match v {
        V::I32(id) if id > 0 => vm
            .apk_res
            .as_ref()
            .and_then(|r| r.string_of(id as u32))
            .map(str::to_string),
        _ => sv(vm, v),
    }
}

fn focused_edit_text(vm: &Vm, h: u32, depth: usize) -> Option<u32> {
    if depth > 40 || pint(vm, h, P_VIS) != 0 {
        return None;
    }
    if pint(vm, h, P_FOCUSED) != 0 && is_edit_text(vm, h) {
        return Some(h);
    }
    for c in children(vm, h) {
        if let V::Ref(ch) = c {
            if ch != 0 && ch != h {
                if let Some(v) = focused_edit_text(vm, ch, depth + 1) {
                    return Some(v);
                }
            }
        }
    }
    None
}

/// Route a host key event into the top-most dex view root. Compose
/// overrides `dispatchKeyEvent` and performs focus/key-input traversal
/// internally; classic views can override the same method.
pub(crate) fn dispatch_key(
    vm: &mut Vm,
    action: i32,
    keycode: i32,
    unicode: i32,
    repeat: bool,
) -> ovelo_error::Result<bool> {
    fn dispatch_node(vm: &mut Vm, h: u32, evt: u32, depth: usize) -> ovelo_error::Result<bool> {
        if depth > 40 {
            return Ok(false);
        }
        for c in children(vm, h) {
            if let V::Ref(ch) = c {
                if ch != 0 && ch != h && dispatch_node(vm, ch, evt, depth + 1)? {
                    return Ok(true);
                }
            }
        }
        if !vm.has_dex_virtual(h, "dispatchKeyEvent", &["Landroid/view/KeyEvent;"]) {
            return Ok(false);
        }
        if crate::dbg_flag("OVELO_KEYDBG") {
            eprintln!("[keydbg] dispatch h{h} {}", vm.dbg_class_of(h));
        }
        Ok(vm
            .call_virtual_sig(
                h,
                "dispatchKeyEvent",
                &["Landroid/view/KeyEvent;"],
                &[V::Ref(evt)],
            )?
            .map(|v| i32v(v) != 0)
            .unwrap_or(false))
    }

    fn input_view(vm: &mut Vm, h: u32, depth: usize) -> Option<u32> {
        if depth > 40 {
            return None;
        }
        if vm.has_dex_virtual(
            h,
            "onCreateInputConnection",
            &["Landroid/view/inputmethod/EditorInfo;"],
        ) {
            return Some(h);
        }
        for c in children(vm, h) {
            if let V::Ref(ch) = c {
                if ch != 0 && ch != h {
                    if let Some(v) = input_view(vm, ch, depth + 1) {
                        return Some(v);
                    }
                }
            }
        }
        None
    }

    fn create_input_connection(vm: &mut Vm, view: u32) -> ovelo_error::Result<u32> {
        let editor = host(
            vm,
            "Landroid/view/inputmethod/EditorInfo;",
            HostState::Empty,
        );
        let out = vm.call_virtual_sig(
            view,
            "onCreateInputConnection",
            &["Landroid/view/inputmethod/EditorInfo;"],
            &[editor],
        )?;
        let conn = match out {
            Some(V::Ref(h)) => h,
            _ => 0,
        };
        if conn != 0 {
            pseudo_set(vm, view, P_INPUT_CONN, V::Ref(conn));
        }
        if crate::dbg_flag("OVELO_KEYDBG") {
            eprintln!("[keydbg] input conn h{conn} view h{view}");
        }
        Ok(conn)
    }

    fn input_connection(vm: &mut Vm, view: u32, refresh: bool) -> ovelo_error::Result<u32> {
        if !refresh {
            if let Some(V::Ref(conn)) = pseudo_get(vm, view, P_INPUT_CONN) {
                if conn != 0 {
                    return Ok(conn);
                }
            }
        }
        create_input_connection(vm, view)
    }

    fn call_input(
        vm: &mut Vm,
        view: u32,
        method: &str,
        params: &[&str],
        args: &[V],
    ) -> ovelo_error::Result<bool> {
        for attempt in 0..2 {
            let conn = input_connection(vm, view, attempt == 1)?;
            if conn == 0 {
                return Ok(false);
            }
            let mut a = Vec::with_capacity(args.len() + 1);
            a.extend_from_slice(args);
            let handled = vm
                .call_virtual_sig(conn, method, params, &a)?
                .map(|v| i32v(v) != 0)
                .unwrap_or(false);
            if crate::dbg_flag("OVELO_KEYDBG") {
                eprintln!("[keydbg] input {method} conn h{conn} handled={handled}");
            }
            if handled {
                return Ok(true);
            }
            pseudo_set(vm, view, P_INPUT_CONN, V::Ref(0));
        }
        Ok(false)
    }

    let V::Ref(evt) = host(vm, "Landroid/view/KeyEvent;", HostState::Empty) else {
        return Ok(false);
    };
    pseudo_set(vm, evt, P_KE_ACTION, V::I32(action));
    pseudo_set(vm, evt, P_KE_CODE, V::I32(keycode));
    pseudo_set(vm, evt, P_KE_UNICODE, V::I32(unicode));
    pseudo_set(vm, evt, P_KE_REPEAT, V::I32(repeat as i32));

    // Classic EditText is a framework skeleton here, so it has no dex
    // InputConnection implementation for `input_view` to discover. Give
    // the focused editor the same visible editing behavior for host text
    // keys; `getText` already reads P_TEXT in this runtime.
    let focused_edit = key_scan_roots(vm)
        .iter()
        .rev()
        .find_map(|r| focused_edit_text(vm, *r, 0));
    if let Some(view) = focused_edit {
        if action == 0 {
            let mut text = pseudo_get(vm, view, P_TEXT)
                .or_else(|| attr_find(vm, view, "text"))
                .and_then(|v| text_value_string(vm, v))
                .unwrap_or_default();
            let mut changed = false;
            if keycode == 67 {
                changed = text.pop().is_some();
            } else if unicode != 0 {
                if let Some(ch) = char::from_u32(unicode as u32) {
                    if !ch.is_control() {
                        text.push(ch);
                        changed = true;
                    }
                }
            }
            if changed {
                let text = jdynstr(vm, &text);
                pseudo_set(vm, view, P_TEXT, text);
                return Ok(true);
            }
        }
    }

    let mut ime_view = 0;
    for root in key_scan_roots(vm).iter().rev() {
        if matches!(pseudo_get(vm, *root, P_PARENT), Some(V::Ref(p)) if p != 0) {
            continue;
        }
        if let Some(v) = input_view(vm, *root, 0) {
            ime_view = v;
            break;
        }
    }
    if ime_view != 0 {
        // Android text input normally reaches a focused field through
        // its InputConnection, not as a direct View key insertion. ASCII
        // host keys become commitText; editing/navigation keys stay
        // KeyEvents so Compose's key-input node can consume them.
        if action == 0 && unicode != 0 {
            if let Some(ch) = char::from_u32(unicode as u32) {
                if !ch.is_control() {
                    let text = jstr(vm, &ch.to_string());
                    if call_input(
                        vm,
                        ime_view,
                        "commitText",
                        &["Ljava/lang/CharSequence;", "I"],
                        &[text, V::I32(1)],
                    )? {
                        return Ok(true);
                    }
                }
            }
        }
    }

    for root in key_scan_roots(vm).iter().rev() {
        if matches!(pseudo_get(vm, *root, P_PARENT), Some(V::Ref(p)) if p != 0) {
            continue;
        }
        if crate::dbg_flag("OVELO_KEYDBG") {
            eprintln!(
                "[keydbg] root h{root} {} a{action} code{keycode} u{unicode}",
                vm.dbg_class_of(*root)
            );
        }
        if dispatch_node(vm, *root, evt, 0)? {
            return Ok(true);
        }
    }
    Ok(vm.cur_activity != 0
        && vm
            .call_virtual_sig(
                vm.cur_activity,
                "dispatchKeyEvent",
                &["Landroid/view/KeyEvent;"],
                &[V::Ref(evt)],
            )?
            .map(|v| i32v(v) != 0)
            .unwrap_or(false))
}

pub(crate) fn attach(vm: &mut Vm, parent: u32, child: V) {
    attach_at(vm, parent, child, None, true, None)
}

/// `idx` mirrors ViewGroup's positional add/attach: absent or past-the-
/// end appends, otherwise the child lands at that child index. `notify`
/// fires the dex `onViewAdded` callback — real `addView` paths only;
/// AOSP's transient `attachViewToParent` never notifies.
pub(crate) fn attach_at(
    vm: &mut Vm,
    parent: u32,
    child: V,
    idx: Option<usize>,
    notify: bool,
    lp: Option<V>,
) {
    if parent == 0 {
        return;
    }
    // Attaching an ancestor under its own descendant makes a cycle —
    // recursive walks (find_in_tree, materialize) would never end.
    // Appcompat's decor layouts contain an android.R.id.content frame,
    // so a tree can legitimately meet itself here; refuse the link.
    if let V::Ref(ch) = child {
        let mut cur = parent;
        for _ in 0..64 {
            if cur == ch {
                if crate::dbg_flag("OVELO_VIEWDBG") {
                    eprintln!(
                        "[viewdbg] attach REFUSED h{ch} -> parent h{parent} (cycle) class={}",
                        vm.dbg_class_of(ch)
                    );
                }
                return;
            }
            match pseudo_get(vm, cur, P_PARENT) {
                Some(V::Ref(p)) if p != 0 => cur = p,
                _ => break,
            }
        }
    }
    if let V::Ref(ch) = child {
        let parent_cls = class_of(vm, V::Ref(parent));
        let child_cls = class_of(vm, V::Ref(ch));
        if ch != 0
            && view_id(vm, ch) == -1
            && is_radio_desc(vm, &child_cls)
            && (parent_cls == "Landroid/widget/RadioGroup;"
                || vm.assignable_desc(&parent_cls, "Landroid/widget/RadioGroup;"))
        {
            // RadioGroup's hierarchy listener generates an id for an
            // id-less RadioButton before tracking checked state.
            pseudo_set(vm, ch, P_VIEW_ID, V::I32(generate_view_id()));
        }
    }

    if let Some(a) = children_arr(vm, parent) {
        // Re-attaching the same child (dex scrap/attach flows do this)
        // must move it, not duplicate it: a later detach removes one
        // copy and clears its parent, leaving a parent-less duplicate
        // the dex ChildHelper still hands to the layout manager.
        let old = match vm.heap.get(a) {
            Some(Obj::Array { data, .. }) => data.iter().position(|&c| value_eq(vm, c, child)),
            _ => None,
        };
        if let Some(Obj::Array { data, .. }) = vm.heap.get_mut(a) {
            if let Some(old) = old {
                data.remove(old);
            }
            match idx {
                Some(i) if i <= data.len() => data.insert(i, child),
                _ => data.push(child),
            }
        }
        pseudo_set(vm, parent, P_IS_GROUP, V::I32(1));
    }
    if let V::Ref(ch) = child {
        pseudo_set(vm, ch, P_PARENT, V::Ref(parent));
        let parent_cls = class_of(vm, V::Ref(parent));
        let child_cls = class_of(vm, V::Ref(ch));
        if ch != 0
            && pint(vm, ch, P_CHECKED) != 0
            && is_radio_desc(vm, &child_cls)
            && (parent_cls == "Landroid/widget/RadioGroup;"
                || vm.assignable_desc(&parent_cls, "Landroid/widget/RadioGroup;"))
        {
            let id = view_id(vm, ch);
            radio_group_set_checked(vm, parent, id);
            if view_parent(vm, ch) != parent {
                return;
            }
        }
        if crate::dbg_flag("OVELO_VIEWDBG") {
            eprintln!(
                "[viewdbg] attach_at h{ch} <- h{parent} parent_att={} cls={}",
                pint(vm, parent, P_ATTACHED),
                vm.dbg_class_of(ch)
            );
        }
        // AOSP ViewGroup.addView → child.dispatchAttachedToWindow when
        // the parent is already attached — set the flag first (AttachInfo
        // is installed before the callback), then deliver the callback.
        // AndroidComposeView.onAttachedToWindow is where the Compose
        // root LayoutNode gets its Owner; skipping it leaves
        // DepthSortedSet.remove crashing on unattached nodes.
        if pint(vm, parent, P_ATTACHED) == 1 {
            mark_attached(vm, ch, 0);
        }
        // AOSP addViewInner checkLayoutParams → generateLayoutParams
        // first: ConstraintLayout's onViewAdded reads getLayoutParams()
        // and mints the child widget from the LP, so the LP must exist
        // before the notification lands.
        // AOSP addViewInner → child.setLayoutParams → requestLayout:
        // the child must report isLayoutRequested or dex parents
        // (ConstraintLayout.updateHierarchy) skip rebuilding the
        // widget list, leaving the solver with no children at 0,0,0,0.
        mark_layout_requested(vm, ch);
        // A passed-in LP wins — dex callers (RecyclerView's
        // attachViewToParent) already built the real params.
        if let Some(v @ V::Ref(lph)) = lp {
            if lph != 0 {
                pseudo_set(vm, ch, P_LP, v);
            }
        }
        if pseudo_get(vm, ch, P_LP).is_none() {
            let attrs = pseudo_get(vm, ch, P_ATTRS).unwrap_or(V::Ref(0));
            let lp = vm
                .call_virtual_sig(
                    parent,
                    "generateLayoutParams",
                    &["Landroid/util/AttributeSet;"],
                    &[attrs],
                )
                .ok()
                .flatten()
                .or_else(|| {
                    vm.call_virtual_rt(parent, "generateDefaultLayoutParams", &[])
                        .ok()
                        .flatten()
                });
            if let Some(v @ V::Ref(lp)) = lp {
                if lp != 0 {
                    pseudo_set(vm, ch, P_LP, v);
                }
            }
        }
        // AOSP addViewInner: `checkLayoutParams` gates whatever LP the
        // child ended up with — a foreign type (e.g. a bare
        // ViewGroup$LayoutParams on a ConstraintLayout child, or a
        // FrameLayout$LayoutParams from another parent) is wrapped by
        // generateLayoutParams so the parent's own LP class — whose
        // ctor mints ConstraintLayout's solver widget — is what the
        // child's getLayoutParams returns. Storing a foreign LP
        // verbatim leaves `lp.widget` null and the dex onLayout NPEs
        // on ConstraintWidget.getX.
        if let Some(v @ V::Ref(lph)) = pseudo_get(vm, ch, P_LP) {
            if lph != 0 {
                let ok = vm
                    .call_virtual_sig(
                        parent,
                        "checkLayoutParams",
                        &["Landroid/view/ViewGroup$LayoutParams;"],
                        &[v],
                    )
                    .ok()
                    .flatten()
                    .map(|r| i32v(r) != 0)
                    .unwrap_or(false);
                if !ok {
                    let wrapped = vm
                        .call_virtual_sig(
                            parent,
                            "generateLayoutParams",
                            &["Landroid/view/ViewGroup$LayoutParams;"],
                            &[v],
                        )
                        .ok()
                        .flatten()
                        .unwrap_or(v);
                    pseudo_set(vm, ch, P_LP, wrapped);
                }
            }
        }
        // AOSP resolves LP direction in resolveLayoutParams during the
        // layout pass; ConstraintLayout$LayoutParams.resolveLayoutDirection
        // is what turns `endToEnd=parent` into the resolvedRightToRight
        // anchor the solver reads — without it start/end constraints
        // silently drop and the child lands at x=0.
        if let Some(lp @ V::Ref(_)) = pseudo_get(vm, ch, P_LP) {
            lp_resolve_dir_dex(vm, lp);
        }
        // AOSP ViewGroup.addViewInner notifies onViewAdded — dex parents
        // (ConstraintLayout) use it to arm mDirtyHierarchy and register
        // the child in mChildrenById; without it updateHierarchy skips
        // the child and the solver leaves its widget at 0,0,0,0.
        if notify {
            let _ = vm.call_virtual_rt(parent, "onViewAdded", &[V::Ref(ch)]);
        }
        // App content lands under the `android.R.id.content` frame —
        // whether via our Window.setContentView or appcompat's dex-side
        // ensureSubDecor — so remember it as the running activity's
        // content and the enclosing tree as its window's real decor.
        if view_id(vm, parent) == ANDROID_ID_CONTENT {
            // Register content under the decor's owning window target:
            // the top of the frame's tree carries P_WIN_OWNER (set by
            // decor_of) — an activity's decor tags its activity, a
            // Dialog's decor tags the dialog so its content lands under
            // the dialog handle instead of overwriting the activity's.
            // AppCompat's own (detached) decor has no tag — fall back to
            // the running activity, matching pre-tagging behavior.
            let mut top = parent;
            let mut hops = 0;
            while let Some(V::Ref(p)) = pseudo_get(vm, top, P_PARENT) {
                if p == 0 || hops > 64 {
                    break;
                }
                top = p;
                hops += 1;
            }
            let owner = match pseudo_get(vm, top, P_WIN_OWNER) {
                Some(V::Ref(a)) => a,
                _ => vm.cur_activity,
            };
            vm.content_frames.insert(owner, parent);
            vm.content_views.insert(owner, ch);
            vm.decor_roots.insert(owner, top);
        }
    }
}

/// `notify` fires `onViewRemoved` — real `removeView*` paths only;
/// the ChildHelper's transient `detachViewFromParent` never notifies
/// (the child is coming right back).
pub(crate) fn detach(vm: &mut Vm, parent: u32, child: V, notify: bool) {
    let Some(a) = children_arr(vm, parent) else {
        return;
    };
    let pos = match vm.heap.get(a) {
        Some(Obj::Array { data, .. }) => data.iter().position(|&c| value_eq(vm, c, child)),
        _ => None,
    };
    if let Some(i) = pos {
        if let Some(Obj::Array { data, .. }) = vm.heap.get_mut(a) {
            data.remove(i);
        }
        mark_layout_requested(vm, parent);
        if let V::Ref(ch) = child {
            pseudo_set(vm, ch, P_PARENT, V::Ref(0));
            // Mirror onViewAdded: dex parents unregister the child here
            // (ConstraintLayout drops it from mChildrenById; adapters
            // recycle through detach and stale registrations would keep
            // dead views in subsequent passes). A real remove also ends
            // the child's attach — `view_is_attached` must stop being
            // true or e.g. the GL pump keeps driving an orphaned
            // SurfaceView after setContentView replaces the tree.
            if notify {
                clear_attached(vm, ch, 0);
                let _ = vm.call_virtual_rt(parent, "onViewRemoved", &[V::Ref(ch)]);
            }
        }
    }
}

/// `dispatchAttachedToWindow` bookkeeping: set the attach mark and
/// deliver `onAttachedToWindow` on the view and every still-unattached
/// descendant (depth-bounded like `find_in_tree_at`). Needed when a
/// view was added before its parent attached — the parent's later
/// attach must reach the existing subtree, not just future children.
fn mark_attached(vm: &mut Vm, h: u32, depth: u32) {
    if h == 0 || depth > 128 {
        return;
    }
    if pint(vm, h, P_ATTACHED) != 1 && pint(vm, h, P_ATTACH_QUEUED) != 1 {
        pseudo_set(vm, h, P_ATTACH_QUEUED, V::I32(1));
        vm.pending_attach_callbacks.push_back(h);
    }
    for c in children(vm, h) {
        if let V::Ref(ch) = c {
            mark_attached(vm, ch, depth + 1);
        }
    }
}

/// Deliver queued window attachment at a drain boundary. The attach
/// flag becomes visible only here: Compose views created while a parent
/// composition is active must still read `isAttachedToWindow=false`,
/// defer their composition creation, and start it after that outer
/// composition has finished.
pub(crate) fn deliver_attach_callbacks(vm: &mut Vm) {
    for h in std::mem::take(&mut vm.pending_attach_callbacks) {
        // The batch is detached from the queue before delivery starts:
        // a parent callback can remove a later child, which clears the
        // child's queued marker but cannot remove it from this local
        // batch. Honor that cleared marker before attaching it.
        if pint(vm, h, P_ATTACH_QUEUED) != 1 {
            continue;
        }
        pseudo_set(vm, h, P_ATTACH_QUEUED, V::I32(0));
        if pint(vm, h, P_ATTACHED) == 1 {
            continue;
        }
        pseudo_set(vm, h, P_ATTACHED, V::I32(1));
        let r = vm.call_virtual(h, "onAttachedToWindow", &[]);
        if crate::dbg_flag("OVELO_VIEWDBG") {
            match r {
                Ok(_) => eprintln!("[viewdbg] onAttachedToWindow h{h} ok"),
                Err(e) => eprintln!("[viewdbg] onAttachedToWindow h{h} err {e}"),
            }
        }
        // ViewGroup.dispatchAttachedToWindow reaches the children
        // present when dispatch runs. A child added while an earlier
        // parent callback was executing therefore joins attachment even
        // though its container was still only attach-queued at addView
        // time. Already-queued or already-attached children are ignored
        // by mark_attached, so this preserves the original order.
        if pint(vm, h, P_ATTACHED) == 1 {
            for c in children(vm, h) {
                if let V::Ref(ch) = c {
                    mark_attached(vm, ch, 0);
                }
            }
        }
    }
}

/// First window attachment for a materialized view: ViewRootImpl
/// attaches the whole decor subtree, not just the node the renderer
/// happened to materialize. Climb to the highest recorded parent and
/// dispatch downward so later addView calls also propagate attachment.
pub(crate) fn window_attached(vm: &mut Vm, h: u32) {
    let mut root = h;
    let mut hops = 0;
    while let Some(V::Ref(p)) = pseudo_get(vm, root, P_PARENT) {
        if p == 0 || hops > 128 {
            break;
        }
        root = p;
        hops += 1;
    }
    mark_attached(vm, root, 0);
}

/// `dispatchDetachedFromWindow` bookkeeping: clear the attach mark on
/// the view and its subtree (depth-bounded like `find_in_tree_at`).
fn clear_attached(vm: &mut Vm, h: u32, depth: u32) {
    if h == 0 || depth > 128 {
        return;
    }
    pseudo_set(vm, h, P_ATTACHED, V::I32(0));
    pseudo_set(vm, h, P_ATTACH_QUEUED, V::I32(0));
    vm.pending_attach_callbacks.retain(|v| *v != h);
    for c in children(vm, h) {
        if let V::Ref(ch) = c {
            clear_attached(vm, ch, depth + 1);
        }
    }
}

/// Depth-first search for a view whose `android:id` equals `id`.
pub(crate) fn find_in_tree(vm: &Vm, root: u32, id: i32) -> Option<u32> {
    find_in_tree_at(vm, root, id, 0)
}

/// Depth-bounded DFS — `attach` refuses cycles, but a defensive bound
/// keeps a malformed tree from recursing forever.
fn find_in_tree_at(vm: &Vm, root: u32, id: i32, depth: u32) -> Option<u32> {
    if root == 0 || depth > 128 {
        return None;
    }
    if view_id(vm, root) == id {
        return Some(root);
    }
    for c in children(vm, root) {
        if let V::Ref(ch) = c {
            if let Some(f) = find_in_tree_at(vm, ch, id, depth + 1) {
                return Some(f);
            }
        }
    }
    None
}

fn is_compound_desc(vm: &Vm, desc: &str) -> bool {
    desc.contains("CompoundButton")
        || desc.contains("CheckBox")
        || desc.contains("Checkbox")
        || desc.contains("RadioButton")
        || desc.contains("Switch")
        || vm.assignable_desc(desc, "Landroid/widget/CompoundButton;")
}

fn is_radio_desc(vm: &Vm, desc: &str) -> bool {
    desc.contains("RadioButton") || vm.assignable_desc(desc, "Landroid/widget/RadioButton;")
}

fn notify_checked_listener(vm: &mut Vm, this: u32, arg: V) {
    // CompoundButton uses mBroadcasting to keep a listener that calls
    // setChecked() from recursively re-entering the same notification.
    if pint(vm, this, P_CHECK_BROADCAST) != 0 {
        return;
    }
    if let Some(V::Ref(listener)) = pseudo_get(vm, this, P_CHECK_LISTENER) {
        if listener != 0 {
            pseudo_set(vm, this, P_CHECK_BROADCAST, V::I32(1));
            let _ = vm.call_virtual(listener, "onCheckedChanged", &[V::Ref(this), arg]);
            pseudo_set(vm, this, P_CHECK_BROADCAST, V::I32(0));
        }
    }
}

fn radio_group_checked_id(vm: &Vm, group: u32) -> i32 {
    pseudo_get(vm, group, P_RADIO_CHECKED)
        .map(i32v)
        .unwrap_or(-1)
}

fn radio_child(vm: &Vm, group: u32, id: i32) -> u32 {
    for c in children(vm, group) {
        if let V::Ref(ch) = c {
            let child_cls = class_of(vm, V::Ref(ch));
            if ch != 0 && is_radio_desc(vm, &child_cls) && view_id(vm, ch) == id {
                return ch;
            }
        }
    }
    0
}

fn set_radio_child_checked(vm: &mut Vm, child: u32, checked: i32) {
    let old = pint(vm, child, P_CHECKED);
    if old == checked {
        return;
    }
    pseudo_set(vm, child, P_CHECKED, V::I32(checked));
    notify_checked_listener(vm, child, V::I32(checked));
}

fn notify_group_checked_listener(vm: &mut Vm, group: u32, id: i32) {
    // RadioGroup deliberately does not suppress nested check() calls:
    // mCheckedId is assigned before the callback, so a listener that
    // selects another item leaves that newer selection in force.
    if let Some(V::Ref(listener)) = pseudo_get(vm, group, P_CHECK_LISTENER) {
        if listener != 0 {
            let _ = vm.call_virtual(listener, "onCheckedChanged", &[V::Ref(group), V::I32(id)]);
        }
    }
}

fn radio_group_set_checked(vm: &mut Vm, group: u32, id: i32) {
    let old = radio_group_checked_id(vm, group);
    if id != -1 && id == old {
        return;
    }
    // AOSP ordering: clear the old button first, then select the new
    // button, then publish the group id and notify the group listener.
    if old != -1 {
        let old_child = radio_child(vm, group, old);
        if old_child != 0 {
            set_radio_child_checked(vm, old_child, 0);
        }
    }
    if id != -1 {
        let new_child = radio_child(vm, group, id);
        if new_child != 0 {
            set_radio_child_checked(vm, new_child, 1);
        }
    }
    pseudo_set(vm, group, P_RADIO_CHECKED, V::I32(id));
    notify_group_checked_listener(vm, group, id);
}

fn set_compound_checked(vm: &mut Vm, this: u32, checked: i32) {
    let checked = (checked != 0) as i32;
    let old = pint(vm, this, P_CHECKED);
    pseudo_set(vm, this, P_CHECKED, V::I32(checked));
    let parent = match pseudo_get(vm, this, P_PARENT) {
        Some(V::Ref(p)) => p,
        _ => 0,
    };
    let parent_is_group = parent != 0 && {
        let cls = class_of(vm, V::Ref(parent));
        cls == "Landroid/widget/RadioGroup;"
            || vm.assignable_desc(&cls, "Landroid/widget/RadioGroup;")
    };
    if parent_is_group && is_radio_desc(vm, &class_of(vm, V::Ref(this))) {
        let id = view_id(vm, this);
        if checked != 0 {
            radio_group_set_checked(vm, parent, id);
        } else if id != -1 && radio_group_checked_id(vm, parent) == id {
            radio_group_set_checked(vm, parent, -1);
        }
    }
    if old != checked {
        notify_checked_listener(vm, this, V::I32(checked));
    }
}

fn is_marked(vm: &Vm, h: u32) -> bool {
    h != 0 && pseudo_get(vm, h, P_IS_VIEW).is_some()
}

/// The view's recorded parent handle (0 when none).
pub(crate) fn view_parent(vm: &Vm, h: u32) -> u32 {
    match pseudo_get(vm, h, P_PARENT) {
        Some(V::Ref(p)) => p,
        _ => 0,
    }
}

fn stub_ctx(vm: &mut Vm) -> V {
    host(vm, "Landroid/content/Context;", HostState::Empty)
}

// ---------------------------------------------------------------------
// Activity window/content plumbing — used by context_call.

/// The Activity's `Window`, created once and cached.
pub(crate) fn window_of(vm: &mut Vm, activity: u32) -> Option<u32> {
    if activity == 0 {
        return None;
    }
    if let Some(V::Ref(w)) = pseudo_get(vm, activity, P_WINDOW) {
        return Some(w);
    }
    let w = vm.alloc_host(WINDOW, HostState::Empty);
    // The owning Activity doubles as the window's default callback.
    pseudo_set(vm, w, P_TARGET, V::Ref(activity));
    pseudo_set(vm, w, P_CONTEXT, V::Ref(activity));
    pseudo_set(vm, activity, P_WINDOW, V::Ref(w));
    Some(w)
}

/// Dialog backing: the dialog's `Window` (same mechanism as an
/// Activity's) with the dialog installed as `Window.Callback` — real
/// `Dialog.<init>` does `mWindow.setCallback(this)`, and androidx
/// dialogs throw "Window callback may not be null" without it.
pub(crate) fn dialog_window(vm: &mut Vm, dialog: u32) -> Option<u32> {
    let w = window_of(vm, dialog)?;
    if pseudo_get(vm, w, P_LISTENER).is_none() {
        pseudo_set(vm, w, P_LISTENER, V::Ref(dialog));
    }
    Some(w)
}

/// Remember the `Context` a `Dialog.<init>` was called with.
pub(crate) fn set_dialog_ctx(vm: &mut Vm, dialog: u32, ctx: V) {
    pseudo_set(vm, dialog, P_CONTEXT, ctx);
}

/// The stored dialog `Context` (0 when the ctor didn't supply one).
pub(crate) fn dialog_ctx(vm: &mut Vm, dialog: u32) -> V {
    pseudo_get(vm, dialog, P_CONTEXT).unwrap_or(V::Ref(0))
}

/// The Window's decor view (a ViewGroup), created once. The decor
/// always contains the framework content frame (`android.R.id.content`,
/// `0x01020002`) that app content and AppCompat's sub-decor rebind into —
/// `window.getDecorView().findViewById(android.R.id.content)` must hit it.
pub(crate) fn decor_of(vm: &mut Vm, window: u32) -> Option<u32> {
    if window == 0 {
        return None;
    }
    // Appcompat builds its own decor tree dex-side — prefer it over
    // the synthetic one so Window.findViewById sees the real content.
    // The window's owning activity (P_TARGET, set at window_of) picks
    // which activity's decor applies.
    let owner = match pseudo_get(vm, window, P_TARGET) {
        Some(V::Ref(a)) => a,
        _ => 0,
    };
    if let Some(&d) = vm.decor_roots.get(&owner) {
        pseudo_set(vm, window, P_DECOR, V::Ref(d));
        return Some(d);
    }
    if let Some(V::Ref(d)) = pseudo_get(vm, window, P_DECOR) {
        return Some(d);
    }
    let d = vm.alloc_host(VIEWGROUP, HostState::Empty);
    pseudo_set(vm, d, P_IS_VIEW, V::I32(1));
    pseudo_set(vm, d, P_IS_GROUP, V::I32(1));
    // Tag the decor with the window's target so `attach` registers
    // content under this window's owner — not blindly under the running
    // activity, which let a Dialog's content replace the activity's.
    pseudo_set(vm, d, P_WIN_OWNER, V::Ref(owner));
    let cf = vm.alloc_host(FRAMELAYOUT, HostState::Empty);
    pseudo_set(vm, cf, P_IS_VIEW, V::I32(1));
    pseudo_set(vm, cf, P_IS_GROUP, V::I32(1));
    pseudo_set(vm, cf, P_VIEW_ID, V::I32(ANDROID_ID_CONTENT));
    // The decor is not attached yet: ViewRootImpl only dispatches
    // attach after the activity lifecycle reaches the window. Keeping
    // it unattached during setContentView is load-bearing for Compose —
    // otherwise AbstractComposeView starts composition reentrantly inside
    // the activity's LifecycleRegistry forward pass.
    attach(vm, d, V::Ref(cf));
    // The decor fills its window — host-side frame now, since no dex
    // `layout()` will ever run over these objects (`getDecorView()
    // .getWidth()` must not read 0).
    let (ww, wh) = vm.window_size;
    frame_set(vm, d, 0, 0, ww, wh);
    frame_set(vm, cf, 0, 0, ww, wh);
    vm.content_frames.insert(owner, cf);
    pseudo_set(vm, window, P_DECOR, V::Ref(d));
    if crate::dbg_flag("OVELO_VIEWDBG") {
        eprintln!("[viewdbg] decor_of window=h{window} owner=h{owner} -> d=h{d} cf=h{cf}");
    }
    Some(d)
}

/// `android.R.id.content` — the framework content frame's res id.
pub(crate) const ANDROID_ID_CONTENT: i32 = 0x0102_0002;

/// The decor's framework content frame (the view appcompat's
/// `ensureSubDecor` rebinds app content into).
pub(crate) fn content_frame_of(vm: &mut Vm, window: u32) -> Option<u32> {
    let d = decor_of(vm, window)?;
    // Prefer the frame we already handed out: ensureSubDecor strips
    // `android.R.id.content` off the framework frame before moving its
    // own ContentFrameLayout in, and the id-search then misses.
    let owner = match pseudo_get(vm, window, P_TARGET) {
        Some(V::Ref(a)) => a,
        _ => vm.cur_activity,
    };
    if let Some(&cf) = vm.content_frames.get(&owner) {
        return Some(cf);
    }
    find_in_tree(vm, d, ANDROID_ID_CONTENT)
}

// ---------------------------------------------------------------------
// Menu model — `Toolbar.getMenu()`/`MenuInflater.inflate` populate a
// HostState::Map of itemId → MenuItem; `findItem` returns a stable
// handle per id (minted lazily, so callers never see a null receiver).

fn is_menuish(vm: &Vm, h: u32) -> bool {
    matches!(pseudo_get(vm, h, P_MENU), Some(V::I32(1)))
}

/// A MenuItem handle — includes SubMenu, which is both a menu and an
/// item in AOSP.
fn is_menuitem(vm: &Vm, h: u32) -> bool {
    pseudo_get(vm, h, P_MENU_VIS).is_some()
}

/// Whether the guest finished this Activity (`finish()` and family).
pub(crate) fn is_finishing(vm: &Vm, h: u32) -> bool {
    pseudo_get(vm, h, P_FINISHED) == Some(V::I32(1))
}

/// The Intent the Activity was launched with — set by the runner when
/// it instantiates the target of a guest `startActivity`.
pub(crate) fn set_activity_intent(vm: &mut Vm, h: u32, intent: V) {
    pseudo_set(vm, h, P_INTENT, intent);
}
pub(crate) fn activity_intent(vm: &Vm, h: u32) -> Option<V> {
    pseudo_get(vm, h, P_INTENT)
}

fn find_toolbar(vm: &Vm, h: u32, depth: usize) -> Option<u32> {
    if h == 0 || depth > 24 {
        return None;
    }
    let desc = class_of(vm, V::Ref(h));
    if vm.extends_named(&desc, "Toolbar") {
        return Some(h);
    }
    for c in children(vm, h) {
        if let V::Ref(ch) = c {
            if let Some(hit) = find_toolbar(vm, ch, depth + 1) {
                return Some(hit);
            }
        }
    }
    None
}

pub(crate) fn menu_of(vm: &mut Vm, view_h: u32) -> V {
    if let Some(v @ V::Ref(h)) = pseudo_get(vm, view_h, P_MENU) {
        if h != 0 {
            return v;
        }
    }
    // AppCompat toolbars present the options menu through the
    // MenuBuilder owned by their ActionMenuView. Reuse that exact object
    // when the content tree already exists; a separate MenuBuilder would
    // inflate successfully but never notify the toolbar presenter.
    let toolbar = match pseudo_get(vm, view_h, P_CONTENT) {
        Some(V::Ref(root)) => find_toolbar(vm, root, 0),
        _ => None,
    };
    let toolbar_menu = toolbar.and_then(|tb| {
        vm.call_virtual_direct(tb, "getMenu", &[])
            .ok()
            .flatten()
            .filter(|v| matches!(v, V::Ref(h) if *h != 0))
    });
    let m = toolbar_menu.unwrap_or_else(|| {
        vm.instantiate_with(
            "Landroidx/appcompat/view/menu/MenuBuilder;",
            &[V::Ref(view_h)],
        )
        .ok()
        .map(V::Ref)
        .unwrap_or_else(|| host(vm, "Landroid/view/Menu;", HostState::Map(vec![])))
    });
    if crate::dbg_flag("OVELO_TRACE_MENU") {
        eprintln!("[menu_of] view={view_h} menu={m:?}");
    }
    if let V::Ref(mh) = m {
        pseudo_set(vm, mh, P_MENU, V::I32(1));
    }
    pseudo_set(vm, view_h, P_MENU, m);
    m
}

fn menu_item_new(vm: &mut Vm, desc: &str, state: HostState, group: i32, id: i32) -> V {
    let it = host(vm, desc, state);
    if let V::Ref(ih) = it {
        pseudo_set(vm, ih, P_MENU_ID, V::I32(id));
        pseudo_set(vm, ih, P_MENU_GROUP, V::I32(group));
        pseudo_set(vm, ih, P_MENU_VIS, V::I32(1));
    }
    it
}

fn menu_push(vm: &mut Vm, menu_h: u32, id: i32, it: V) {
    let Some(h) = menu_store(vm, menu_h) else {
        return;
    };
    if let Some(Obj::Host {
        state: HostState::Map(items),
        ..
    }) = vm.heap.get_mut(h)
    {
        items.push((V::I32(id), it));
    }
}

/// AOSP `Menu.add` always appends — duplicate item ids are legal.
fn menu_item_add(vm: &mut Vm, menu_h: u32, group: i32, id: i32) -> V {
    let it = menu_item_new(vm, "Landroid/view/MenuItem;", HostState::Empty, group, id);
    menu_push(vm, menu_h, id, it);
    it
}

/// `addSubMenu` returns a SubMenu — itself a menu container, so nested
/// `add` calls register on it. The parent list holds the MenuItem that
/// opens it; the two are cross-linked so `SubMenu.getItem()` and
/// `MenuItem.getSubMenu()` resolve each other.
fn menu_sub_add(vm: &mut Vm, menu_h: u32, group: i32, id: i32) -> V {
    let item = menu_item_new(vm, "Landroid/view/MenuItem;", HostState::Empty, group, id);
    let sub = host(vm, "Landroid/view/SubMenu;", HostState::Map(vec![]));
    if let V::Ref(sh) = sub {
        pseudo_set(vm, sh, P_MENU, V::I32(1));
        pseudo_set(vm, sh, P_SUBMENU_ITEM, item);
        // Mutators (`setIcon` etc.) chain — a SubMenu is item-like for
        // `sub.setIcon(...).add(...)` to keep the same receiver.
        pseudo_set(vm, sh, P_MENU_VIS, V::I32(1));
    }
    if let V::Ref(ih) = item {
        pseudo_set(vm, ih, P_ITEM_SUBMENU, sub);
    }
    menu_push(vm, menu_h, id, item);
    sub
}

/// The object holding a menu's item list: a host `Menu` is its own
/// store; a dex `MenuBuilder`/`SubMenuBuilder` gets a backing
/// `HostState::Map` remembered under `P_MENU_STORE`.
fn menu_store_of(vm: &Vm, h: u32) -> Option<u32> {
    match vm.heap.get(h) {
        Some(Obj::Host {
            state: HostState::Map(_),
            ..
        }) => Some(h),
        Some(Obj::Instance { .. }) => match pseudo_get(vm, h, P_MENU_STORE) {
            Some(V::Ref(s)) if s != 0 => Some(s),
            _ => None,
        },
        _ => None,
    }
}

/// `menu_store_of`, minting the backing store for a dex menu on first
/// use and marking the instance menu-ish.
fn menu_store(vm: &mut Vm, h: u32) -> Option<u32> {
    if let s @ Some(_) = menu_store_of(vm, h) {
        return s;
    }
    if !matches!(vm.heap.get(h), Some(Obj::Instance { .. })) {
        return None;
    }
    let m = host(vm, "Landroid/view/Menu;", HostState::Map(vec![]));
    if let V::Ref(mh) = m {
        pseudo_set(vm, mh, P_MENU, V::I32(1));
        pseudo_set(vm, h, P_MENU, V::I32(1));
        pseudo_set(vm, h, P_MENU_STORE, V::Ref(mh));
        Some(mh)
    } else {
        None
    }
}

fn menu_items_paired(vm: &Vm, menu_h: u32) -> Vec<(i32, V)> {
    let Some(h) = menu_store_of(vm, menu_h) else {
        return vec![];
    };
    match vm.heap.get(h) {
        Some(Obj::Host {
            state: HostState::Map(items),
            ..
        }) => items.iter().map(|(k, v)| (i64v(*k) as i32, *v)).collect(),
        _ => vec![],
    }
}

fn menu_items(vm: &Vm, menu_h: u32) -> Vec<V> {
    menu_items_paired(vm, menu_h)
        .into_iter()
        .map(|(_, v)| v)
        .collect()
}

/// Depth-first search: `findItem` reaches into submenus too (items
/// carry `P_ITEM_SUBMENU`; a submenu may also sit in the list itself).
fn menu_search(vm: &Vm, menu_h: u32, id: i32, depth: u8) -> Option<V> {
    if depth > 8 {
        return None;
    }
    for (k, v) in menu_items_paired(vm, menu_h) {
        if k == id {
            return Some(v);
        }
        if let V::Ref(ih) = v {
            let sub = match pseudo_get(vm, ih, P_ITEM_SUBMENU) {
                Some(V::Ref(sh)) => Some(sh),
                _ if is_menuish(vm, ih) => Some(ih),
                _ => None,
            };
            if let Some(sh) = sub {
                if let hit @ Some(_) = menu_search(vm, sh, id, depth + 1) {
                    return hit;
                }
            }
        }
    }
    None
}

/// Find-or-create: `findItem` for a missing id mints a stub (AOSP's
/// null would NPE most guests on the next chained call).
fn menu_item(vm: &mut Vm, menu_h: u32, id: i32) -> V {
    if let Some(v) = menu_search(vm, menu_h, id, 0) {
        return v;
    }
    menu_item_add(vm, menu_h, 0, id)
}

pub(crate) fn menu_find(vm: &mut Vm, menu_h: u32, id_v: Option<V>) -> V {
    if crate::dbg_flag("OVELO_TRACE_MENU") {
        eprintln!(
            "[menu_find] menu={menu_h} items={} want={id_v:?}",
            menu_items(vm, menu_h).len()
        );
    }
    if menu_store(vm, menu_h).is_none() {
        return V::Ref(0);
    }
    menu_item(vm, menu_h, i64v(id_v.unwrap_or(V::I32(0))) as i32)
}

/// `setTitle(int resId)` stores a resource id; resolve it to the real
/// string so `getTitle()` hands the guest a CharSequence.
fn menu_title(vm: &mut Vm, v: V) -> V {
    if let V::I32(id) = v {
        if let Some(s) = vm.apk_res.as_ref().and_then(|r| r.string_of(id as u32)) {
            let s = s.to_string();
            return jstr(vm, &s);
        }
    }
    v
}

/// `MenuInflater.inflate(menuRes, Menu)` — parse the res/menu xml and
/// register each `<item android:id>` on the menu. On a dex menu
/// (`MenuBuilder`) items are added through the dex `add`/`addSubMenu`
/// methods so the menu's own list stays authoritative — `findItem`,
/// `getVisibleItems`, casts to `MenuItemImpl` all see real entries. A
/// `<menu>` nested inside an `<item>` inflates into its SubMenu.
pub(crate) fn menu_inflate(vm: &mut Vm, res_v: Option<V>, menu_h: u32) {
    let trace = crate::dbg_flag("OVELO_TRACE_MENU");
    if trace {
        eprintln!("[menu_inflate] res={res_v:?} menu_h={menu_h}");
    }
    let Some(V::I32(id)) = res_v else { return };
    let Some(doc) = vm.apk_res.as_ref().and_then(|r| r.document(id as u32)) else {
        if trace {
            eprintln!("[menu_inflate] no document for {id:#x}");
        }
        return;
    };
    if trace {
        eprintln!("[menu_inflate] doc {} elements", doc.len());
    }
    if !matches!(vm.heap.get(menu_h), Some(Obj::Instance { .. }))
        && menu_store(vm, menu_h).is_none()
    {
        return;
    }
    // doc[0] is the root `<menu>` (depth 0) — its `item`s are one
    // level deeper (and a nested `<menu>` inside an item nests two).
    let items_depth = doc.first().map(|e| e.depth + 1).unwrap_or(1);
    let mut i = usize::from(!doc.is_empty());
    menu_inflate_level(vm, &doc, &mut i, items_depth, menu_h);
}

/// One level of menu xml: `item` adds an entry; a nested `menu` opens
/// its SubMenu and recurses; any other element is skipped.
fn menu_inflate_level(vm: &mut Vm, doc: &[Element], i: &mut usize, depth: usize, menu_h: u32) {
    let trace = crate::dbg_flag("OVELO_TRACE_MENU");
    while *i < doc.len() {
        let el = &doc[*i];
        if el.depth < depth {
            break;
        }
        if el.name != "item" {
            *i += 1;
            continue;
        }
        let item_id = el.android("id").map(|v| attr_v(vm, v)).unwrap_or(V::I32(0));
        let order = el
            .android("orderInCategory")
            .map(|v| attr_v(vm, v))
            .unwrap_or(V::I32(0));
        let title = el
            .android("title")
            .map(|v| attr_v(vm, v))
            .unwrap_or(V::Ref(0));
        let visible = el
            .android("visible")
            .map(|v| attr_v(vm, v))
            .unwrap_or(V::I32(1));
        *i += 1;
        let sub = doc
            .get(*i)
            .map(|n| n.name == "menu" && n.depth > depth)
            .unwrap_or(false);
        if trace {
            eprintln!(
                "[menu_inflate] item id={:?} sub={sub}",
                i64v(item_id) as i32
            );
        }
        let it = menu_add_entry(
            vm,
            menu_h,
            0,
            i64v(item_id) as i32,
            i64v(order) as i32,
            title,
            sub,
        );
        if let V::Ref(ih) = it {
            if i64v(visible) == 0 {
                menu_item_visible(vm, ih, 0);
            }
            if let Some(action_class) = el
                .attrs
                .iter()
                .find(|a| a.name == "actionViewClass")
                .and_then(|a| match &a.value {
                    Value::Str(s) => Some(s.as_str()),
                    _ => None,
                })
            {
                let ctx = vm
                    .get_object_field(
                        menu_h,
                        "Landroidx/appcompat/view/menu/MenuBuilder;",
                        "mContext",
                    )
                    .filter(|v| !is_null(*v))
                    .unwrap_or(V::Ref(vm.cur_activity));
                let desc = format!("L{};", action_class.replace('.', "/"));
                if let Some(av) = try_construct_view(vm, &desc, ctx, V::Ref(0)) {
                    pseudo_set(vm, av, P_IS_VIEW, V::I32(1));
                    if desc_is_a(&desc, VIEWGROUP) || vm.assignable_desc(&desc, VIEWGROUP) {
                        pseudo_set(vm, av, P_IS_GROUP, V::I32(1));
                    }
                    pseudo_set(vm, av, P_CONTEXT, ctx);
                    if vm
                        .call_virtual_sig(
                            ih,
                            "setActionView",
                            &["Landroid/view/View;"],
                            &[V::Ref(av)],
                        )
                        .is_err()
                    {
                        vm.set_object_field(
                            ih,
                            "Landroidx/appcompat/view/menu/MenuItemImpl;",
                            "mActionView",
                            V::Ref(av),
                        );
                    }
                }
            }
        }
        if sub {
            let sub_h = match it {
                V::Ref(h) => h,
                _ => 0,
            };
            // `*i` is the nested `<menu>` element (`depth+1`) — skip it;
            // its `item`s sit at `depth+2`.
            *i += 1;
            menu_inflate_level(vm, doc, i, depth + 2, sub_h);
        }
    }
}

/// Add one entry to a menu: dex menus go through their own `add` /
/// `addSubMenu` (real `MenuItemImpl`/`SubMenuBuilder` objects, so casts
/// and unoverridden methods behave); host menus mint host items.
fn menu_add_entry(
    vm: &mut Vm,
    menu_h: u32,
    group: i32,
    id: i32,
    order: i32,
    title: V,
    sub: bool,
) -> V {
    if matches!(vm.heap.get(menu_h), Some(Obj::Instance { .. })) {
        let title_v = menu_title(vm, title);
        let m = if sub { "addSubMenu" } else { "add" };
        // Bypass the host compatibility surface: MenuBuilder.add is the
        // callback point through which ActionMenuPresenter materializes
        // toolbar items. Mirror the resulting item into the host backing
        // store only for compatibility queries (`findItem`, `size`).
        match vm.call_virtual_direct(
            menu_h,
            m,
            &[V::I32(group), V::I32(id), V::I32(order), title_v],
        ) {
            Ok(Some(it)) => {
                menu_push(vm, menu_h, id, it);
                return it;
            }
            r => {
                if crate::dbg_flag("OVELO_TRACE_MENU") {
                    eprintln!("[menu_add] dex {m} failed: {r:?}");
                }
            }
        }
    }
    if sub {
        menu_sub_add(vm, menu_h, group, id)
    } else {
        let it = menu_item_add(vm, menu_h, group, id);
        if let (V::Ref(ih), t) = (it, menu_title(vm, title)) {
            pseudo_set(vm, ih, P_TEXT, t);
        }
        it
    }
}

/// `setVisible` on a minted host item or a dex `MenuItemImpl`.
fn menu_item_visible(vm: &mut Vm, ih: u32, vis: i32) {
    match vm.heap.get(ih) {
        Some(Obj::Instance { .. }) => {
            let _ = vm.call_virtual(ih, "setVisible", &[V::I32(vis)]);
        }
        _ => pseudo_set(vm, ih, P_MENU_VIS, V::I32(vis)),
    }
}

/// `Activity.setContentView(int|View)`: inflate (if an id) and attach the
/// content under the window's decor, remembering it for findViewById.
pub(crate) fn set_content_view(vm: &mut Vm, activity: u32, content: V) {
    let cv = match content {
        V::I32(id) if id != 0 => inflate_res(vm, id as u32, V::Ref(activity)),
        v @ V::Ref(_) => v,
        _ => V::Ref(0),
    };
    if matches!(cv, V::Ref(0)) {
        return;
    }
    pseudo_set(vm, activity, P_CONTENT, cv);
    if let V::Ref(h) = cv {
        if h != 0 {
            vm.view_roots.retain(|r| *r != h);
            vm.view_roots.push(h);
        }
    }
    if let Some(w) = window_of(vm, activity) {
        if let Some(cf) = content_frame_of(vm, w) {
            // A second setContentView replaces the content view — the
            // old tree must leave the frame or it keeps drawing (and
            // e.g. keeps pumping an orphaned GLSurfaceView renderer).
            for c in children(vm, cf) {
                if matches!(c, V::Ref(h) if h != 0) {
                    detach(vm, cf, c, true);
                }
            }
            attach(vm, cf, cv);
        }
    }
}

// ---------------------------------------------------------------------
// androidx menu inflation — `MenuInflater.inflate(int, Menu)` is
// overridden so menu xml lands on the real Menu object whether it's a
// host Menu (options menu, toolbar) or a dex `MenuBuilder`; item ops
// themselves stay guest-side so the dex list remains authoritative.

/// Lay out overflow-menu rows compactly when the popup cannot afford
/// their natural heights. This is a host overlay approximation of the
/// scrollable ListPopupWindow: keeping every row visible preserves
/// reachability in the small desktop window, while the row content is
/// still the app's real adapter view hierarchy.
fn layout_popup_rows_compact(vm: &mut Vm, root: u32, width: i32, height: i32) {
    let rows = children(vm, root);
    if rows.is_empty() {
        return;
    }
    let row_h = (height / rows.len() as i32).max(32);
    for (pos, row) in rows.iter().copied().enumerate() {
        let V::Ref(row_hnd) = row else { continue };
        if row_hnd == 0 {
            continue;
        }
        let y = pos as i32 * row_h;
        let _ = vm.call_virtual(
            row_hnd,
            "layout",
            &[V::I32(0), V::I32(y), V::I32(width), V::I32(y + row_h)],
        );
        let pad = lp_px(
            vm,
            attr_find(vm, row_hnd, "paddingLeft").unwrap_or(V::I32(0)),
        )
        .unwrap_or(12)
        .min(width / 8);
        let mut x = pad.max(6);
        let kids = children(vm, row_hnd);
        for child in kids.iter().copied() {
            let V::Ref(ch) = child else { continue };
            if ch == 0 {
                continue;
            }
            let mw = pint(vm, ch, P_MEAS_W) & 0x00ff_ffff;
            let mh = pint(vm, ch, P_MEAS_H) & 0x00ff_ffff;
            let desc = class_of(vm, V::Ref(ch));
            let is_image = desc.contains("ImageView") || pseudo_get(vm, ch, P_IMAGE).is_some();
            let (cw, avail_w) = if is_image {
                let side = (row_h - 8).max(20).min(mw.max(1));
                (side, side)
            } else {
                (
                    (width - x - pad.max(6)).max(24),
                    (width - x - pad.max(6)).max(24),
                )
            };
            let ch_h = if is_image {
                cw
            } else {
                mh.max((row_h / 2).min(28)).min(row_h)
            };
            let cy = (row_h - ch_h).max(0) / 2;
            let use_w = if is_image { avail_w } else { cw };
            let _ = vm.call_virtual(
                ch,
                "layout",
                &[V::I32(x), V::I32(cy), V::I32(x + use_w), V::I32(cy + ch_h)],
            );
            x += use_w + pad.max(6);
        }
    }
}

fn lpw_field(vm: &Vm, popup: u32, logical: &str) -> V {
    // Release builds minify the bundled appcompat widget (`i`, `v`,
    // `w`, …) while debug/unminified apps keep the source names. Accept
    // both spellings of the same ListPopupWindow state.
    let names: &[&str] = match logical {
        "adapter" => &["i", "mAdapter"],
        "width" => &["l", "mDropDownWidth"],
        "context" => &["h", "mContext"],
        "anchor" => &["v", "mDropDownAnchorView"],
        "xoff" => &["m", "mDropDownHorizontalOffset"],
        "yoff" => &["n", "mDropDownVerticalOffset"],
        "listener" => &["w", "mItemClickListener"],
        _ => &[logical],
    };
    for name in names {
        if let Some(v) = field_get(
            vm,
            popup,
            "Landroidx/appcompat/widget/ListPopupWindow;",
            name,
        ) {
            return v;
        }
    }
    V::Ref(0)
}

/// Materialize an appcompat ListPopupWindow as a simple vertical popup
/// built from its real adapter rows. The platform PopupWindow display
/// path is host-window functionality the dex implementation cannot
/// provide; adapter `getView` still supplies the app's row layout,
/// icons and labels.
fn list_popup_show(vm: &mut Vm, popup: u32) {
    if crate::dbg_flag("OVELO_TOUCHDBG") {
        eprintln!("[touchdbg] listpopup enter h{popup}");
    }
    list_popup_dismiss(vm, popup);
    let adapter = match lpw_field(vm, popup, "adapter") {
        V::Ref(h) if h != 0 => h,
        _ => return,
    };
    let mut count = vm
        .call_virtual(adapter, "getCount", &[])
        .ok()
        .flatten()
        .map(i32v)
        .unwrap_or(0);
    if count == 0 {
        // Framework ArrayAdapter is a host skeleton, so its inherited
        // getCount reports zero even though the dex subclass stores the
        // real backing list in an instance field. Use the densest list
        // field as the row source count; getView itself is dex code.
        if let Some(Obj::Instance { fields, .. }) = vm.heap.get(adapter) {
            count = fields
                .values()
                .filter_map(|v| match v {
                    V::Ref(h) if *h != 0 => match vm.heap.get(*h) {
                        Some(Obj::Host {
                            state: HostState::List(l),
                            ..
                        }) => Some(l.len() as i32),
                        Some(Obj::Array { data, .. }) => Some(data.len() as i32),
                        _ => None,
                    },
                    _ => None,
                })
                .max()
                .unwrap_or(0);
        }
    }
    let count = count.clamp(0, 32);
    if crate::dbg_flag("OVELO_TOUCHDBG") {
        eprintln!("[touchdbg] listpopup count={count}");
    }
    if count == 0 {
        return;
    }
    let width = match lpw_field(vm, popup, "width") {
        V::I32(w) if w > 0 => w,
        _ => 384,
    };
    let root = vm.alloc_host("Landroid/widget/LinearLayout;", HostState::Empty);
    pseudo_set(vm, root, P_IS_VIEW, V::I32(1));
    pseudo_set(vm, root, P_IS_GROUP, V::I32(1));
    pseudo_set(vm, root, P_ORIENTATION, V::I32(1));
    pseudo_set(vm, root, P_BGCOLOR, V::I32(0xff2b2b2bu32 as i32));
    pseudo_set(vm, root, P_POPUP_ROOT, V::Ref(popup));
    if let V::Ref(ctx) = lpw_field(vm, popup, "context") {
        pseudo_set(vm, root, P_CONTEXT, V::Ref(ctx));
    }

    let mut rows = 0;
    for pos in 0..count {
        let row = vm
            .call_virtual(adapter, "getView", &[V::I32(pos), V::Ref(0), V::Ref(root)])
            .ok()
            .flatten();
        if crate::dbg_flag("OVELO_TOUCHDBG") {
            eprintln!("[touchdbg] listpopup row{pos} view={row:?}");
        }
        let Some(V::Ref(row_h)) = row else { continue };
        if row_h == 0 {
            continue;
        }
        // inflate_res remembers detached adapter rows as search roots;
        // once adopted by the popup they must not render/touch as
        // independent roots as well.
        vm.view_roots.retain(|r| *r != row_h);
        attach_at(vm, root, V::Ref(row_h), None, false, None);
        let click = vm.alloc_host("Lovelo/PopupRowClickListener;", HostState::Empty);
        pseudo_set(vm, click, P_IS_VIEW, V::I32(1));
        pseudo_set(vm, click, P_POPUP_OWNER, V::Ref(popup));
        pseudo_set(vm, click, P_POPUP_POS, V::I32(pos));
        pseudo_set(vm, row_h, P_LISTENER, V::Ref(click));
        rows += 1;
    }
    if rows == 0 {
        return;
    }

    if crate::dbg_flag("OVELO_TOUCHDBG") {
        eprintln!("[touchdbg] listpopup measuring rows={rows}");
    }
    let spec_w = 0x4000_0000i32 | width;
    let _ = vm.call_virtual(root, "measure", &[V::I32(spec_w), V::I32(0)]);
    let measured_height = (pint(vm, root, P_MEAS_H) & 0x00ff_ffff).max(rows * 48);

    let anchor = match lpw_field(vm, popup, "anchor") {
        V::Ref(h) if h != 0 => h,
        _ => 0,
    };
    let (ww, wh) = vm.window_size;
    let ar = if anchor != 0 {
        pint(vm, anchor, P_RIGHT)
    } else {
        ww
    };
    let at = if anchor != 0 {
        pint(vm, anchor, P_TOP)
    } else {
        wh
    };
    let xoff = match lpw_field(vm, popup, "xoff") {
        V::I32(v) => v,
        _ => 0,
    };
    let yoff = match lpw_field(vm, popup, "yoff") {
        V::I32(v) => v,
        _ => 0,
    };
    let max_h = if at > 0 { at + yoff.max(0) } else { wh }.max(96);
    let height = measured_height.min(max_h).min(wh);
    let x = (ar - width + xoff).clamp(0, (ww - width).max(0));
    let y = (at - height + yoff).clamp(0, (wh - height).max(0));
    let compressed = measured_height > height;
    let _ = vm.call_virtual(
        root,
        "layout",
        &[V::I32(0), V::I32(0), V::I32(width), V::I32(height)],
    );
    pseudo_set(vm, root, P_POPUP_COMPACT, V::I32(compressed as i32));
    if compressed {
        layout_popup_rows_compact(vm, root, width, height);
    }

    let parent = activity_content(vm, vm.cur_activity).unwrap_or(0);
    if parent == 0 {
        return;
    }
    // Frames are parent-relative once attached. The activity content
    // normally starts at the window origin, but subtract its origin so
    // action-bar offsets do not shift the popup twice.
    let pl = pint(vm, parent, P_LEFT);
    let pt = pint(vm, parent, P_TOP);
    frame_set(vm, root, x - pl, y - pt, x - pl + width, y - pt + height);
    attach_at(vm, parent, V::Ref(root), None, false, None);
    vm.view_roots.retain(|r| *r != root);
    vm.view_roots.push(root);
    pseudo_set(vm, popup, P_POPUP_ROOT, V::Ref(root));
    if crate::dbg_flag("OVELO_TOUCHDBG") {
        eprintln!("[touchdbg] listpopup show root h{root} rows={rows} at {x},{y} {width}x{height}");
    }
}

fn list_popup_dismiss(vm: &mut Vm, popup: u32) {
    let Some(V::Ref(root)) = pseudo_get(vm, popup, P_POPUP_ROOT) else {
        return;
    };
    if root == 0 {
        return;
    }
    if let Some(V::Ref(parent)) = pseudo_get(vm, root, P_PARENT) {
        detach(vm, parent, V::Ref(root), false);
    }
    vm.view_roots.retain(|r| *r != root);
    pseudo_set(vm, popup, P_POPUP_ROOT, V::Ref(0));
    mark_layout_requested(vm, root);
}

fn ov_popup_show(vm: &mut Vm, a: &[V]) -> ovelo_error::Result<Option<V>> {
    if let Some(V::Ref(popup)) = a.first().copied() {
        list_popup_show(vm, popup);
    }
    Ok(None)
}

fn ov_popup_dismiss(vm: &mut Vm, a: &[V]) -> ovelo_error::Result<Option<V>> {
    if let Some(V::Ref(popup)) = a.first().copied() {
        list_popup_dismiss(vm, popup);
    }
    Ok(None)
}

#[allow(clippy::too_many_arguments)]
fn dialog_scan_buttons(
    vm: &Vm,
    h: u32,
    depth: usize,
    seen: &mut std::collections::HashSet<u32>,
    pos_listener: &mut u32,
    neg_listener: &mut u32,
    pos_label: &mut Option<String>,
    neg_label: &mut Option<String>,
) {
    if h == 0 || depth > 5 || !seen.insert(h) {
        return;
    }
    let desc = class_of(vm, V::Ref(h));
    if depth > 0
        && (pseudo_get(vm, h, P_IS_VIEW).is_some()
            || desc == WINDOW
            || vm.assignable_desc(&desc, "Landroid/content/Context;"))
    {
        return;
    }
    if desc == "Landroid/os/Message;" {
        let what = pseudo_get(vm, h, P_MSG_WHAT).map(i32v).unwrap_or(0);
        if let Some(V::Ref(listener)) = pseudo_get(vm, h, P_MSG_OBJ) {
            if listener != 0 {
                if what == -1 && *pos_listener == 0 {
                    *pos_listener = listener;
                } else if what == -2 && *neg_listener == 0 {
                    *neg_listener = listener;
                }
            }
        }
    }
    let vals: Vec<V> = match vm.heap.get(h) {
        Some(Obj::Instance { fields, .. }) | Some(Obj::Host { fields, .. }) => {
            fields.values().copied().collect()
        }
        _ => Vec::new(),
    };
    for v in vals {
        match v {
            V::Ref(ch) if ch != 0 => {
                if let Some(Obj::Str(text)) = vm.heap.get(ch) {
                    if text.eq_ignore_ascii_case("ok") {
                        pos_label.get_or_insert(text.clone());
                    } else if text.eq_ignore_ascii_case("cancel") {
                        neg_label.get_or_insert(text.clone());
                    }
                }
                dialog_scan_buttons(
                    vm,
                    ch,
                    depth + 1,
                    seen,
                    pos_listener,
                    neg_listener,
                    pos_label,
                    neg_label,
                );
            }
            _ => {}
        }
    }
}

fn dialog_button(vm: &mut Vm, dialog: u32, which: i32, label: &str) -> u32 {
    let b = vm.alloc_host("Landroid/widget/Button;", HostState::Empty);
    pseudo_set(vm, b, P_IS_VIEW, V::I32(1));
    let text = jstr(vm, label);
    pseudo_set(vm, b, P_TEXT, text);
    pseudo_set(vm, b, P_TCOLOR, V::I32(0xff8ab4f8u32 as i32));
    pseudo_set(vm, b, P_TSIZE, V::F32(15.0));
    pseudo_set(vm, b, P_DIALOG_BUTTON_DIALOG, V::Ref(dialog));
    pseudo_set(vm, b, P_DIALOG_BUTTON_WHICH, V::I32(which));
    b
}

fn layout_dialog_buttons(vm: &mut Vm, root: u32, width: i32, height: i32) {
    let buttons: Vec<u32> = children(vm, root)
        .into_iter()
        .filter_map(|v| match v {
            V::Ref(h) if h != 0 && pseudo_get(vm, h, P_DIALOG_BUTTON_WHICH).is_some() => Some(h),
            _ => None,
        })
        .collect();
    if buttons.is_empty() {
        return;
    }
    if let Some(V::Ref(content)) = pseudo_get(vm, root, P_DIALOG_CONTENT) {
        if content != 0 && content != root {
            let content_h = (height - 52).max(0);
            let spec = 0x4000_0000i32 | content_h;
            let spec_w = 0x4000_0000i32 | width;
            let _ = vm.call_virtual(content, "measure", &[V::I32(spec_w), V::I32(spec)]);
            let _ = vm.call_virtual(
                content,
                "layout",
                &[V::I32(0), V::I32(0), V::I32(width), V::I32(content_h)],
            );
        }
    }
    let bw = 104;
    let bh = 42;
    let bottom = height - 8;
    let mut right = width - 12;
    for b in buttons.iter().rev().copied() {
        let l = (right - bw).max(0);
        let t = (bottom - bh).max(0);
        let _ = vm.call_virtual(
            b,
            "layout",
            &[V::I32(l), V::I32(t), V::I32(l + bw), V::I32(t + bh)],
        );
        right = l - 8;
    }
}

fn dialog_view_scan(
    vm: &Vm,
    h: u32,
    depth: usize,
    seen: &mut std::collections::HashSet<u32>,
) -> Option<(u32, i32)> {
    if h == 0 || depth > 5 || !seen.insert(h) {
        return None;
    }
    let desc = class_of(vm, V::Ref(h));
    // Contexts/windows lead back to the activity content tree; a dialog
    // scan must not adopt the main UI as its own content.
    if depth > 0
        && (desc == WINDOW
            || desc.ends_with("Context;")
            || desc.ends_with("Activity;")
            || vm.assignable_desc(&desc, "Landroid/content/Context;"))
    {
        return None;
    }
    let mut best = if pseudo_get(vm, h, P_IS_VIEW).is_some() {
        let mut score = 1;
        if pseudo_get(vm, h, P_IS_GROUP).is_some() || desc.contains("Layout") {
            score += 4;
        }
        if pseudo_get(vm, h, P_PARENT).is_none() {
            score += 2;
        }
        Some((h, score))
    } else {
        None
    };
    let vals: Vec<V> = match vm.heap.get(h) {
        Some(Obj::Instance { fields, .. }) | Some(Obj::Host { fields, .. }) => {
            fields.values().copied().collect()
        }
        _ => Vec::new(),
    };
    for v in vals {
        if let V::Ref(ch) = v {
            if let Some(cand) = dialog_view_scan(vm, ch, depth + 1, seen) {
                if best.is_none_or(|b| cand.1 > b.1) {
                    best = Some(cand);
                }
            }
        }
    }
    best
}

pub(crate) fn set_dialog_cancelable(vm: &mut Vm, dialog: u32, value: V) {
    pseudo_set(vm, dialog, P_DIALOG_CANCELABLE, V::I32(i32v(value)));
}

/// Show a framework/appcompat dialog as a host overlay. Dex AlertDialog
/// keeps its custom view inside the obfuscated AlertController fields
/// rather than calling Dialog.setContentView, so scan the dialog object
/// for the detached view it built and attach that as a centered layer.
pub(crate) fn dialog_show(vm: &mut Vm, dialog: u32) {
    dialog_dismiss(vm, dialog);
    let explicit = match pseudo_get(vm, dialog, P_CONTENT) {
        Some(V::Ref(h)) if h != 0 => Some(h),
        _ => None,
    };
    let mut seen = std::collections::HashSet::new();
    let Some(content) =
        explicit.or_else(|| dialog_view_scan(vm, dialog, 0, &mut seen).map(|x| x.0))
    else {
        return;
    };
    // AlertDialog normally wraps custom content in the appcompat decor:
    // an opaque panel, title area, and button bar. The runtime does not
    // execute that decor construction, so give the content its opaque
    // panel explicitly; otherwise transparent list rows draw directly
    // over the activity behind them.
    let root = match pseudo_get(vm, dialog, P_DIALOG_PANEL) {
        Some(V::Ref(p)) if p != 0 => p,
        _ => {
            let p = vm.alloc_host("Landroid/widget/LinearLayout;", HostState::Empty);
            pseudo_set(vm, p, P_IS_VIEW, V::I32(1));
            pseudo_set(vm, p, P_IS_GROUP, V::I32(1));
            pseudo_set(vm, p, P_ORIENTATION, V::I32(1));
            pseudo_set(vm, p, P_BGCOLOR, V::I32(0xff202020u32 as i32));
            pseudo_set(vm, dialog, P_DIALOG_PANEL, V::Ref(p));
            p
        }
    };
    if content != root {
        // `setContentView` can replace the content between shows while
        // the panel object is reused. Drop the prior child or both old
        // and new content remain stacked in the same panel.
        for child in children(vm, root) {
            if child != V::Ref(content) {
                detach(vm, root, child, true);
            }
        }
        if let Some(V::Ref(old_parent)) = pseudo_get(vm, content, P_PARENT) {
            if old_parent != 0 && old_parent != root {
                detach(vm, old_parent, V::Ref(content), false);
            }
        }
        if pseudo_get(vm, content, P_PARENT) != Some(V::Ref(root)) {
            vm.view_roots.retain(|r| *r != content);
            attach_at(vm, root, V::Ref(content), None, false, None);
        }
    }
    let mut pos_listener = 0;
    let mut neg_listener = 0;
    let mut pos_label = None;
    let mut neg_label = None;
    dialog_scan_buttons(
        vm,
        dialog,
        0,
        &mut std::collections::HashSet::new(),
        &mut pos_listener,
        &mut neg_listener,
        &mut pos_label,
        &mut neg_label,
    );
    pseudo_set(vm, dialog, P_DIALOG_POS_LISTENER, V::Ref(pos_listener));
    pseudo_set(vm, dialog, P_DIALOG_NEG_LISTENER, V::Ref(neg_listener));
    for child in children(vm, root) {
        if let V::Ref(ch) = child {
            if pseudo_get(vm, ch, P_DIALOG_BUTTON_WHICH).is_some() {
                detach(vm, root, child, true);
            }
        }
    }
    // The scan knows the canonical strings but not their declaring fields
    // after R8 minification. When only one action exists, prefer that sole
    // label even if it is spelled "Cancel"; otherwise reserve that spelling
    // for the negative action.
    let sole_cancel_positive =
        pos_listener != 0 && neg_listener == 0 && pos_label.is_none() && neg_label.is_some();
    let positive_label = if sole_cancel_positive {
        neg_label.clone()
    } else {
        pos_label.clone()
    };
    let negative_label = if sole_cancel_positive {
        None
    } else {
        neg_label.clone()
    };
    let mut any_buttons = false;
    if positive_label.is_some() || pos_listener != 0 {
        let label = positive_label.as_deref().unwrap_or("OK");
        let button = dialog_button(vm, dialog, -1, label);
        attach_at(vm, root, V::Ref(button), None, false, None);
        any_buttons = true;
    }
    if negative_label.is_some() || neg_listener != 0 {
        let label = negative_label.as_deref().unwrap_or("Cancel");
        let button = dialog_button(vm, dialog, -2, label);
        attach_at(vm, root, V::Ref(button), None, false, None);
        any_buttons = true;
    }
    pseudo_set(vm, root, P_DIALOG_HAS_BUTTONS, V::I32(any_buttons as i32));
    pseudo_set(vm, root, P_DIALOG_CONTENT, V::Ref(content));
    let parent = activity_content(vm, vm.cur_activity).unwrap_or(0);
    if parent == 0 {
        return;
    }
    let (ww, wh) = vm.window_size;
    let width = (ww * 4 / 5).clamp(360, 680);
    let height = (wh * 3 / 5).clamp(260, 460);
    let spec_w = 0x4000_0000i32 | width;
    let spec_h = 0x4000_0000i32 | height;
    let _ = vm.call_virtual(root, "measure", &[V::I32(spec_w), V::I32(spec_h)]);
    let _ = vm.call_virtual(
        root,
        "layout",
        &[V::I32(0), V::I32(0), V::I32(width), V::I32(height)],
    );
    if content != root {
        // Measure wrap-style custom content at most to the panel. Only a
        // dimension that remains empty is expanded: fixed-size content
        // must keep its bounds or its touch target would cover the panel.
        let content_limit = if any_buttons {
            (height - 52).max(1)
        } else {
            height
        };
        let aw = 0x8000_0000u32 as i32 | width;
        let ah = 0x8000_0000u32 as i32 | content_limit;
        let _ = vm.call_virtual(content, "measure", &[V::I32(aw), V::I32(ah)]);
        let cw = pint(vm, content, P_MEAS_W).clamp(0, width);
        let ch = pint(vm, content, P_MEAS_H).clamp(0, content_limit);
        let cw = if cw == 0 { width } else { cw };
        let ch = if ch == 0 { content_limit } else { ch };
        let _ = vm.call_virtual(
            content,
            "layout",
            &[V::I32(0), V::I32(0), V::I32(cw), V::I32(ch)],
        );
    }
    let x = ((ww - width) / 2).max(0);
    let y = ((wh - height) / 2).max(0);
    let pl = pint(vm, parent, P_LEFT);
    let pt = pint(vm, parent, P_TOP);
    frame_set(vm, root, x - pl, y - pt, x - pl + width, y - pt + height);
    if let Some(V::Ref(old_parent)) = pseudo_get(vm, root, P_PARENT) {
        if old_parent != 0 {
            detach(vm, old_parent, V::Ref(root), false);
        }
    }
    attach_at(vm, parent, V::Ref(root), None, false, None);
    vm.view_roots.retain(|r| *r != root);
    vm.view_roots.push(root);
    pseudo_set(vm, root, P_POPUP_ROOT, V::Ref(dialog));
    pseudo_set(vm, dialog, P_POPUP_ROOT, V::Ref(root));
    mark_layout_requested(vm, root);
}

/// Remove the overlay attached by `dialog_show`.
pub(crate) fn dialog_dismiss(vm: &mut Vm, dialog: u32) {
    let Some(V::Ref(root)) = pseudo_get(vm, dialog, P_POPUP_ROOT) else {
        return;
    };
    if root == 0 {
        return;
    }
    if let Some(V::Ref(parent)) = pseudo_get(vm, root, P_PARENT) {
        detach(vm, parent, V::Ref(root), false);
    }
    vm.view_roots.retain(|r| *r != root);
    pseudo_set(vm, dialog, P_POPUP_ROOT, V::Ref(0));
    mark_layout_requested(vm, root);
}

fn ov_menu_inflate(vm: &mut Vm, a: &[V]) -> ovelo_error::Result<Option<V>> {
    if let Some(V::Ref(mh)) = a.get(2).copied() {
        menu_inflate(vm, a.get(1).copied(), mh);
    }
    Ok(None)
}

/// Register `MenuInflater.inflate` overrides — dex menus receive their
/// entries through `MenuBuilder.add`, so `findItem`, `getVisibleItems`
/// and `MenuItemImpl` casts all work against the dex list.
pub(crate) fn register_menu_overrides(vm: &mut Vm) {
    let popup_show: HostCall = std::rc::Rc::new(std::cell::RefCell::new(ov_popup_show));
    let popup_dismiss: HostCall = std::rc::Rc::new(std::cell::RefCell::new(ov_popup_dismiss));
    let show_ok = vm.register_host(
        "Landroidx/appcompat/widget/ListPopupWindow;",
        "show",
        "()V",
        popup_show,
    );
    if crate::dbg_flag("OVELO_TOUCHDBG") {
        eprintln!("[touchdbg] listpopup register show={show_ok:?}");
    }
    let _ = vm.register_host(
        "Landroidx/appcompat/widget/ListPopupWindow;",
        "dismiss",
        "()V",
        popup_dismiss,
    );
    use std::cell::RefCell;
    use std::rc::Rc;
    let mk = |f: fn(&mut Vm, &[V]) -> ovelo_error::Result<Option<V>>| -> HostCall {
        Rc::new(RefCell::new(f))
    };
    // AppCompat's dex ResourceManagerInternal can construct a
    // VectorDrawableCompat shell whose dex draw implementation the host
    // ImageView cannot execute. Resource lookup itself is host-backed, so
    // answer the compatibility entry point with the rasterized drawable.
    let _ = vm.register_host(
        "Landroidx/appcompat/widget/ResourceManagerInternal;",
        "getDrawable",
        "(Landroid/content/Context;I)Landroid/graphics/drawable/Drawable;",
        Rc::new(RefCell::new(|vm: &mut Vm, a: &[V]| {
            let res = a.get(2).copied().map(i64v).unwrap_or(0);
            Ok(Some(if res > 0 {
                super::vector::drawable_for_res(vm, res as u32)
            } else {
                V::Ref(0)
            }))
        })) as HostCall,
    );
    // Material's NavigationBarItemView wraps menu icons in a
    // StateListDrawable copy chain implemented in dex. That copy loses
    // host pseudo bitmap state, so bind the original rasterized icon to
    // the item ImageView directly; checked-state recoloring is secondary
    // to rendering the icon at all.
    let _ = vm.register_host(
        "Lcom/google/android/material/navigation/NavigationBarItemView;",
        "setIcon",
        "(Landroid/graphics/drawable/Drawable;)V",
        Rc::new(RefCell::new(|vm: &mut Vm, a: &[V]| {
            let this = match a.first().copied() {
                Some(V::Ref(h)) => h,
                _ => 0,
            };
            let icon = a.get(1).copied().unwrap_or(V::Ref(0));
            if let Some(V::Ref(iv)) = vm.get_object_field(
                this,
                "Lcom/google/android/material/navigation/NavigationBarItemView;",
                "A",
            ) {
                pseudo_set(vm, iv, P_IMAGE, icon);
            }
            Ok(None)
        })) as HostCall,
    );
    for class in [
        "Landroid/app/Activity;",
        "Landroidx/fragment/app/FragmentActivity;",
        "Landroidx/appcompat/app/AppCompatActivity;",
    ] {
        let _ = vm.register_host(
            class,
            "setTheme",
            "(I)V",
            Rc::new(RefCell::new(|vm: &mut Vm, a: &[V]| {
                if let Some(v) = a.get(1).copied() {
                    vm.theme_id = i32v(v) as u32;
                }
                Ok(None)
            })) as HostCall,
        );
    }
    for class in [
        "Landroid/view/MenuInflater;",
        "Landroidx/appcompat/view/SupportMenuInflater;",
    ] {
        let ok = vm.register_host(
            class,
            "inflate",
            "(ILandroid/view/Menu;)V",
            mk(ov_menu_inflate),
        );
        if crate::dbg_flag("OVELO_TRACE_MENU") {
            eprintln!("[menu_inflate] register_host {class} -> {ok:?}");
        }
    }
}

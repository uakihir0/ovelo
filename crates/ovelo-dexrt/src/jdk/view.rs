//! `android.view` builtins: an inflated view tree held on pseudo fields.
//!
//! `LayoutInflater.inflate` parses the real layout XML from the installed
//! APK (`res.rs`) into a tree of host objects — each carries its
//! `android:id`, children, parent and tag as pseudo fields — so
//! `findViewById`, `getChildAt`/`getChildCount`, and ViewBinding's
//! `findChildViewById` walk real structure instead of nulls.

use super::*;
use crate::HostCall;
use ovelo_apk::manifest::{Attribute, Element, Value};

const VIEW: &str = "Landroid/view/View;";
const VIEWGROUP: &str = "Landroid/view/ViewGroup;";
const FRAMELAYOUT: &str = "Landroid/widget/FrameLayout;";
const WINDOW: &str = "Landroid/view/Window;";
const INFLATER: &str = "Landroid/view/LayoutInflater;";

// Pseudo keys for view-tree state (see mod.rs for the other P_* slots).
const P_VIEW_ID: u32 = u32::MAX - 7;
const P_CHILDREN: u32 = u32::MAX - 8;
const P_PARENT: u32 = u32::MAX - 9;
const P_TAG: u32 = u32::MAX - 10;
const P_TEXT: u32 = u32::MAX - 11;
const P_CONTENT: u32 = u32::MAX - 12;
const P_WINDOW: u32 = u32::MAX - 13;
// Marker keys are pub(crate): `is_instance` consults them so inflated
// views pass `instanceof View`/`ViewGroup` checks.
pub(crate) const P_IS_VIEW: u32 = u32::MAX - 14;
pub(crate) const P_IS_GROUP: u32 = u32::MAX - 15;
const P_DECOR: u32 = u32::MAX - 16;
const P_LISTENER: u32 = u32::MAX - 17;
const P_CONTEXT: u32 = u32::MAX - 18;
const P_VIS: u32 = u32::MAX - 19;
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
const P_TA_STYLE: u32 = 0xFFFD_FFFD;
/// Pseudo key: `defStyleAttr` res id on a `TypedArray`.
const P_TA_DEF_ATTR: u32 = 0xFFFD_FFFC;
/// Pseudo key: `defStyleRes` res id on a `TypedArray`.
const P_TA_DEF_RES: u32 = 0xFFFD_FFFB;
/// Pseudo key: the `AttributeSet` a view was inflated with (for
/// `getLayoutParams` re-parse).
const P_ATTRS: u32 = 0xFFFD_FFFA;
/// Pseudo key: `LayoutParams` object set by `setLayoutParams` or
/// constructed lazily on first `getLayoutParams`.
const P_LP: u32 = 0xFFFD_FFF9;

/// Re-exported for `context_call` — Activity.findViewById searches the
/// content tree set by setContentView.
pub(crate) fn content_root(vm: &mut Vm, h: u32) -> Option<u32> {
    match pseudo_get(vm, h, P_CONTENT) {
        Some(V::Ref(r)) if r != 0 => Some(r),
        _ => window_of(vm, h).and_then(|w| decor_of(vm, w)),
    }
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
    ) || desc_is_a(class, VIEW)
        || class.ends_with("$DecorView;")
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
        // ---------- inflation ----------
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
        "getMenuInflater" | "getSupportMenuInflater" => {
            Some(Some(host(vm, "Landroid/view/MenuInflater;", HostState::Empty)))
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
        "size" if is_menuish(vm, this) => {
            Some(Some(V::I32(menu_items(vm, this).len() as i32)))
        }
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
                    items
                        .into_iter()
                        .map(|(k, v)| (V::I32(k), v))
                        .collect()
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
        "setVisible" | "setEnabled" | "setChecked" | "setCheckable"
        | "setTitle" | "setTitleCondensed" | "setIcon" | "setShowAsAction"
        | "setShowAsActionFlags" | "setActionView" | "setIntent"
        | "setAlphabeticShortcut" | "setNumericShortcut" | "setTooltipText"
        | "setContentDescription" | "setGroupVisible" if is_menuitem(vm, this) =>
        {
            let key = match name {
                "setVisible" => P_MENU_VIS,
                "setEnabled" => P_MENU_EN,
                "setCheckable" => P_MENU_CKABLE,
                "setChecked" => P_MENU_CHK,
                "setTitle" | "setTitleCondensed" => P_TEXT,
                _ => P_TAG,
            };
            let v = args.get(1).copied().unwrap_or(V::Ref(0));
            let v = if key == P_TEXT { menu_title(vm, v) } else { v };
            pseudo_set(vm, this, key, v);
            // AOSP item mutators return the item for chaining.
            Some(Some(V::Ref(this)))
        }
        "setOnMenuItemClickListener" | "setOnActionExpandListener"
            if is_menuitem(vm, this) =>
        {
            pseudo_set(vm, this, P_LISTENER, args.get(1).copied().unwrap_or(V::Ref(0)));
            Some(Some(V::Ref(this)))
        }
        "getItemId" if is_menuitem(vm, this) => {
            Some(Some(pseudo_get(vm, this, P_MENU_ID).unwrap_or(V::I32(0))))
        }
        "getGroupId" if is_menuitem(vm, this) => {
            Some(Some(pseudo_get(vm, this, P_MENU_GROUP).unwrap_or(V::I32(0))))
        }
        "isVisible" | "isEnabled" | "isChecked" | "isCheckable"
            if is_menuitem(vm, this) =>
        {
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
            let hit = find_in_tree(vm, root, id).unwrap_or(0);
            if hit == 0 && std::env::var_os("OVELO_VIEWDBG").is_some() {
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
        "setId" => {
            pseudo_set(vm, this, P_VIEW_ID, args.get(1).copied().unwrap_or(V::I32(0)));
            Some(None)
        }
        "getChildCount" => {
            let n = children(vm, this).len();
            if std::env::var_os("OVELO_VIEWDBG").is_some() {
                let d = class_of(vm, V::Ref(this));
                eprintln!("[viewdbg] getChildCount {d} h{this} -> {n}");
            }
            Some(Some(V::I32(n as i32)))
        }
        "getChildAt" => Some(Some(
            children(vm, this)
                .get(i32v(args.get(1).copied().unwrap_or(V::I32(0))) as usize)
                .copied()
                .unwrap_or(V::Ref(0)),
        )),
        "indexOfChild" => {
            let target = args.get(1).copied().unwrap_or(V::Ref(0));
            let i = children(vm, this)
                .iter()
                .position(|&c| value_eq(vm, c, target))
                .map(|i| i as i32)
                .unwrap_or(-1);
            Some(Some(V::I32(i)))
        }
        "addView" | "addViewInLayout" | "addFocusables" | "addChildrenForAccessibility" => {
            if let Some(&child @ V::Ref(_)) = args.get(1) {
                attach(vm, this, child);
            }
            Some(match ret(sig) {
                "V" => None,
                _ => Some(stub_ret(vm, sig)),
            })
        }
        "removeView" | "removeViewInLayout" => {
            detach(vm, this, args.get(1).copied().unwrap_or(V::Ref(0)));
            Some(None)
        }
        "removeViewAt" => {
            let i = i32v(args.get(1).copied().unwrap_or(V::I32(0))) as usize;
            if let Some(arrh) = children_arr(vm, this) {
                if let Some(Obj::Array { data, .. }) = vm.heap.get_mut(arrh) {
                    if i < data.len() {
                        let gone = data.remove(i);
                        if let V::Ref(ch) = gone {
                            pseudo_set(vm, ch, P_PARENT, V::Ref(0));
                        }
                    }
                }
            }
            Some(None)
        }
        "removeAllViews" => {
            for c in children(vm, this) {
                if let V::Ref(ch) = c {
                    pseudo_set(vm, ch, P_PARENT, V::Ref(0));
                }
            }
            if let Some(arrh) = children_arr(vm, this) {
                if let Some(Obj::Array { data, .. }) = vm.heap.get_mut(arrh) {
                    data.clear();
                }
            }
            Some(None)
        }
        "bringChildToFront" => Some(None),
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
        "getTag" => Some(Some(
            match args.get(1) {
                Some(&key) => pseudo_get(vm, this, tag_key(i32v(key))),
                None => pseudo_get(vm, this, P_TAG),
            }
            .unwrap_or(V::Ref(0)),
        )),
        "setTag" => {
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
        "setText" | "setHint" | "setTitle" | "setContentDescription" | "setTextScaleX" => {
            if name != "setTextScaleX" {
                pseudo_set(vm, this, P_TEXT, args.get(1).copied().unwrap_or(V::Ref(0)));
            }
            Some(None)
        }
        "getText" | "getHint" | "getTitle" | "getContentDescription" => Some(Some(
            pseudo_get(vm, this, P_TEXT).unwrap_or_else(|| jstr(vm, "")),
        )),
        "setOnClickListener" | "setOnLongClickListener" | "setOnTouchListener"
        | "setOnFocusChangeListener" | "setOnKeyListener" | "setOnAttachStateChangeListener"
        | "addOnAttachStateChangeListener" => {
            pseudo_set(vm, this, P_LISTENER, args.get(1).copied().unwrap_or(V::Ref(0)));
            Some(match ret(sig) {
                "V" => None,
                _ => Some(V::I32(1)),
            })
        }
        "performClick" | "callOnClick" => {
            let fired = match pseudo_get(vm, this, P_LISTENER) {
                Some(V::Ref(l)) if l != 0 => vm
                    .call_virtual(l, "onClick", &[V::Ref(this)])
                    .ok()
                    .flatten()
                    .is_some(),
                _ => false,
            };
            Some(Some(V::I32(fired as i32)))
        }
        "setVisibility" => {
            pseudo_set(vm, this, P_VIS, args.get(1).copied().unwrap_or(V::I32(0)));
            Some(None)
        }
        "getVisibility" => Some(Some(
            pseudo_get(vm, this, P_VIS).unwrap_or(V::I32(0)),
        )),
        "isShown" | "isAttachedToWindow" | "hasWindowFocus" | "isEnabled"
        | "isClickable" | "isFocusable" | "isFocusableInTouchMode" | "isFocusableOnScreen"
        | "hasFocusable" | "isImportantForAccessibility" => Some(Some(V::I32(1))),
        "isSelected" | "isActivated" | "isPressed" | "isFocused" | "hasFocus"
        | "isDirty" | "isOpaque" | "isInEditMode" | "isHardwareAccelerated" => {
            Some(Some(V::I32(0)))
        }
        "getVisibility0" => Some(Some(V::I32(0))),
        "getResources" => Some(Some(host(vm, "Landroid/content/res/Resources;", HostState::Empty))),
        "getHandler" => Some(Some(host(vm, "Landroid/os/Handler;", HostState::Empty))),
        "post" | "postDelayed" | "postOnAnimation" | "postOnAnimationDelayed" => {
            Some(Some(V::I32(1)))
        }
        "removeCallbacks" => Some(Some(V::I32(1))),
        "animate" => Some(Some(host(
            vm,
            "Landroid/view/ViewPropertyAnimator;",
            HostState::Empty,
        ))),
        "getLayoutParams" => {
            let lp = layout_params(vm, this);
            if std::env::var_os("OVELO_VIEWDBG").is_some() {
                let desc = class_of(vm, V::Ref(this));
                if desc.contains("DrawerLayout") || desc.contains("NavigationView") {
                    let lp_desc = match lp {
                        V::Ref(h) => class_of(vm, V::Ref(h)),
                        _ => "primitive".into(),
                    };
                    eprintln!("[viewdbg] {desc}.getLayoutParams h{this} -> {lp_desc}");
                }
            }
            Some(Some(lp))
        }
        "setLayoutParams" => {
            pseudo_set(vm, this, P_LP, args.get(1).copied().unwrap_or(V::Ref(0)));
            Some(None)
        }
        "requestLayout" | "invalidate" | "postInvalidate"
        | "requestFocus" | "clearFocus" | "forceLayout" | "measure" | "layout"
        | "setAlpha" | "setBackgroundColor" | "setBackgroundResource" | "setPadding"
        | "setMinimumHeight" | "setMinimumWidth" | "setElevation" | "setTranslationX"
        | "setTranslationY" | "setTranslationZ" | "setScaleX" | "setScaleY" | "setX"
        | "setY" | "setRotation" | "setScrollX" | "setScrollY" | "scrollTo" | "scrollBy"
        | "setSelected" | "setActivated" | "setPressed" | "setEnabled" | "setClickable"
        | "setLongClickable" | "setFocusable" | "setFocusableInTouchMode"
        | "setWillNotDraw" | "setDescendantFocusability" | "setClipChildren"
        | "setClipToPadding" | "setNestedScrollingEnabled" | "startNestedScroll"
        | "stopNestedScroll" | "setSaveEnabled" | "setDuplicateParentStateEnabled"
        | "setLayerType" | "setKeepScreenOn" | "setFitsSystemWindows"
        | "setSystemUiVisibility" | "setForegroundGravity" | "setHorizontalGravity"
        | "setVerticalGravity" | "setGravity" | "setOrientation" | "setWeightSum"
        | "setClipBounds" | "setOutlineAmbientShadowColor" | "setOverScrollMode"
        | "setPivotX" | "setPivotY" | "setTooltipText" | "setTransitionName"
        | "setImportantForAccessibility" | "setAccessibilityLiveRegion"
        | "setBackground" | "setImageResource" | "setImageDrawable" | "setImageBitmap"
        | "setImageURI" | "setImageIcon" | "setTextColor" | "setTextSize" | "setTypeface"
        | "setSingleLine" | "setMaxLines" | "setMinLines" | "setLines" | "setEllipsize"
        | "setInputType" | "setImeOptions" | "setCompoundDrawables" | "setError"
        | "setMovementMethod" | "setMarqueeRepeatLimit" | "setFilters" | "setEnabled_true"
        | "setCheckMarkDrawable" | "setChecked" | "setOnCheckedChangeListener"
        | "setProgress" | "setMax" | "setIndeterminate" | "setAdapter" | "setOnItemClickListener"
        | "setOnItemSelectedListener" | "setOnScrollListener" | "setOnRefreshListener"
        | "setRefreshing" | "setLayoutManager" | "setClipToOutline" | "setOutlineProvider"
        | "setAccessibilityDelegate" | "setAutofillHints" | "setScreenReaderFocusable"
        | "dispatchKeyEvent" | "dispatchTouchEvent" | "dispatchGenericMotionEvent" => {
            Some(match ret(sig) {
                "V" => None,
                "Z" => Some(V::I32(if name == "setRefreshing" { 0 } else { 1 })),
                _ => Some(stub_ret(vm, sig)),
            })
        }

        // ---------- Window ----------
        "getDecorView" | "peekDecorView" => Some(Some(V::Ref(
            decor_of(vm, this).unwrap_or(0),
        ))),
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
            }
            Some(None)
        }
        // On real Android the Activity is the Window.Callback; the
        // window stores its owner on P_TARGET (set by window_of).
        "getCallback" => Some(Some(
            pseudo_get(vm, this, P_LISTENER)
                .or_else(|| pseudo_get(vm, this, P_TARGET))
                .unwrap_or(V::Ref(0)),
        )),
        "setCallback" => {
            pseudo_set(vm, this, P_LISTENER, args.get(1).copied().unwrap_or(V::Ref(0)));
            Some(None)
        }
        "getAttributes" => Some(Some(host(
            vm,
            "Landroid/view/WindowManager$LayoutParams;",
            HostState::Empty,
        ))),
        "setAttributes" | "setLayout" | "addFlags" | "clearFlags"
        | "setFlags" | "setSoftInputMode" | "setFeatureInt" | "takeSurface"
        | "setBackgroundDrawable" | "setBackgroundDrawableResource" | "setDimAmount"
        | "setWindowAnimations" | "setType" | "setStatusBarColor" | "setNavigationBarColor"
        | "setFormat" | "setWindowManager" | "setColorMode" | "addSystemFlags"
        | "setBlurBehindRadius" | "setHideOverlayWindows" | "setDecorFitsSystemWindows" => {
            Some(None)
        }
        "isFloating" | "isActive" | "hasFeature" => Some(Some(V::I32(0))),
        "getWindowManager" | "getContainer" | "getCurrentFocus" | "getNavigationBarColor"
        | "getStatusBarColor" | "getMediaController" | "getLayoutInflater" => {
            Some(Some(stub_ret(vm, sig)))
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

fn inflate_call(vm: &mut Vm, args: &[V]) -> V {
    // inflate(int, ViewGroup, bool) — the parser-taking overloads aren't
    // modeled yet and degrade to a bare view.
    let ctx = args
        .first()
        .and_then(|a| match a {
            V::Ref(h) => pseudo_get(vm, *h, P_CONTEXT),
            _ => None,
        })
        .unwrap_or(V::Ref(0));
    let inflated = match args.get(1).copied() {
        Some(V::I32(id)) if id != 0 => inflate_res(vm, id as u32, ctx),
        _ => empty_view(vm),
    };
    let root = args.get(2).copied().unwrap_or(V::Ref(0));
    let attach_to = args.get(3).map(|v| i32v(*v) != 0).unwrap_or(false);
    if let V::Ref(r) = root {
        if r != 0 && attach_to {
            attach(vm, r, inflated);
            return root;
        }
    }
    inflated
}

/// res_id → inflated view tree (or a bare placeholder when the APK's
/// resources aren't loaded or the id doesn't name a layout).
pub(crate) fn inflate_res(vm: &mut Vm, res_id: u32, ctx: V) -> V {
    let elems = vm
        .apk_res
        .as_ref()
        .and_then(|r| r.layout_elements(res_id));
    if std::env::var_os("OVELO_VIEWDBG").is_some() {
        eprintln!(
            "[viewdbg] inflate {res_id:#x}: {}",
            match &elems {
                Some(e) => format!("{} elems", e.len()),
                None => "no layout".to_string(),
            }
        );
    }
    match elems {
        Some(e) if !e.is_empty() => build_tree(vm, &e, ctx),
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
                format!("L{class_attr};")
            } else {
                VIEW.to_string()
            }
        }
        "View" => VIEW.to_string(),
        _ if name.contains('.') => format!("L{};", name.replace('.', "/")),
        _ => format!("Landroid/widget/{name};"),
    };
    let group = is_group_tag(name) || desc_is_a(&desc, VIEWGROUP);
    // Dex-defined view classes (custom views, bundled libs like
    // material's NavigationView) get a real instance whose constructor
    // runs — the standard overloads are (Context), (Context, AttributeSet)
    // and (Context, AttributeSet, int). A crashed ctor falls back to the
    // opaque host stub.
    let h = try_construct_view(vm, &desc, ctx, attrs).unwrap_or_else(|| {
        if std::env::var_os("OVELO_VIEWDBG").is_some() {
            eprintln!("[viewdbg] make_view_named {desc} -> host stub");
        }
        vm.alloc_host(&desc, HostState::Empty)
    });
    pseudo_set(vm, h, P_IS_VIEW, V::I32(1));
    if group {
        pseudo_set(vm, h, P_IS_GROUP, V::I32(1));
    }
    if !matches!(ctx, V::Ref(0) | V::I32(0)) {
        pseudo_set(vm, h, P_CONTEXT, ctx);
    }
    V::Ref(h)
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
        Some(Obj::Array { data, .. }) => {
            data.iter().map(|v| i64v(*v) as i32).collect()
        }
        _ => Vec::new(),
    }
}

/// Styleable index → the value stored in the backing `AttributeSet`
/// under the attr's entry name (app `resources.arsc` or the framework
/// attr table for `android:` ids), falling back to the style/theme
/// resolution chain for theme-scoped arrays and unset attrs.
/// `None` when the value can't be resolved anywhere.
pub(crate) fn ta_lookup(vm: &mut Vm, h: u32, index: usize) -> Option<V> {
    let attr_id = *ta_attrs(vm, h).get(index)? as u32;
    if std::env::var_os("OVELO_VIEWDBG").is_some() {
        eprintln!(
            "[viewdbg] ta_lookup[{index}] attr_id={attr_id:#x} name={:?}",
            vm.apk_res
                .as_ref()
                .and_then(|r| r.attr_name(attr_id).map(|s| s.to_string()))
        );
    }
    // An XmlResourceParser doubles as AttributeSet: snapshot the
    // element its cursor sits on first — reading values interns
    // strings, which borrows `vm` mutably.
    let parser_attrs: Vec<(String, V)> = match pseudo_get(vm, h, P_TA_SET) {
        Some(V::Ref(set_h))
            if matches!(
                vm.heap.get(set_h),
                Some(Obj::Host {
                    state: HostState::Xml { .. },
                    ..
                })
            ) =>
        {
            xml::cur_attrs(vm, set_h)
        }
        _ => Vec::new(),
    };
    let res = vm.apk_res.as_ref()?;
    // 1) Explicit AttributeSet: match by entry name (app attrs and
    //    framework attrs share the local-name space).
    if let Some(name) = res.attr_name(attr_id).map(|s| s.to_string()) {
        if let Some(v) = parser_attrs
            .iter()
            .find_map(|(n, v)| (*n == name).then_some(*v))
        {
            return Some(v);
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
                    return Some(v);
                }
            }
        }
    }
    // 2) Explicit style (obtainStyledAttributes(resId, attrs)).
    if let Some(V::I32(s)) = pseudo_get(vm, h, P_TA_STYLE) {
        if let Some(ev) = res.style_item(s as u32, attr_id).cloned() {
            return Some(entry_v(vm, &ev));
        }
    }
    // 3) defStyleAttr: an attr whose theme value is a style reference.
    if let Some(V::I32(a)) = pseudo_get(vm, h, P_TA_DEF_ATTR) {
        if let Some(style_ref) = res.style_item(vm.theme_id, a as u32).map(|ev| ev.data) {
            if let Some(ev) = res.style_item(style_ref, attr_id).cloned() {
                return Some(entry_v(vm, &ev));
            }
        }
    }
    // 4) defStyleRes: an explicit style resource.
    if let Some(V::I32(r)) = pseudo_get(vm, h, P_TA_DEF_RES) {
        if let Some(ev) = res.style_item(r as u32, attr_id).cloned() {
            return Some(entry_v(vm, &ev));
        }
    }
    // 5) The Activity theme (manifest `android:theme`).
    res.style_item(vm.theme_id, attr_id)
        .cloned()
        .map(|ev| entry_v(vm, &ev))
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
        if std::env::var_os("OVELO_VIEWDBG").is_some() {
            eprintln!("[viewdbg] {desc} not in dex → host stub");
        }
        return None;
    }
    let mut ctors = vm.ctor_list(desc);
    // Score: params needing typed placeholders (fewer is better), then
    // longer signatures (more real state wired up) preferred.
    ctors.sort_by_key(|(_, ps)| (fill_count(ps), usize::MAX - ps.len()));
    for (midx, params) in ctors {
        let args: Vec<V> = params
            .iter()
            .map(|t| ctor_arg(vm, t, ctx, attrs))
            .collect();
        match vm.instantiate_ctor(desc, midx, &args) {
            Ok(h) => return Some(h),
            Err(e) => {
                if std::env::var_os("OVELO_VIEWDBG").is_some() {
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
        _ if t.starts_with('L') || t.starts_with('[') => {
            host(vm, t, HostState::Empty)
        }
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
        if let Ok(Some(v @ V::Ref(h))) = vm.call_virtual(p, "generateLayoutParams", &[attrs]) {
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
                            if std::env::var_os("OVELO_VIEWDBG").is_some() {
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
    host(vm, "Landroid/view/ViewGroup$LayoutParams;", HostState::Empty)
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
            let sub = vm
                .apk_res
                .as_ref()
                .and_then(|r| r.layout_elements(layout));
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
                    if std::env::var_os("OVELO_VIEWDBG").is_some() {
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
    if std::env::var_os("OVELO_VIEWDBG").is_some() {
        let tags: Vec<String> = expanded
            .iter()
            .map(|e| format!("d{}:{}", e.depth, e.name))
            .collect();
        eprintln!("[viewdbg] expanded: {}", tags.join(", "));
    }
    let mut stack: Vec<(usize, u32)> = Vec::new();
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
        let V::Ref(h) = make_view_named(vm, &name, attrs, ctx) else {
            continue;
        };
        pseudo_set(vm, h, P_ATTRS, attrs);
        if let Some(id) = el_id(vm, el) {
            pseudo_set(vm, h, P_VIEW_ID, V::I32(id));
        }
        if let Some(Value::Str(t)) = el.android("tag") {
            let v = jstr(vm, t);
            pseudo_set(vm, h, P_TAG, v);
        }
        if let Some(Value::Str(t)) = el.android("text") {
            let v = jstr(vm, t);
            pseudo_set(vm, h, P_TEXT, v);
        }
        while stack.last().is_some_and(|(d, _)| *d >= el.depth) {
            stack.pop();
        }
        match stack.last() {
            Some((_, p)) => {
                let p = *p;
                attach(vm, p, V::Ref(h));
                // A parent with XML children is a container.
                pseudo_set(vm, p, P_IS_GROUP, V::I32(1));
            }
            None => root = h,
        }
        stack.push((el.depth, h));
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
        Some(Value::Reference(r)) | Some(Value::Other { data: r, .. }) => {
            Some(*r as i32)
        }
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
    (u32::MAX as u64 - 20) - (id as u64 & 0xffff)
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

fn view_id(vm: &Vm, h: u32) -> i32 {
    match pseudo_get(vm, h, P_VIEW_ID) {
        Some(V::I32(i)) => i,
        _ => -1,
    }
}

pub(crate) fn attach(vm: &mut Vm, parent: u32, child: V) {
    if parent == 0 {
        return;
    }
    if let Some(a) = children_arr(vm, parent) {
        if let Some(Obj::Array { data, .. }) = vm.heap.get_mut(a) {
            data.push(child);
        }
        pseudo_set(vm, parent, P_IS_GROUP, V::I32(1));
    }
    if let V::Ref(ch) = child {
        pseudo_set(vm, ch, P_PARENT, V::Ref(parent));
    }
}

fn detach(vm: &mut Vm, parent: u32, child: V) {
    let Some(a) = children_arr(vm, parent) else {
        return;
    };
    let pos = match vm.heap.get(a) {
        Some(Obj::Array { data, .. }) => {
            data.iter().position(|&c| value_eq(vm, c, child))
        }
        _ => None,
    };
    if let Some(i) = pos {
        if let Some(Obj::Array { data, .. }) = vm.heap.get_mut(a) {
            data.remove(i);
        }
        if let V::Ref(ch) = child {
            pseudo_set(vm, ch, P_PARENT, V::Ref(0));
        }
    }
}

/// Depth-first search for a view whose `android:id` equals `id`.
pub(crate) fn find_in_tree(vm: &Vm, root: u32, id: i32) -> Option<u32> {
    if root == 0 {
        return None;
    }
    if view_id(vm, root) == id {
        return Some(root);
    }
    for c in children(vm, root) {
        if let V::Ref(ch) = c {
            if let Some(f) = find_in_tree(vm, ch, id) {
                return Some(f);
            }
        }
    }
    None
}

fn is_marked(vm: &Vm, h: u32) -> bool {
    h != 0 && pseudo_get(vm, h, P_IS_VIEW).is_some()
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

/// The Window's decor view (a ViewGroup), created once. The decor
/// always contains the framework content frame (`android.R.id.content`,
/// `0x01020002`) that app content and AppCompat's sub-decor rebind into —
/// `window.getDecorView().findViewById(android.R.id.content)` must hit it.
pub(crate) fn decor_of(vm: &mut Vm, window: u32) -> Option<u32> {
    if window == 0 {
        return None;
    }
    if let Some(V::Ref(d)) = pseudo_get(vm, window, P_DECOR) {
        return Some(d);
    }
    let d = vm.alloc_host(VIEWGROUP, HostState::Empty);
    pseudo_set(vm, d, P_IS_VIEW, V::I32(1));
    pseudo_set(vm, d, P_IS_GROUP, V::I32(1));
    let cf = vm.alloc_host(FRAMELAYOUT, HostState::Empty);
    pseudo_set(vm, cf, P_IS_VIEW, V::I32(1));
    pseudo_set(vm, cf, P_IS_GROUP, V::I32(1));
    pseudo_set(vm, cf, P_VIEW_ID, V::I32(ANDROID_ID_CONTENT));
    attach(vm, d, V::Ref(cf));
    pseudo_set(vm, window, P_DECOR, V::Ref(d));
    Some(d)
}

/// `android.R.id.content` — the framework content frame's res id.
pub(crate) const ANDROID_ID_CONTENT: i32 = 0x0102_0002;

/// The decor's framework content frame (the view appcompat's
/// `ensureSubDecor` rebinds app content into).
pub(crate) fn content_frame_of(vm: &mut Vm, window: u32) -> Option<u32> {
    let d = decor_of(vm, window)?;
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

pub(crate) fn menu_of(vm: &mut Vm, view_h: u32) -> V {
    if let Some(v @ V::Ref(h)) = pseudo_get(vm, view_h, P_MENU) {
        if h != 0 {
            return v;
        }
    }
    let m = host(vm, "Landroid/view/Menu;", HostState::Map(vec![]));
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
        }) => items
            .iter()
            .map(|(k, v)| (i64v(*k) as i32, *v))
            .collect(),
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
    if menu_store(vm, menu_h).is_none() {
        return V::Ref(0);
    }
    menu_item(vm, menu_h, i64v(id_v.unwrap_or(V::I32(0))) as i32)
}

/// `setTitle(int resId)` stores a resource id; resolve it to the real
/// string so `getTitle()` hands the guest a CharSequence.
fn menu_title(vm: &mut Vm, v: V) -> V {
    if let V::I32(id) = v {
        if let Some(s) = vm
            .apk_res
            .as_ref()
            .and_then(|r| r.string_of(id as u32))
        {
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
    let Some(V::I32(id)) = res_v else { return };
    let Some(doc) = vm.apk_res.as_ref().and_then(|r| r.document(id as u32)) else {
        return;
    };
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
    while *i < doc.len() {
        let el = &doc[*i];
        if el.depth < depth {
            break;
        }
        if el.depth > depth || el.name != "item" {
            *i += 1;
            continue;
        }
        let item_id = el
            .android("id")
            .map(|v| attr_v(vm, v))
            .unwrap_or(V::I32(0));
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
        if let Ok(Some(it)) = vm.call_virtual(
            menu_h,
            m,
            &[V::I32(group), V::I32(id), V::I32(order), title_v],
        ) {
            return it;
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
    if let Some(w) = window_of(vm, activity) {
        if let Some(cf) = content_frame_of(vm, w) {
            attach(vm, cf, cv);
        }
    }
}

// ---------------------------------------------------------------------
// androidx menu inflation — `MenuInflater.inflate(int, Menu)` is
// overridden so menu xml lands on the real Menu object whether it's a
// host Menu (options menu, toolbar) or a dex `MenuBuilder`; item ops
// themselves stay guest-side so the dex list remains authoritative.

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
    use std::cell::RefCell;
    use std::rc::Rc;
    let mk = |f: fn(&mut Vm, &[V]) -> ovelo_error::Result<Option<V>>| -> HostCall {
        Rc::new(RefCell::new(f))
    };
    for class in [
        "Landroid/view/MenuInflater;",
        "Landroidx/appcompat/view/SupportMenuInflater;",
    ] {
        let _ = vm.register_host(class, "inflate", "(ILandroid/view/Menu;)V", mk(ov_menu_inflate));
    }
}

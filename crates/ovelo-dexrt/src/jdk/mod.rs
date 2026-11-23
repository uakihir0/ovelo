//! JDK + Android framework builtin methods.
//!
//! `call` answers a method on a class with no DEX code with real host
//! behavior: `Ok(Some(Some(v)))` produced a value, `Ok(Some(None))`
//! handled-void, `Ok(None)` not covered (caller stubs or reports
//! `dexrt.invoke.unresolved`).

mod android;
mod collect;
pub(crate) mod db;
pub(crate) mod gfx;
pub(crate) mod gl;
pub(crate) mod http;
mod io;
mod lang;
mod rx;
pub(crate) mod time;
mod util;
pub(crate) mod vector;
pub(crate) mod view;
pub(crate) mod xml;

use crate::value::{HostState, Obj, V};
use crate::{Rt, Vm};
use std::collections::HashMap;

pub(crate) const OBJ: &str = "Ljava/lang/Object;";
pub(crate) const THROWABLE: &str = "Ljava/lang/Throwable;";
pub(crate) const STRING: &str = "Ljava/lang/String;";

// Pseudo field-idx keys for state a guest can't address via a real field
// reference (Intent action/data, Runnable target, SharedPreferences link).
pub(crate) const P_ACTION: u32 = u32::MAX - 1;
/// The `P_DATA` pseudo key (Intent data, `*Info` name, ...).
pub const P_DATA: u32 = u32::MAX - 2;
pub(crate) const P_FLAGS: u32 = u32::MAX - 3;
/// The `P_COMPONENT` pseudo key (Intent component, `*Info` package).
pub const P_COMPONENT: u32 = u32::MAX - 4;
/// Resolved `GIdx` of the `<init>` a `Constructor` mirror stands for.
pub(crate) const P_CTOR: u32 = u32::MAX - 36;
/// Resolved `GIdx` of the method a `Method` mirror stands for.
pub(crate) const P_METH: u32 = u32::MAX - 44;
/// A `Drawable`'s cached `Paint` host (ShapeDrawable.getPaint).
pub(crate) const P_PAINT: u32 = u32::MAX - 37;
/// The `P_TARGET` pseudo key (ResolveInfo's `*Info` member, ...).
pub const P_TARGET: u32 = u32::MAX - 5;
pub(crate) const P_EDIT_PARENT: u32 = u32::MAX - 6;
// Rect pseudo-fields (left/top/right/bottom ints) + a Drawable's
// cached bounds Rect handle.
pub(crate) const P_RECT_L: u32 = u32::MAX - 7;
pub(crate) const P_RECT_T: u32 = u32::MAX - 8;
pub(crate) const P_RECT_R: u32 = u32::MAX - 9;
pub(crate) const P_RECT_B: u32 = u32::MAX - 10;
pub(crate) const P_BOUNDS: u32 = u32::MAX - 11;
/// A `BitmapDrawable`'s wrapped Bitmap — kept as a fields entry so the
/// renderer's bitmap field-walk (and `getBitmap`) find it.
pub(crate) const P_BITMAP_SLOT: u64 = 0xFFFD_FEE0;
/// A `ColorDrawable`'s fill ARGB — `Drawable.draw` emits a rect fill.
pub(crate) const P_DRAW_COLOR: u64 = 0xFFFD_FEE1;
/// `ByteBuffer` write/read cursor and endianness (1 = little) — the
/// backing bytes live in `HostState::Slot`.
pub(crate) const P_BBUF_POS: u64 = 0xFFFD_FEE2;
pub(crate) const P_BBUF_ORD: u64 = 0xFFFD_FEE3;
/// Element width (bytes) for a typed buffer view (`asIntBuffer`…):
/// cursor positions are element indices, capacity is backing/esz.
/// Absent or 1 = plain `ByteBuffer` (positions are bytes).
pub(crate) const P_BBUF_ESZ: u64 = 0xFFFD_FEEF;
/// Atomic-slot value on a dex `Obj::Instance` subclassing a host
/// atomic (`AtomicReference`, `AtomicInteger`, `Optional`, …) — RxJava
/// R8-merges subscribers into atomic subclasses, so the slot can't
/// live only on `Obj::Host`.
pub(crate) const P_SLOT: u64 = 0xFFFD_FEF0;
/// Byte offset into the backing array where a typed view's content
/// starts — `asIntBuffer` on a positioned ByteBuffer views it from its
/// position, not from the array base. Absent/0 = no offset.
pub(crate) const P_BBUF_BASE: u64 = 0xFFFD_FF00;
/// `Message` routing: target `Handler`, payload `obj`, `callback`
/// `Runnable` — and a `Handler`'s own `Handler.Callback` — so
/// `obtainMessage(...).sendToTarget()`/`sendMessage` actually deliver
/// (Glide's `EngineJob` drives `onResourceReady` through this path).
pub(crate) const P_MSG_TARGET: u64 = 0xFFFD_FEE4;
pub(crate) const P_MSG_OBJ: u64 = 0xFFFD_FEE5;
pub(crate) const P_MSG_CALLBACK: u64 = 0xFFFD_FEE6;
pub(crate) const P_HANDLER_CB: u64 = 0xFFFD_FEE7;
pub(crate) const P_MSG_WHAT: u64 = 0xFFFD_FEE8;
/// A `LayerDrawable`/`TransitionDrawable`'s `Drawable[]` layers — kept as
/// a fields entry so the renderer's bitmap field-walk reaches the
/// wrapped drawables (Glide's TransitionDrawable).
pub(crate) const P_LAYERS: u64 = 0xFFFD_FEE9;
/// `Message.arg1`/`arg2` int payloads — separate keys so they don't
/// alias `what`.
pub(crate) const P_MSG_ARG1: u64 = 0xFFFD_FEEA;
pub(crate) const P_MSG_ARG2: u64 = 0xFFFD_FEEB;
/// Marks a `TreeMap` view created by `descendingMap()` — key navigation
/// (`firstKey`, `ceilingKey`, `subMap` bounds, …) inverts.
pub(crate) const P_NAV_DESC: u64 = 0xFFFD_FEEC;
/// ProviderInfo.metaData — a Bundle handle carrying the manifest's
/// provider meta-data (androidx.startup's initializer list).
pub const P_META: u64 = 0xFFFD_FEEE;
/// Backing element stores lazily minted for dex subclasses of host
/// collections — nanojson's `JsonArray extends ArrayList` and
/// `JsonObject extends HashMap` allocate as `Obj::Instance` with no
/// `HostState`, so their `add`/`put` ops back onto a host `List`/`Map`
/// whose handle lives under these keys.
pub(crate) const P_LIST_BACKING: u64 = 0xFFFD_FEF1;
pub(crate) const P_MAP_BACKING: u64 = 0xFFFD_FEF2;

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub(crate) fn is_null(v: V) -> bool {
    matches!(v, V::Ref(0))
}

pub(crate) fn i32v(v: V) -> i32 {
    match v {
        V::I32(i) => i,
        V::I64(i) => i as i32,
        V::F32(f) => f as i32,
        V::F64(f) => f as i32,
        _ => 0,
    }
}

pub(crate) fn i64v(v: V) -> i64 {
    match v {
        V::I32(i) => i as i64,
        V::I64(i) => i,
        V::F32(f) => f as i64,
        V::F64(f) => f as i64,
        _ => 0,
    }
}

pub(crate) fn f64v(v: V) -> f64 {
    match v {
        V::I32(i) => i as f64,
        V::I64(i) => i as f64,
        V::F32(f) => f as f64,
        V::F64(f) => f,
        _ => 0.0,
    }
}

/// Text of a CharSequence-ish handle (Str / StrBuf / Path / Regex).
pub(crate) fn s(vm: &Vm, h: u32) -> Option<String> {
    s_d(vm, h, 0)
}

/// Depth-capped: a self-referential holder (e.g. a Map.Entry whose
/// value is itself) must not walk P_DATA forever.
fn s_d(vm: &Vm, h: u32, depth: u8) -> Option<String> {
    if depth > 16 {
        return None;
    }
    match vm.heap.get(h) {
        Some(Obj::Str(t)) => Some(t.clone()),
        Some(Obj::Host {
            state: HostState::StrBuf(t) | HostState::Path(t) | HostState::Regex(t),
            ..
        }) => Some(t.clone()),
        // `new String(x)` — the String.<init> arm stashes the source
        // text under P_DATA so copies read like the interned form.
        // new-instance allocates a Host for Ljava/lang/String;, so both
        // object shapes must answer it.
        Some(Obj::Instance { fields, .. }) | Some(Obj::Host { fields, .. }) => {
            match fields.get(&(P_DATA as u64)) {
                Some(&V::Ref(rh)) if rh != 0 => s_d(vm, rh, depth + 1),
                _ => None,
            }
        }
        _ => None,
    }
}

pub(crate) fn sv(vm: &Vm, v: V) -> Option<String> {
    match v {
        V::Ref(h) => s(vm, h),
        _ => None,
    }
}

pub(crate) fn jstr(vm: &mut Vm, t: &str) -> V {
    V::Ref(vm.heap.intern_str(t))
}

pub(crate) fn arr(vm: &mut Vm, elem: &str, data: Vec<V>) -> V {
    V::Ref(vm.heap.alloc(Obj::Array {
        elem: elem.to_string(),
        data,
    }))
}

pub(crate) fn host(vm: &mut Vm, desc: &str, state: HostState) -> V {
    V::Ref(vm.alloc_host(desc, state))
}

/// The one main-thread object — androidx checks
/// `Looper.getMainLooper().getThread() == Thread.currentThread()` by
/// identity, so both must return the same handle.
pub(crate) fn main_thread(vm: &mut Vm) -> V {
    if vm.main_thread_h == 0 {
        let V::Ref(h) = host(vm, "Ljava/lang/Thread;", HostState::StrBuf("main".into())) else {
            unreachable!()
        };
        vm.main_thread_h = h;
    }
    V::Ref(vm.main_thread_h)
}

/// The one main looper — androidx compares
/// `Looper.myLooper() == hostHandler.getLooper()` by identity, so every
/// source must hand back the same handle.
pub(crate) fn main_looper(vm: &mut Vm) -> V {
    if vm.main_looper_h == 0 {
        let V::Ref(h) = host(vm, "Landroid/os/Looper;", HostState::Empty) else {
            unreachable!()
        };
        vm.main_looper_h = h;
    }
    V::Ref(vm.main_looper_h)
}

/// A background thread handle — distinct from `main_thread` so
/// `assertNotMainThread`-style checks pass inside executor work.
pub(crate) fn worker_thread(vm: &mut Vm) -> V {
    if vm.worker_thread_h == 0 {
        let V::Ref(h) = host(vm, "Ljava/lang/Thread;", HostState::StrBuf("worker".into())) else {
            unreachable!()
        };
        vm.worker_thread_h = h;
    }
    V::Ref(vm.worker_thread_h)
}

/// Run a `Runnable` on the virtual worker thread — executor jobs
/// (Room, WorkManager, kotlinx dispatchers) assert they are NOT on
/// the main thread, so `Thread.currentThread()` must differ while
/// they execute. Nested calls restore the previous context.
/// A queued `Message` isn't a `Runnable` — `drain_runnables` must
/// deliver it to its target `Handler` (callback → `handleMessage`)
/// rather than call `run()` on it.
fn dispatch_message(
    vm: &mut Vm,
    msg: u32,
) -> std::result::Result<Option<V>, ovelo_error::OveloError> {
    if let Some(V::Ref(h)) = pseudo_get(vm, msg, P_MSG_CALLBACK) {
        if h != 0 {
            return vm.call_virtual(h, "run", &[]);
        }
    }
    if let Some(V::Ref(h)) = pseudo_get(vm, msg, P_MSG_TARGET) {
        if h != 0 {
            if let Some(V::Ref(cbh)) = pseudo_get(vm, h, P_HANDLER_CB) {
                if cbh != 0 {
                    // Handler.Callback.handleMessage returning false means
                    // "not handled" — the Handler's own handleMessage must
                    // still see the message.
                    match vm.call_virtual(cbh, "handleMessage", &[V::Ref(msg)]) {
                        Ok(Some(v)) if i32v(v) != 0 => return Ok(Some(v)),
                        Err(e) => return Err(e),
                        _ => {}
                    }
                }
            }
            // Host Handler with no callback: `call_virtual` resolves a
            // dex override if present, else falls back to the builtin
            // `handler_call` arm which no-ops cleanly.
            return vm.call_virtual(h, "handleMessage", &[V::Ref(msg)]);
        }
    }
    Ok(None)
}

/// `true` when `h` is a host `Message` needing `dispatch_message`.
fn is_message(vm: &Vm, h: u32) -> bool {
    matches!(
        vm.heap.get(h),
        Some(Obj::Host { desc, .. }) if desc == "Landroid/os/Message;"
    )
}

pub(crate) fn run_on_worker(
    vm: &mut Vm,
    r: u32,
) -> std::result::Result<Option<V>, ovelo_error::OveloError> {
    let prev = vm.cur_thread;
    let prev_r = vm.cur_runnable;
    worker_thread(vm);
    vm.cur_thread = vm.worker_thread_h;
    vm.cur_runnable = r;
    let v = if is_message(vm, r) {
        dispatch_message(vm, r)
    } else {
        vm.call_virtual(r, "run", &[])
    };
    vm.cur_thread = prev;
    vm.cur_runnable = prev_r;
    v
}

/// Run a `Runnable` on the main thread — a `Handler` bound to the main
/// looper delivers its callbacks there even when `post` is called from
/// inside executor work (LiveData/RecyclerView rely on this).
pub(crate) fn run_on_main(
    vm: &mut Vm,
    r: u32,
) -> std::result::Result<Option<V>, ovelo_error::OveloError> {
    let prev = vm.cur_thread;
    let prev_r = vm.cur_runnable;
    main_thread(vm);
    vm.cur_thread = vm.main_thread_h;
    vm.cur_runnable = r;
    let v = if is_message(vm, r) {
        dispatch_message(vm, r)
    } else {
        vm.call_virtual(r, "run", &[])
    };
    vm.cur_thread = prev;
    vm.cur_runnable = prev_r;
    v
}

/// The one `Application` — `Context.getApplication[Context]` callers
/// `check-cast` the result (e.g. Glide's `RequestManagerRetriever`),
/// so it must be Application-typed, not the Activity receiver.
pub(crate) fn app_object(vm: &mut Vm) -> V {
    if vm.app_h == 0 {
        let V::Ref(h) = host(vm, "Landroid/app/Application;", HostState::Empty) else {
            unreachable!()
        };
        vm.app_h = h;
    }
    V::Ref(vm.app_h)
}

/// Default return for an uncovered method, by its signature —
/// object/array types get a typed placeholder (never null, so chained
/// framework calls keep flowing), String gets "", primitives 0.
pub(crate) fn stub_ret(vm: &mut Vm, sig: &str) -> V {
    match ret(sig) {
        "V" => V::I32(0),
        "Ljava/lang/String;" => jstr(vm, ""),
        r if r.starts_with('[') => V::Ref(vm.heap.alloc(Obj::Array {
            elem: r[1..].to_string(),
            data: Vec::new(),
        })),
        r if r.starts_with('L') => host(vm, r, HostState::Empty),
        _ => V::I32(0),
    }
}

/// `Class` mirror object carrying the named descriptor.
pub(crate) fn class_obj(vm: &mut Vm, desc: String) -> V {
    // Prefer the interned `Obj::Class` mirror so `getParameterTypes()[i]
    // == Foo.class` (and Class equality generally) compares by identity.
    // Non-DEX types keep the string-backed host form.
    if let Some(ci) = vm.type_index_of(&desc) {
        return V::Ref(vm.class_mirror(ci));
    }
    host(vm, "Ljava/lang/Class;", HostState::StrBuf(desc))
}

/// The descriptor a `java.lang.Class` handle names, from either mirror
/// form: `Obj::Class` (`SomeType.class` literal) or a `class_obj` Host.
pub(crate) fn class_desc_of(vm: &Vm, h: u32) -> Option<String> {
    match vm.heap.get(h) {
        Some(Obj::Class { type_idx }) => vm.type_desc_of(*type_idx).ok(),
        _ => s(vm, h),
    }
}

pub(crate) fn fmt_f64(f: f64) -> String {
    if f == f.trunc() && f.is_finite() && f.abs() < 1e15 {
        format!("{f:.1}")
    } else {
        format!("{f}")
    }
}

/// Java toString semantics for any value; guest Instances run their own
/// `toString` when one exists.
pub(crate) fn v_to_s(vm: &mut Vm, v: V) -> String {
    match v {
        V::Ref(0) => "null".to_string(),
        V::I32(i) => i.to_string(),
        V::I64(i) => i.to_string(),
        V::F32(f) => fmt_f64(f as f64),
        V::F64(f) => fmt_f64(f),
        V::Ref(h) => match vm.heap.get(h) {
            Some(Obj::Str(t)) => t.clone(),
            Some(Obj::Host { desc, state, .. }) => match state {
                HostState::StrBuf(t) | HostState::Path(t) | HostState::Regex(t) => t.clone(),
                HostState::Slot(inner) => v_to_s(vm, *inner),
                HostState::Millis(ms) => time::date_to_string(*ms),
                HostState::List(l) => {
                    let parts: Vec<String> = l.iter().map(|e| shallow_s(vm, *e)).collect();
                    format!("[{}]", parts.join(", "))
                }
                HostState::Map(m) => {
                    let parts: Vec<String> = m
                        .iter()
                        .map(|(k, v)| format!("{}={}", shallow_s(vm, *k), shallow_s(vm, *v)))
                        .collect();
                    format!("{{{}}}", parts.join(", "))
                }
                _ => {
                    let d = desc.trim_start_matches('L').trim_end_matches(';');
                    format!("{d}@{h:x}")
                }
            },
            Some(Obj::Instance { .. }) => match vm.call_virtual(h, "toString", &[]) {
                Ok(Some(r)) => sv(vm, r).unwrap_or_else(|| format!("object@{h:x}")),
                _ => format!("object@{h:x}"),
            },
            Some(Obj::Throwable { descriptor, detail }) => {
                let d = descriptor.trim_start_matches('L').trim_end_matches(';');
                if detail.is_empty() {
                    d.to_string()
                } else {
                    format!("{d}: {detail}")
                }
            }
            Some(Obj::Class { type_idx }) => {
                let d = vm.type_desc_of(*type_idx).unwrap_or_else(|_| "?".into());
                format!("class {}", d.trim_start_matches('L').trim_end_matches(';'))
            }
            Some(Obj::Array { elem, .. }) => format!("[{elem}@{h:x}"),
            None => format!("ref@{h:x}"),
        },
    }
}

/// Cheap toString without re-entering guest code.
fn shallow_s(vm: &Vm, v: V) -> String {
    match v {
        V::Ref(h) => s(vm, h).unwrap_or_else(|| format!("@{h:x}")),
        x => crate::jdk::fmt_v(x),
    }
}

pub(crate) fn fmt_v(v: V) -> String {
    match v {
        V::I32(i) => i.to_string(),
        V::I64(i) => i.to_string(),
        V::F32(f) => fmt_f64(f as f64),
        V::F64(f) => fmt_f64(f),
        V::Ref(h) => format!("@{h:x}"),
    }
}

pub(crate) fn value_eq(vm: &Vm, a: V, b: V) -> bool {
    match (a, b) {
        (V::Ref(x), V::Ref(y)) => {
            if x == y {
                return true;
            }
            match (vm.heap.get(x), vm.heap.get(y)) {
                (Some(Obj::Str(p)), Some(Obj::Str(q))) => p == q,
                // Class mirrors are singletons per class in the JVM —
                // compare by type index so `set.contains(Foo.class)`
                // holds across separate const-class literals.
                (Some(Obj::Class { type_idx: a }), Some(Obj::Class { type_idx: b })) => {
                    // Same descriptor may index differently per dex —
                    // compare resolved descriptors, not raw indices.
                    a == b
                        || match (vm.type_desc_of(*a), vm.type_desc_of(*b)) {
                            (Ok(sa), Ok(sb)) => sa == sb,
                            _ => false,
                        }
                }
                // Boxed primitives (Integer/Long/Double/...) are host
                // Slot objects — `equals` compares the wrapped value,
                // not the box identity, or `list.contains(Long.valueOf(x))`
                // never matches (MediaStore id joins drop every row).
                // Other Slot carriers (AtomicReference, Optional, …)
                // inherit Object.equals — identity — so limit this to
                // the value types, and only same-class boxes match
                // (`Integer(5).equals(Long(5))` is false in Java).
                (
                    Some(Obj::Host {
                        desc: da,
                        state: HostState::Slot(a),
                        ..
                    }),
                    Some(Obj::Host {
                        desc: db,
                        state: HostState::Slot(b),
                        ..
                    }),
                ) => {
                    da == db
                        && matches!(
                            da.as_str(),
                            "Ljava/lang/Boolean;"
                                | "Ljava/lang/Byte;"
                                | "Ljava/lang/Character;"
                                | "Ljava/lang/Short;"
                                | "Ljava/lang/Integer;"
                                | "Ljava/lang/Long;"
                                | "Ljava/lang/Float;"
                                | "Ljava/lang/Double;"
                        )
                        && value_eq(vm, *a, *b)
                }
                _ => false,
            }
        }
        _ => a == b,
    }
}

pub(crate) fn desc_is_a(sub: &str, sup: &str) -> bool {
    sub == sup || host_supers(sub).contains(&sup)
}

pub(crate) fn dotted(desc: &str) -> String {
    desc.trim_start_matches('L')
        .trim_end_matches(';')
        .replace('/', ".")
}

pub(crate) fn fields_mut(vm: &mut Vm, h: u32) -> Option<&mut HashMap<u64, V>> {
    match vm.heap.get_mut(h) {
        Some(Obj::Instance { fields, .. }) | Some(Obj::Host { fields, .. }) => Some(fields),
        _ => None,
    }
}

pub(crate) fn field_get(vm: &Vm, h: u32, owner: &str, name: &str) -> Option<V> {
    let idx = vm.field_index_of(owner, name).map(|g| vm.canon_field(g))?;
    match vm.heap.get(h) {
        Some(Obj::Instance { fields, .. }) | Some(Obj::Host { fields, .. }) => {
            fields.get(&idx).copied()
        }
        _ => None,
    }
}

pub(crate) fn field_set(vm: &mut Vm, h: u32, owner: &str, name: &str, v: V) {
    if let Some(idx) = vm.field_index_of(owner, name).map(|g| vm.canon_field(g)) {
        if let Some(f) = fields_mut(vm, h) {
            f.insert(idx, v);
        }
    }
}

/// Read a pseudo field (`P_*` key) off a host/instance object.
pub fn pseudo_get(vm: &Vm, h: u32, key: impl Into<u64>) -> Option<V> {
    match vm.heap.get(h) {
        Some(Obj::Instance { fields, .. }) | Some(Obj::Host { fields, .. }) => {
            fields.get(&key.into()).copied()
        }
        _ => None,
    }
}

/// Write a pseudo field (`P_*` key) on a host/instance object.
pub fn pseudo_set(vm: &mut Vm, h: u32, key: impl Into<u64>, v: V) {
    let key = key.into();
    if let Some(f) = fields_mut(vm, h) {
        // Re-writing the same value (deterministic layout passes do this
        // every pass) must not dirty the tree — the render/layout
        // drivers key their "did anything change" check off view_gen.
        if f.get(&key) == Some(&v) {
            return;
        }
        f.insert(key, v);
        vm.view_gen = vm.view_gen.wrapping_add(1);
    }
}

pub(crate) fn list_mut(vm: &mut Vm, h: u32) -> Option<&mut Vec<V>> {
    match vm.heap.get(h) {
        Some(Obj::Instance { .. }) => {
            // Dex subclasses of host collections (nanojson `JsonArray
            // extends ArrayList`, …) mutate a lazily-minted backing
            // store — delegate fields win when the guest holds one.
            let bh = backing_handle(vm, h, false)?;
            match vm.heap.get_mut(bh) {
                Some(Obj::Host {
                    state: HostState::List(l),
                    ..
                }) => Some(l),
                _ => None,
            }
        }
        _ => match vm.heap.get_mut(h) {
            Some(Obj::Host {
                state: HostState::List(l),
                ..
            }) => Some(l),
            _ => None,
        },
    }
}

pub(crate) fn list_ref(vm: &Vm, h: u32) -> Option<&Vec<V>> {
    match vm.heap.get(h) {
        Some(Obj::Host {
            state: HostState::List(l),
            ..
        }) => Some(l),
        // Guest List implementations (desugared `unmodifiableList`/
        // `Arrays$ArrayList`/`ImmutableCollections` — all R8-renamable,
        // so matching is structural, not by name): resolve the backing
        // store they delegate element access to.
        Some(Obj::Instance { .. }) => match guest_list_backing(vm, h, 8) {
            Some(x) => match vm.heap.get(x) {
                Some(Obj::Host {
                    state: HostState::List(l),
                    ..
                }) => Some(l),
                _ => None,
            },
            None => None,
        },
        _ => None,
    }
}

/// Element snapshot of any List-typed value — host lists, arrays, and
/// guest List implementations whose backing store is reachable. Read
/// sites that copy the source (`addAll`, `copyOf`, constructors…) use
/// this instead of `list_ref`, which borrows and so cannot expose an
/// array-backed guest list.
pub(crate) fn list_snapshot(vm: &Vm, h: u32) -> Option<Vec<V>> {
    match vm.heap.get(h) {
        Some(Obj::Host {
            state: HostState::List(l),
            ..
        }) => Some(l.clone()),
        Some(Obj::Array { data, .. }) => Some(data.clone()),
        Some(Obj::Instance { .. }) => {
            let x = guest_list_backing(vm, h, 8)?;
            match vm.heap.get(x) {
                Some(Obj::Host {
                    state: HostState::List(l),
                    ..
                }) => Some(l.clone()),
                Some(Obj::Array { data, .. }) => Some(data.clone()),
                _ => None,
            }
        }
        _ => None,
    }
}

/// The `HostState::List` or `Obj::Array` a guest Collection
/// implementation delegates to, or None. Verified `instanceof
/// Collection` so arbitrary guest objects holding collection fields
/// are not misread; direct backing fields are preferred over nested
/// delegates, and the depth budget ends reference cycles. A `SubList`
/// collapses to its parent — slightly wrong (offset/size lost) but
/// strictly better than empty.
fn guest_list_backing(vm: &Vm, h: u32, depth: u8) -> Option<u32> {
    if depth == 0 || !vm.is_guest_collection(h) {
        return None;
    }
    // A lazily-minted mutation store (see `backing_handle`) takes
    // priority — delegate fields only describe reads.
    if let Some(V::Ref(bh)) = pseudo_get(vm, h, P_LIST_BACKING) {
        if matches!(
            vm.heap.get(bh),
            Some(Obj::Host {
                state: HostState::List(_),
                ..
            }) | Some(Obj::Array { .. })
        ) {
            return Some(bh);
        }
    }
    let Some(Obj::Instance { fields, .. }) = vm.heap.get(h) else {
        return None;
    };
    let cands: Vec<u32> = fields
        .values()
        .filter_map(|v| match v {
            V::Ref(x) if *x != 0 && *x != h => Some(*x),
            _ => None,
        })
        .collect();
    for &x in &cands {
        if matches!(
            vm.heap.get(x),
            Some(Obj::Host {
                state: HostState::List(_),
                ..
            }) | Some(Obj::Array { .. })
        ) {
            return Some(x);
        }
    }
    for &x in &cands {
        if let Some(Obj::Instance { .. }) = vm.heap.get(x) {
            if let Some(y) = guest_list_backing(vm, x, depth - 1) {
                return Some(y);
            }
        }
    }
    None
}

/// Same search for a guest Map implementation (`JsonObject extends
/// HashMap`, guest `Map` wrappers delegating a field).
fn guest_map_backing(vm: &Vm, h: u32, depth: u8) -> Option<u32> {
    if depth == 0 || !vm.is_guest_map(h) {
        return None;
    }
    if let Some(V::Ref(bh)) = pseudo_get(vm, h, P_MAP_BACKING) {
        if matches!(
            vm.heap.get(bh),
            Some(Obj::Host {
                state: HostState::Map(_),
                ..
            })
        ) {
            return Some(bh);
        }
    }
    let Some(Obj::Instance { fields, .. }) = vm.heap.get(h) else {
        return None;
    };
    let cands: Vec<u32> = fields
        .values()
        .filter_map(|v| match v {
            V::Ref(x) if *x != 0 && *x != h => Some(*x),
            _ => None,
        })
        .collect();
    for &x in &cands {
        if matches!(
            vm.heap.get(x),
            Some(Obj::Host {
                state: HostState::Map(_),
                ..
            })
        ) {
            return Some(x);
        }
    }
    for &x in &cands {
        if let Some(Obj::Instance { .. }) = vm.heap.get(x) {
            if let Some(y) = guest_map_backing(vm, x, depth - 1) {
                return Some(y);
            }
        }
    }
    None
}

/// The element-store handle a guest collection/map subclass mutates
/// through: an existing delegate field's store, or a freshly minted
/// `ArrayList`/`HashMap` host object remembered under the pseudo key.
fn backing_handle(vm: &mut Vm, h: u32, map: bool) -> Option<u32> {
    let key = if map { P_MAP_BACKING } else { P_LIST_BACKING };
    if let Some(V::Ref(bh)) = pseudo_get(vm, h, key) {
        return Some(bh);
    }
    if !map {
        if let Some(x) = guest_list_backing(vm, h, 8) {
            match vm.heap.get(x) {
                Some(Obj::Host {
                    state: HostState::List(_),
                    ..
                }) => return Some(x),
                // Array backing is fixed-size — copy it into a
                // growable store so add/remove keep working.
                Some(Obj::Array { data, .. }) => {
                    let data = data.clone();
                    let bh = vm.alloc_host("Ljava/util/ArrayList;", HostState::List(data));
                    pseudo_set(vm, h, key, V::Ref(bh));
                    return Some(bh);
                }
                _ => {}
            }
        }
        if !vm.is_guest_collection(h) {
            return None;
        }
        let bh = vm.alloc_host("Ljava/util/ArrayList;", HostState::List(Vec::new()));
        pseudo_set(vm, h, key, V::Ref(bh));
        return Some(bh);
    }
    if let Some(x) = guest_map_backing(vm, h, 8) {
        return Some(x);
    }
    if !vm.is_guest_map(h) {
        return None;
    }
    let bh = vm.alloc_host("Ljava/util/HashMap;", HostState::Map(Vec::new()));
    pseudo_set(vm, h, key, V::Ref(bh));
    Some(bh)
}

pub(crate) fn map_mut(vm: &mut Vm, h: u32) -> Option<&mut Vec<(V, V)>> {
    match vm.heap.get(h) {
        Some(Obj::Instance { .. }) => {
            let bh = backing_handle(vm, h, true)?;
            match vm.heap.get_mut(bh) {
                Some(Obj::Host {
                    state: HostState::Map(m),
                    ..
                }) => Some(m),
                _ => None,
            }
        }
        _ => match vm.heap.get_mut(h) {
            Some(Obj::Host {
                state: HostState::Map(m),
                ..
            }) => Some(m),
            _ => None,
        },
    }
}

/// Read-side pairs for any map-typed value — host maps and guest Map
/// implementations whose backing store resolves (`map_pairs` in
/// collect.rs covers the field-scanning views).
pub(crate) fn map_pairs_of(vm: &Vm, h: u32) -> Vec<(V, V)> {
    match vm.heap.get(h) {
        Some(Obj::Host {
            state: HostState::Map(m),
            ..
        }) => m.clone(),
        Some(Obj::Instance { .. }) => {
            match guest_map_backing(vm, h, 8).and_then(|bh| vm.heap.get(bh)) {
                Some(Obj::Host {
                    state: HostState::Map(m),
                    ..
                }) => m.clone(),
                _ => Vec::new(),
            }
        }
        _ => Vec::new(),
    }
}

pub(crate) fn map_get(vm: &Vm, h: u32, k: V) -> Option<V> {
    match vm.heap.get(h) {
        Some(Obj::Host {
            state: HostState::Map(m),
            ..
        }) => m
            .iter()
            .find(|(ek, _)| value_eq(vm, *ek, k))
            .map(|(_, v)| *v),
        Some(Obj::Instance { .. }) => {
            let hit = match guest_map_backing(vm, h, 8).and_then(|bh| vm.heap.get(bh)) {
                Some(Obj::Host {
                    state: HostState::Map(m),
                    ..
                }) => m
                    .iter()
                    .find(|(ek, _)| value_eq(vm, *ek, k))
                    .map(|(_, v)| *v),
                _ => None,
            };
            if std::env::var_os("OVELO_MAPDBG").is_some() {
                let kd = match k {
                    V::Ref(kh) => match vm.heap.get(kh) {
                        Some(Obj::Str(s)) => s.clone(),
                        _ => format!("{kh}"),
                    },
                    v => format!("{v:?}"),
                };
                if kd.contains("humbnail") || kd.contains("Renderer") {
                    eprintln!(
                        "[mapdbg] get h{h} {kd} -> {hit:?} ({})",
                        hit.map(|v| class_of(vm, v)).unwrap_or_else(|| "-".into())
                    );
                }
            }
            hit
        }
        _ => None,
    }
}

pub(crate) fn map_put(vm: &mut Vm, h: u32, k: V, v: V) -> Option<V> {
    let pos = match vm.heap.get(h) {
        Some(Obj::Host {
            state: HostState::Map(m),
            ..
        }) => m.iter().position(|(ek, _)| value_eq(vm, *ek, k)),
        Some(Obj::Instance { .. }) => {
            match guest_map_backing(vm, h, 8).and_then(|bh| vm.heap.get(bh)) {
                Some(Obj::Host {
                    state: HostState::Map(m),
                    ..
                }) => m.iter().position(|(ek, _)| value_eq(vm, *ek, k)),
                _ => None,
            }
        }
        _ => None,
    };
    let m = map_mut(vm, h)?;
    if let Some(i) = pos {
        return Some(std::mem::replace(&mut m[i].1, v));
    }
    m.push((k, v));
    None
}

pub(crate) fn map_remove(vm: &mut Vm, h: u32, k: V) -> Option<V> {
    let i = match vm.heap.get(h) {
        Some(Obj::Host {
            state: HostState::Map(m),
            ..
        }) => m.iter().position(|(ek, _)| value_eq(vm, *ek, k)),
        Some(Obj::Instance { .. }) => {
            match guest_map_backing(vm, h, 8).and_then(|bh| vm.heap.get(bh)) {
                Some(Obj::Host {
                    state: HostState::Map(m),
                    ..
                }) => m.iter().position(|(ek, _)| value_eq(vm, *ek, k)),
                _ => None,
            }
        }
        _ => None,
    }?;
    let m = map_mut(vm, h)?;
    Some(m.remove(i).1)
}

pub(crate) fn map_len(vm: &Vm, h: u32) -> usize {
    match vm.heap.get(h) {
        Some(Obj::Host {
            state: HostState::Map(m),
            ..
        }) => m.len(),
        Some(Obj::Instance { .. }) => {
            match guest_map_backing(vm, h, 8).and_then(|bh| vm.heap.get(bh)) {
                Some(Obj::Host {
                    state: HostState::Map(m),
                    ..
                }) => m.len(),
                _ => 0,
            }
        }
        _ => 0,
    }
}

pub(crate) fn slot_of(vm: &Vm, h: u32) -> Option<V> {
    match vm.heap.get(h) {
        Some(Obj::Host {
            state: HostState::Slot(v),
            ..
        }) => Some(*v),
        Some(Obj::Instance { .. }) | Some(Obj::Host { .. }) => pseudo_get(vm, h, P_SLOT),
        _ => None,
    }
}

/// Write an atomic-style slot value — `HostState::Slot` on host
/// objects, the `P_SLOT` pseudo field on dex instances and host
/// objects without a dedicated slot state (e.g. a `new-instance`
/// `BitSet` minted as `HostState::Empty`).
pub(crate) fn slot_put(vm: &mut Vm, h: u32, v: V) {
    match vm.heap.get_mut(h) {
        Some(Obj::Host {
            state: HostState::Slot(s),
            ..
        }) => *s = v,
        Some(Obj::Instance { fields, .. }) | Some(Obj::Host { fields, .. }) => {
            fields.insert(P_SLOT, v);
        }
        _ => {}
    }
}

/// `sig` → param descriptors.
pub(crate) fn params(sig: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let Some(end) = sig.find(')') else {
        return out;
    };
    let mut i = 1;
    let b = sig.as_bytes();
    while i < end {
        let st = i;
        while i < end && b[i] == b'[' {
            i += 1;
        }
        if i < end && b[i] == b'L' {
            while i < end && b[i] != b';' {
                i += 1;
            }
            i += 1;
        } else if i < end {
            i += 1;
        }
        out.push(&sig[st..i]);
    }
    out
}

/// `sig` → return descriptor.
pub(crate) fn ret(sig: &str) -> &str {
    sig.split_once(')').map(|(_, r)| r).unwrap_or("V")
}

pub(crate) fn class_of(vm: &Vm, v: V) -> String {
    match v {
        V::Ref(h) => match vm.heap.get(h) {
            Some(Obj::Instance { class_idx, .. }) => {
                vm.type_desc_of(*class_idx).unwrap_or_else(|_| OBJ.into())
            }
            Some(Obj::Host { desc, .. }) => desc.clone(),
            Some(Obj::Str(_)) => STRING.into(),
            Some(Obj::Class { .. }) => "Ljava/lang/Class;".into(),
            Some(Obj::Array { elem, .. }) => format!("[{elem}"),
            Some(Obj::Throwable { descriptor, .. }) => descriptor.clone(),
            None => OBJ.into(),
        },
        V::I32(_) => "I".into(),
        V::I64(_) => "J".into(),
        V::F32(_) => "F".into(),
        V::F64(_) => "D".into(),
    }
}

// ---------------------------------------------------------------------
// dispatch

/// Try host behavior for a method on a class with no DEX code.
pub(crate) fn call(
    vm: &mut Vm,
    class: &str,
    name: &str,
    sig: &str,
    args: &[V],
) -> Rt<Option<Option<V>>> {
    // Desugared `j$.*` mirrors `java.*` — dispatch on the canonical name.
    let canon;
    let class = if class.starts_with("Lj$/") {
        canon = class.replacen("Lj$/", "Ljava/", 1);
        &canon
    } else {
        class
    };
    // androidx.sqlite bundled-driver JNI natives — Room3 bypasses the
    // android.database layer entirely, so route them before the generic
    // modules (which never see androidx owners).
    if class.starts_with("Landroidx/sqlite/driver/bundled/") {
        if let Some(r) = db::bundled_call(vm, name, args)? {
            return Ok(Some(r));
        }
    }
    if let Some(r) = universal(vm, class, name, sig, args)? {
        return Ok(Some(r));
    }
    if let Some(r) = lang::call(vm, class, name, sig, args)? {
        return Ok(Some(r));
    }
    if let Some(r) = util::call(vm, class, name, sig, args)? {
        return Ok(Some(r));
    }
    if let Some(r) = collect::call(vm, class, name, sig, args)? {
        return Ok(Some(r));
    }
    if let Some(r) = io::call(vm, class, name, sig, args)? {
        return Ok(Some(r));
    }
    if let Some(r) = time::call(vm, class, name, sig, args)? {
        return Ok(Some(r));
    }
    if let Some(r) = android::call(vm, class, name, sig, args)? {
        return Ok(Some(r));
    }
    if let Some(r) = gfx::call(vm, class, name, sig, args)? {
        return Ok(Some(r));
    }
    if let Some(r) = gl::call_static(vm, class, name, sig, args)? {
        return Ok(Some(r));
    }
    if let Some(r) = gl::call(vm, class, name, sig, args)? {
        return Ok(Some(r));
    }
    Ok(None)
}

/// Methods that mean the same thing on every object.
fn universal(vm: &mut Vm, class: &str, name: &str, sig: &str, args: &[V]) -> Rt<Option<Option<V>>> {
    Ok(match name {
        "toString" if ret(sig) == STRING => {
            let t = v_to_s(vm, args[0]);
            Some(Some(jstr(vm, &t)))
        }
        "hashCode" => Some(Some(lang::hashcode(vm, args))),
        "equals" => Some(Some(V::I32(
            (args.len() > 1 && value_eq(vm, args[0], args[1])) as i32,
        ))),
        "getClass" => {
            let d = class_of(vm, args[0]);
            Some(Some(class_obj(vm, d)))
        }
        "clone" => {
            // Object.clone() is a shallow copy. Returning the receiver
            // itself lets callers mutate the source through the "clone"
            // (e.g. fossify's `getSortedDirectories`/`mDirs` lists grow
            // forever when clone aliases the input).
            let h = match args[0] {
                V::Ref(h) => h,
                _ => {
                    return Ok(Some(Some(args[0])));
                }
            };
            let copy = match vm.heap.get(h) {
                Some(Obj::Host {
                    class_idx,
                    desc,
                    state: HostState::List(l),
                    fields,
                }) => Some(Obj::Host {
                    class_idx: *class_idx,
                    desc: desc.clone(),
                    state: HostState::List(l.clone()),
                    fields: fields.clone(),
                }),
                Some(Obj::Host {
                    class_idx,
                    desc,
                    state: HostState::Map(m),
                    fields,
                }) => Some(Obj::Host {
                    class_idx: *class_idx,
                    desc: desc.clone(),
                    state: HostState::Map(m.clone()),
                    fields: fields.clone(),
                }),
                Some(Obj::Instance { class_idx, fields }) => Some(Obj::Instance {
                    class_idx: *class_idx,
                    fields: fields.clone(),
                }),
                Some(Obj::Array { elem, data }) => Some(Obj::Array {
                    elem: elem.clone(),
                    data: data.clone(),
                }),
                // Strings/class mirrors/other host states: immutable or
                // singleton — aliasing is correct.
                _ => None,
            };
            match copy {
                Some(o) => {
                    let nh = vm.heap.alloc(o);
                    Some(Some(V::Ref(nh)))
                }
                None => Some(Some(args[0])),
            }
        }
        // kotlinx MainCoroutineDispatcher.getImmediate() returns the
        // dispatcher itself.
        "getImmediate" => Some(Some(args[0])),
        "notify" | "notifyAll" | "finalize" => Some(None),
        "wait" => {
            // A worker thread's `wait()` can't really suspend — yield
            // with ovelo.park so the drain lets other runnables (the
            // render task that will set the flag it waits on) run, then
            // re-enter. On the main thread stay a no-op.
            if vm.cur_thread != 0 && vm.cur_thread != vm.main_thread_h {
                // wait() / wait(ms): record a wake deadline — with no
                // timeout use a short retry slice.
                let ms = match args.get(1).copied().unwrap_or(V::I64(1)) {
                    V::I64(m) => m.max(1),
                    V::I32(m) => (m as i64).max(1),
                    _ => 1,
                };
                let due = (now_ms().saturating_add(ms)).max(0) as u64;
                if vm.cur_runnable != 0 {
                    vm.sleep_until.insert(vm.cur_runnable, due);
                }
                return Err(crate::E::Vm(crate::err("ovelo.park")));
            }
            Some(None)
        }
        "compareTo"
            if args.len() == 2
                && !desc_is_a(class, "Ljava/lang/Enum;")
                && !vm.assignable_desc(class, "Ljava/lang/Enum;") =>
        {
            // Generic compareTo: boxed numbers and strings only;
            // enums compare by ordinal in `enum_call`. Dex-defined
            // enums (e.g. androidx `Lifecycle$State`) don't appear in
            // `host_supers`, so `desc_is_a` alone would steal them.
            let c = collect::natural_cmp(vm, args[0], args[1]);
            Some(Some(V::I32(c)))
        }
        _ => {
            let _ = class;
            None
        }
    })
}

// ---------------------------------------------------------------------
// static fields

/// Static field on a host-provided class (`Build.VERSION.SDK_INT`,
/// `StandardCharsets.UTF_8`, `Boolean.TRUE`, `Color.BLACK`, …).
/// `ftype` is the field's declared type — a self-typed field
/// (`TimeUnit.SECONDS: Ljava/util/concurrent/TimeUnit;`) is an enum
/// constant and gets a stable host object carrying its name.
pub(crate) fn sfield(vm: &mut Vm, owner: &str, name: &str, ftype: &str) -> Option<V> {
    let v: V = match (owner, name) {
        ("Ljava/lang/Integer;", "MAX_VALUE") => V::I32(i32::MAX),
        ("Ljava/lang/Integer;", "MIN_VALUE") => V::I32(i32::MIN),
        ("Ljava/lang/Integer;", "SIZE") => V::I32(32),
        ("Ljava/lang/Integer;", "BYTES") => V::I32(4),
        ("Ljava/lang/Long;", "MAX_VALUE") => V::I64(i64::MAX),
        ("Ljava/lang/Long;", "MIN_VALUE") => V::I64(i64::MIN),
        ("Ljava/lang/Long;", "SIZE") => V::I32(64),
        ("Ljava/lang/Long;", "BYTES") => V::I32(8),
        ("Ljava/lang/Short;", "MAX_VALUE") => V::I32(i16::MAX as i32),
        ("Ljava/lang/Short;", "MIN_VALUE") => V::I32(i16::MIN as i32),
        ("Ljava/lang/Short;", "SIZE") => V::I32(16),
        ("Ljava/lang/Short;", "BYTES") => V::I32(2),
        ("Ljava/lang/Byte;", "MAX_VALUE") => V::I32(i8::MAX as i32),
        ("Ljava/lang/Byte;", "MIN_VALUE") => V::I32(i8::MIN as i32),
        ("Ljava/lang/Byte;", "SIZE") => V::I32(8),
        ("Ljava/lang/Byte;", "BYTES") => V::I32(1),
        ("Ljava/lang/Character;", "MAX_VALUE") => V::I32(0xFFFF),
        ("Ljava/lang/Character;", "MIN_VALUE") => V::I32(0),
        ("Ljava/lang/Character;", "SIZE") => V::I32(16),
        ("Ljava/lang/Character;", "BYTES") => V::I32(2),
        ("Ljava/lang/Double;", "MAX_VALUE") => V::F64(f64::MAX),
        ("Ljava/lang/Double;", "MIN_VALUE") => V::F64(f64::MIN_POSITIVE),
        ("Ljava/lang/Double;", "MIN_NORMAL") => V::F64(f64::MIN_POSITIVE),
        ("Ljava/lang/Double;", "POSITIVE_INFINITY") => V::F64(f64::INFINITY),
        ("Ljava/lang/Double;", "NEGATIVE_INFINITY") => V::F64(f64::NEG_INFINITY),
        ("Ljava/lang/Double;", "NaN") => V::F64(f64::NAN),
        ("Ljava/lang/Double;", "SIZE") => V::I32(64),
        ("Ljava/lang/Double;", "BYTES") => V::I32(8),
        ("Ljava/lang/Float;", "MAX_VALUE") => V::F32(f32::MAX),
        ("Ljava/lang/Float;", "MIN_VALUE") => V::F32(f32::MIN_POSITIVE),
        ("Ljava/lang/Float;", "MIN_NORMAL") => V::F32(f32::MIN_POSITIVE),
        ("Ljava/lang/Float;", "POSITIVE_INFINITY") => V::F32(f32::INFINITY),
        ("Ljava/lang/Float;", "NEGATIVE_INFINITY") => V::F32(f32::NEG_INFINITY),
        ("Ljava/lang/Float;", "NaN") => V::F32(f32::NAN),
        ("Ljava/lang/Float;", "SIZE") => V::I32(32),
        ("Ljava/lang/Float;", "BYTES") => V::I32(4),
        ("Ljava/lang/Math;", "PI") => V::F64(std::f64::consts::PI),
        ("Ljava/lang/Math;", "E") => V::F64(std::f64::consts::E),
        ("Ljava/io/File;", "separator") => {
            return Some(jstr(vm, "/"));
        }
        ("Ljava/io/File;", "pathSeparator") => {
            return Some(jstr(vm, ":"));
        }
        ("Ljava/io/File;", "separatorChar") => V::I32('/' as i32),
        ("Ljava/io/File;", "pathSeparatorChar") => V::I32(':' as i32),
        ("Landroid/os/Environment;", "DIRECTORY_MUSIC") => {
            return Some(jstr(vm, "Music"));
        }
        ("Landroid/os/Environment;", "DIRECTORY_PODCASTS") => {
            return Some(jstr(vm, "Podcasts"));
        }
        ("Landroid/os/Environment;", "DIRECTORY_RINGTONES") => {
            return Some(jstr(vm, "Ringtones"));
        }
        ("Landroid/os/Environment;", "DIRECTORY_ALARMS") => {
            return Some(jstr(vm, "Alarms"));
        }
        ("Landroid/os/Environment;", "DIRECTORY_NOTIFICATIONS") => {
            return Some(jstr(vm, "Notifications"));
        }
        ("Landroid/os/Environment;", "DIRECTORY_PICTURES") => {
            return Some(jstr(vm, "Pictures"));
        }
        ("Landroid/os/Environment;", "DIRECTORY_DCIM") => {
            return Some(jstr(vm, "DCIM"));
        }
        ("Landroid/os/Environment;", "DIRECTORY_DOWNLOADS") => {
            return Some(jstr(vm, "Download"));
        }
        ("Landroid/os/Environment;", "DIRECTORY_MOVIES") => {
            return Some(jstr(vm, "Movies"));
        }
        ("Landroid/os/Environment;", "DIRECTORY_DOCUMENTS") => {
            return Some(jstr(vm, "Documents"));
        }
        ("Landroid/os/Environment;", "DIRECTORY_RECORDINGS") => {
            return Some(jstr(vm, "Recordings"));
        }
        ("Landroid/os/Environment;", "DIRECTORY_SCREENSHOTS") => {
            return Some(jstr(vm, "Screenshots"));
        }
        ("Landroid/os/Environment;", "DIRECTORY_AUDIOBOOKS") => {
            return Some(jstr(vm, "Audiobooks"));
        }
        ("Ljava/lang/Boolean;", "TRUE") => {
            return Some(host(vm, "Ljava/lang/Boolean;", HostState::Slot(V::I32(1))))
        }
        ("Ljava/lang/Boolean;", "FALSE") => {
            return Some(host(vm, "Ljava/lang/Boolean;", HostState::Slot(V::I32(0))))
        }
        (o, "TYPE") if lang::primitive_type(o) => {
            // `Integer.TYPE` is `int.class` — the primitive descriptor,
            // not the boxed class.
            let prim = match o {
                "Ljava/lang/Integer;" => "I",
                "Ljava/lang/Long;" => "J",
                "Ljava/lang/Short;" => "S",
                "Ljava/lang/Byte;" => "B",
                "Ljava/lang/Character;" => "C",
                "Ljava/lang/Boolean;" => "Z",
                "Ljava/lang/Double;" => "D",
                _ => "F",
            };
            return Some(class_obj(vm, prim.to_string()));
        }
        ("Ljava/lang/System;", "out") | ("Ljava/lang/System;", "err") => {
            return Some(host(vm, "Ljava/io/PrintStream;", HostState::Empty))
        }
        ("Ljava/lang/System;", "in") => {
            return Some(host(vm, "Ljava/io/InputStream;", HostState::Empty))
        }
        ("Ljava/nio/ByteOrder;", "BIG_ENDIAN") => {
            return Some(host(vm, "Ljava/nio/ByteOrder;", HostState::Slot(V::I32(0))))
        }
        ("Ljava/nio/ByteOrder;", "LITTLE_ENDIAN") => {
            return Some(host(vm, "Ljava/nio/ByteOrder;", HostState::Slot(V::I32(1))))
        }
        ("Ljava/lang/Void;", "TYPE") => return Some(class_obj(vm, "V".into())),
        ("Ljava/lang/String;", "CASE_INSENSITIVE_ORDER") => {
            return Some(host(vm, "Ljava/util/Comparator;", HostState::Empty))
        }
        ("Ljava/nio/charset/StandardCharsets;", n) => {
            return Some(host(
                vm,
                "Ljava/nio/charset/Charset;",
                HostState::StrBuf(n.replace('_', "-")),
            ))
        }
        ("Ljava/util/Locale;", "ENGLISH") => {
            return Some(host(
                vm,
                "Ljava/util/Locale;",
                HostState::StrBuf("en".into()),
            ))
        }
        ("Ljava/util/Locale;", "US") => {
            return Some(host(
                vm,
                "Ljava/util/Locale;",
                HostState::StrBuf("en-US".into()),
            ))
        }
        ("Ljava/util/Locale;", "UK") => {
            return Some(host(
                vm,
                "Ljava/util/Locale;",
                HostState::StrBuf("en-GB".into()),
            ))
        }
        ("Ljava/util/Locale;", "JAPAN" | "JAPANESE") => {
            return Some(host(
                vm,
                "Ljava/util/Locale;",
                HostState::StrBuf("ja-JP".into()),
            ))
        }
        ("Ljava/util/Locale;", "GERMAN" | "GERMANY") => {
            return Some(host(
                vm,
                "Ljava/util/Locale;",
                HostState::StrBuf("de-DE".into()),
            ))
        }
        ("Ljava/util/Locale;", "FRENCH" | "FRANCE") => {
            return Some(host(
                vm,
                "Ljava/util/Locale;",
                HostState::StrBuf("fr-FR".into()),
            ))
        }
        ("Ljava/util/Locale;", "CHINESE" | "CHINA" | "SIMPLIFIED_CHINESE") => {
            return Some(host(
                vm,
                "Ljava/util/Locale;",
                HostState::StrBuf("zh-CN".into()),
            ))
        }
        ("Ljava/util/Locale;", "ROOT") => {
            return Some(host(vm, "Ljava/util/Locale;", HostState::StrBuf("".into())))
        }
        ("Ljava/util/Collections;", "EMPTY_LIST") => {
            return Some(host(
                vm,
                "Ljava/util/Collections$EmptyList;",
                HostState::List(Vec::new()),
            ))
        }
        ("Ljava/util/Collections;", "EMPTY_SET") => {
            return Some(host(
                vm,
                "Ljava/util/Collections$EmptySet;",
                HostState::List(Vec::new()),
            ))
        }
        ("Ljava/util/Collections;", "EMPTY_MAP") => {
            return Some(host(
                vm,
                "Ljava/util/Collections$EmptyMap;",
                HostState::Map(Vec::new()),
            ))
        }
        ("Ljava/util/regex/Pattern;", "CASE_INSENSITIVE") => V::I32(2),
        ("Ljava/util/regex/Pattern;", "MULTILINE") => V::I32(8),
        ("Ljava/util/regex/Pattern;", "DOTALL") => V::I32(32),
        ("Ljava/util/regex/Pattern;", "UNICODE_CASE") => V::I32(64),
        ("Ljava/util/regex/Pattern;", "CANON_EQ") => V::I32(128),
        ("Ljava/util/regex/Pattern;", "UNIX_LINES") => V::I32(1),
        ("Ljava/util/regex/Pattern;", "LITERAL") => V::I32(16),
        ("Ljava/util/regex/Pattern;", "COMMENTS") => V::I32(4),
        ("Landroid/util/Base64;", "DEFAULT") => V::I32(0),
        ("Landroid/util/Base64;", "NO_PADDING") => V::I32(1),
        ("Landroid/util/Base64;", "NO_WRAP") => V::I32(2),
        ("Landroid/util/Base64;", "CRLF") => V::I32(4),
        ("Landroid/util/Base64;", "URL_SAFE") => V::I32(8),
        ("Landroid/util/Base64;", "NO_CLOSE") => V::I32(16),
        ("Landroid/graphics/Color;", n) => V::I32(match n {
            "BLACK" => 0xFF000000u32 as i32,
            "DKGRAY" => 0xFF444444u32 as i32,
            "GRAY" => 0xFF888888u32 as i32,
            "LTGRAY" => 0xFFCCCCCCu32 as i32,
            "WHITE" => 0xFFFFFFFFu32 as i32,
            "RED" => 0xFFFF0000u32 as i32,
            "GREEN" => 0xFF00FF00u32 as i32,
            "BLUE" => 0xFF0000FFu32 as i32,
            "YELLOW" => 0xFFFFFF00u32 as i32,
            "CYAN" => 0xFF00FFFFu32 as i32,
            "MAGENTA" => 0xFFFF00FFu32 as i32,
            "TRANSPARENT" => 0,
            _ => 0xFF000000u32 as i32,
        }),
        ("Landroid/os/Build$VERSION;", "SDK_INT") => V::I32(35),
        ("Landroid/os/Build$VERSION;", "RELEASE" | "RELEASE_OR_CODENAME") => {
            return Some(jstr(vm, "15"))
        }
        ("Landroid/os/Build$VERSION;", "CODENAME") => return Some(jstr(vm, "REL")),
        ("Landroid/os/Build$VERSION;", "INCREMENTAL") => return Some(jstr(vm, "ovelo")),
        (
            "Landroid/os/Build;",
            "DEVICE" | "MODEL" | "PRODUCT" | "HARDWARE" | "BOARD" | "DISPLAY" | "ID",
        ) => return Some(jstr(vm, "ovelo")),
        ("Landroid/os/Build;", "MANUFACTURER" | "BRAND") => return Some(jstr(vm, "ovelo")),
        (
            "Landroid/os/Build;",
            "FINGERPRINT" | "HOST" | "TAGS" | "TYPE" | "BOOTLOADER" | "RADIO" | "SERIAL"
            | "SOC_MANUFACTURER" | "SOC_MODEL" | "ODM_SKU" | "SKU",
        ) => return Some(jstr(vm, "ovelo")),
        (
            "Landroid/os/Build;",
            "SUPPORTED_ABIS" | "SUPPORTED_32_BIT_ABIS" | "SUPPORTED_64_BIT_ABIS",
        ) => {
            let abi = jstr(vm, "arm64-v8a");
            return Some(arr(vm, STRING, vec![abi]));
        }
        ("Landroid/os/Build;", "CPU_ABI" | "CPU_ABI2") => return Some(jstr(vm, "arm64-v8a")),
        ("Landroid/os/Build$VERSION_CODES;", n) => V::I32(match n {
            "BASE" => 1,
            "DONUT" => 4,
            "ECLAIR" => 5,
            "FROYO" => 8,
            "GINGERBREAD" => 9,
            "HONEYCOMB" => 11,
            "ICE_CREAM_SANDWICH" => 14,
            "JELLY_BEAN" => 16,
            "KITKAT" => 19,
            "LOLLIPOP" => 21,
            "M" => 23,
            "N" => 24,
            "O" => 26,
            "P" => 28,
            "Q" => 29,
            "R" => 30,
            "S" => 31,
            "TIRAMISU" => 33,
            "UPSIDE_DOWN_CAKE" => 34,
            "VANILLA_ICE_CREAM" => 35,
            _ => 35,
        }),
        (o, "SDK_INT") if o.starts_with("Landroid/os/Build") => V::I32(35),
        ("Landroid/content/Context;", n) => return Some(jstr(vm, context_service_name(n))),
        ("Landroid/content/Intent;", n) if n.starts_with("ACTION_") => {
            let a = n
                .trim_start_matches("ACTION_")
                .to_lowercase()
                .replace('_', ".");
            return Some(jstr(
                vm,
                &format!(
                    "android.intent.action.{}",
                    a.to_uppercase().replace('.', "_")
                ),
            ));
        }
        ("Landroid/content/Intent;", n) if n.starts_with("CATEGORY_") => {
            let a = n
                .trim_start_matches("CATEGORY_")
                .to_lowercase()
                .replace('_', ".");
            return Some(jstr(
                vm,
                &format!(
                    "android.intent.category.{}",
                    a.to_uppercase().replace('.', "_")
                ),
            ));
        }
        ("Landroid/content/Intent;", n) if n.starts_with("EXTRA_") => {
            let a = n
                .trim_start_matches("EXTRA_")
                .to_lowercase()
                .replace('_', ".");
            return Some(jstr(
                vm,
                &format!(
                    "android.intent.extra.{}",
                    a.to_uppercase().replace('.', "_")
                ),
            ));
        }
        ("Landroid/content/Intent;", "FLAG_ACTIVITY_NEW_TASK") => V::I32(0x10000000),
        ("Landroid/content/Intent;", "FLAG_ACTIVITY_CLEAR_TOP") => V::I32(0x04000000),
        ("Landroid/content/Intent;", "FLAG_ACTIVITY_SINGLE_TOP") => V::I32(0x20000000),
        ("Landroid/content/Intent;", "FLAG_GRANT_READ_URI_PERMISSION") => V::I32(1),
        ("Landroid/content/Intent;", "FLAG_GRANT_WRITE_URI_PERMISSION") => V::I32(2),
        ("Lorg/json/JSONObject;", "NULL") => {
            return Some(host(vm, "Lorg/json/JSONObject$Null;", HostState::Empty))
        }
        // MediaStore$Images$Media.EXTERNAL_CONTENT_URI & friends —
        // content-provider base URIs guests parse/query on. Null here
        // NPEs inside <clinit> (fossify gallery NewPhotoFetcher).
        (o, "EXTERNAL_CONTENT_URI" | "INTERNAL_CONTENT_URI" | "CONTENT_URI")
            if o.starts_with("Landroid/provider/MediaStore") =>
        {
            // `MediaStore$Images$Media` → "images/media",
            // `MediaStore$Images$Thumbnails` → "images/thumbnails" —
            // the real content-provider path suffixes.
            let media = o
                .trim_start_matches("Landroid/provider/MediaStore$")
                .trim_end_matches(';')
                .split('$')
                .collect::<Vec<_>>()
                .join("/")
                .to_lowercase();
            let vol = if name.starts_with("EXTERNAL") {
                "external"
            } else {
                "internal"
            };
            return Some(host(
                vm,
                "Landroid/net/Uri;",
                HostState::StrBuf(format!("content://media/{vol}/{media}")),
            ));
        }
        // Other android.provider contract *_URI statics — SMS/contacts/
        // call-log/calendar apps sget them and `checkNotNull` on the
        // value, so a null here aborts whole data paths (fossify
        // messages' getConversations died on BlockedNumberContract).
        (o, "CONTENT_URI" | "AUTHORITY_URI" | "BASE_CONTENT_URI")
            if o.starts_with("Landroid/provider/") =>
        {
            let body = o
                .trim_start_matches("Landroid/provider/")
                .trim_end_matches(';');
            let uri = match body {
                "Telephony$Sms" => "content://sms",
                "Telephony$Sms$Inbox" => "content://sms/inbox",
                "Telephony$Sms$Sent" => "content://sms/sent",
                "Telephony$Sms$Draft" => "content://sms/draft",
                "Telephony$Sms$Outbox" => "content://sms/outbox",
                "Telephony$Sms$Conversations" => "content://sms/conversations",
                "Telephony$Mms" => "content://mms",
                "Telephony$Mms$Inbox" => "content://mms/inbox",
                "Telephony$Mms$Sent" => "content://mms/sent",
                "Telephony$Mms$Draft" => "content://mms/draft",
                "Telephony$Mms$Outbox" => "content://mms/outbox",
                "Telephony$Threads" => "content://mms-sms/conversations",
                "Telephony$MmsSms" => "content://mms-sms",
                "Telephony$MmsSms$PendingMessages" => "content://mms-sms/pending",
                "Telephony$CanonicalAddresses" => "content://mms-sms/canonical-addresses",
                "Telephony$CarrierMessages" => "content://carrier_information",
                "BlockedNumberContract$BlockedNumbers" => {
                    "content://com.android.blockednumber/blocked"
                }
                "CallLog$Calls" => "content://call_log/calls",
                "ContactsContract$Contacts" => "content://com.android.contacts/contacts",
                "ContactsContract$RawContacts" => "content://com.android.contacts/raw_contacts",
                "ContactsContract$Data" => "content://com.android.contacts/data",
                "ContactsContract$CommonDataKinds$Phone" => {
                    "content://com.android.contacts/data/phones"
                }
                "ContactsContract$CommonDataKinds$Email" => {
                    "content://com.android.contacts/data/emails"
                }
                "ContactsContract$CommonDataKinds$StructuredName" => {
                    "content://com.android.contacts/data"
                }
                "ContactsContract$CommonDataKinds$Photo" => {
                    "content://com.android.contacts/data/photos"
                }
                "ContactsContract$Groups" => "content://com.android.contacts/groups",
                "ContactsContract$PhoneLookup" => "content://com.android.contacts/phone_lookup",
                "CalendarContract$Events" => "content://com.android.calendar/events",
                "CalendarContract$Calendars" => "content://com.android.calendar/calendars",
                "CalendarContract$Instances" => "content://com.android.calendar/instances/when",
                "CalendarContract$Reminders" => "content://com.android.calendar/reminders",
                "CalendarContract$Attendees" => "content://com.android.calendar/attendees",
                "UserDictionary$Words" => "content://user_dictionary/words",
                "VoicemailContract$Voicemails" => "content://com.android.voicemail/voicemails",
                "Settings$System" => "content://settings/system",
                "Settings$Secure" => "content://settings/secure",
                "Settings$Global" => "content://settings/global",
                "DocumentsContract$Document" => "content://com.android.externalstorage.documents",
                "DocumentsContract$Root" => "content://com.android.externalstorage.documents/root",
                _ => "",
            };
            // Fallback for unmapped contracts: a deterministic URI built
            // from the class name — never null, so `checkNotNull` sites
            // survive; queries on it just return the empty cursor.
            let uri = if uri.is_empty() {
                format!(
                    "content://com.android.{}",
                    body.split('$').collect::<Vec<_>>().join("/").to_lowercase()
                )
            } else {
                uri.to_string()
            };
            return Some(host(vm, "Landroid/net/Uri;", HostState::StrBuf(uri)));
        }
        _ if ftype == owner => {
            // Self-typed constant on a host class: an enum constant
            // (TimeUnit.SECONDS, TimeUnit.MILLISECONDS, …). The pseudo
            // fields give name()/ordinal() something to read; the
            // caller caches it into the class statics so reference
            // equality (`unit == TimeUnit.SECONDS`) holds.
            let h = host(vm, owner, HostState::Empty);
            if let V::Ref(hh) = h {
                let n = jstr(vm, name);
                pseudo_set(vm, hh, P_DATA, n);
                // JDK enum ordinals by declaration order — needed by
                // TimeUnit.convert/ordinal/compareTo (kotlin.time's
                // Duration encoding is built on them).
                let ord = match (owner, name) {
                    ("Ljava/util/concurrent/TimeUnit;", n) => match n {
                        "NANOSECONDS" => 0,
                        "MICROSECONDS" => 1,
                        "MILLISECONDS" => 2,
                        "SECONDS" => 3,
                        "MINUTES" => 4,
                        "HOURS" => 5,
                        "DAYS" => 6,
                        _ => 0,
                    },
                    _ => 0,
                };
                pseudo_set(vm, hh, P_FLAGS, V::I32(ord));
            }
            return Some(h);
        }
        _ => return None,
    };
    Some(v)
}

fn context_service_name(n: &str) -> &'static str {
    match n {
        "LAYOUT_INFLATER_SERVICE" => "layout_inflater",
        "ACTIVITY_SERVICE" => "activity",
        "WINDOW_SERVICE" => "window",
        "NOTIFICATION_SERVICE" => "notification",
        "ALARM_SERVICE" => "alarm",
        "POWER_SERVICE" => "power",
        "KEYGUARD_SERVICE" => "keyguard",
        "INPUT_METHOD_SERVICE" => "input_method",
        "CONNECTIVITY_SERVICE" => "connectivity",
        "WIFI_SERVICE" => "wifi",
        "AUDIO_SERVICE" => "audio",
        "VIBRATOR_SERVICE" | "VIBRATOR_MANAGER_SERVICE" => "vibrator",
        "CLIPBOARD_SERVICE" => "clipboard",
        "LOCATION_SERVICE" => "location",
        "SENSOR_SERVICE" => "sensor",
        "TELEPHONY_SERVICE" => "phone",
        "STORAGE_SERVICE" => "storage",
        "ACCOUNT_SERVICE" => "account",
        "SEARCH_SERVICE" => "search",
        "DOWNLOAD_SERVICE" => "download",
        "UI_MODE_SERVICE" => "uimode",
        "APP_OPS_SERVICE" => "appops",
        "JOB_SCHEDULER_SERVICE" => "jobscheduler",
        "PACKAGE_SERVICE" => "package",
        "NSD_SERVICE" => "servicediscovery",
        "DISPLAY_SERVICE" => "display",
        "ACCESSIBILITY_SERVICE" => "accessibility",
        "TEXT_SERVICES_MANAGER_SERVICE" => "textservices",
        "INPUT_SERVICE" => "input",
        "USER_SERVICE" => "user",
        "BATTERY_SERVICE" => "batterymanager",
        "CAMERA_SERVICE" => "camera",
        "MEDIA_SESSION_SERVICE" => "media_session",
        "NFC_SERVICE" => "nfc",
        "BLUETOOTH_SERVICE" => "bluetooth",
        "DEVICE_POLICY_SERVICE" => "device_policy",
        "DROPBOX_SERVICE" => "dropbox",
        "PRINT_SERVICE" => "print",
        "USAGE_STATS_SERVICE" => "usagestats",
        "MEDIA_ROUTER_SERVICE" => "media_router",
        "SHORTCUT_SERVICE" => "shortcut",
        "WALLPAPER_SERVICE" => "wallpaper",
        "HARDWARE_PROPERTIES_SERVICE" => "hardware_properties",
        "CARRIER_CONFIG_SERVICE" => "carrier_config",
        "COMPANION_DEVICE_SERVICE" => "companiondevice",
        "SYSTEM_HEALTH_SERVICE" => "systemhealth",
        "APP_SEARCH_SERVICE" => "app_search",
        "FILE_SERVICE" => "file",
        "LOADER_CLASS" => "loader",
        _ => "unknown",
    }
}

// ---------------------------------------------------------------------
// instance fields

/// Named field of a host object read on `iget` miss — defaults for
/// fields the guest reads before writing (`PackageInfo.versionCode`,
/// `DisplayMetrics.density`, `Point.x`…).
pub(crate) fn ifield(vm: &Vm, h: u32, owner: &str, name: &str) -> Option<V> {
    match (owner, name) {
        ("Landroid/util/DisplayMetrics;", "density") => Some(V::F32(2.0)),
        ("Landroid/util/DisplayMetrics;", "densityDpi") => Some(V::I32(320)),
        ("Landroid/util/DisplayMetrics;", "scaledDensity") => Some(V::F32(2.0)),
        ("Landroid/util/DisplayMetrics;", "widthPixels") => Some(V::I32(1080)),
        ("Landroid/util/DisplayMetrics;", "heightPixels") => Some(V::I32(1920)),
        ("Landroid/util/DisplayMetrics;", "xdpi" | "ydpi") => Some(V::F32(320.0)),
        // androidx.startup reads `getProviderInfo(...).metaData` for the
        // manifest <meta-data> bundle — the handle our CLI attaches.
        ("Landroid/content/pm/ProviderInfo;", "metaData") => {
            Some(pseudo_get(vm, h, P_META).unwrap_or(V::Ref(0)))
        }
        ("Landroid/content/pm/PackageInfo;", "versionCode") => Some(V::I32(1)),
        ("Landroid/content/pm/PackageInfo;", "versionName") => {
            Some(V::Ref(0)) // caller lacks str context; guests tolerate null
        }
        ("Landroid/content/pm/ApplicationInfo;", "targetSdkVersion") => Some(V::I32(35)),
        ("Landroid/content/pm/ApplicationInfo;", "minSdkVersion") => Some(V::I32(21)),
        ("Landroid/content/pm/ApplicationInfo;", "flags") => Some(V::I32(0)),
        // Manifest-backed component infos the CLI mints for
        // resolveService/queryIntentServices & friends — ResolveInfo
        // links its *Info in P_TARGET; *Info carries name/packageName
        // (P_DATA/P_COMPONENT) and ProviderInfo's authority (P_FLAGS).
        ("Landroid/content/pm/ResolveInfo;", "serviceInfo")
        | ("Landroid/content/pm/ResolveInfo;", "activityInfo")
        | ("Landroid/content/pm/ResolveInfo;", "providerInfo") => {
            Some(pseudo_get(vm, h, P_TARGET).unwrap_or(V::Ref(0)))
        }
        ("Landroid/content/pm/ServiceInfo;", "name")
        | ("Landroid/content/pm/ActivityInfo;", "name")
        | ("Landroid/content/pm/ProviderInfo;", "name") => {
            Some(pseudo_get(vm, h, P_DATA).unwrap_or(V::Ref(0)))
        }
        ("Landroid/content/pm/ServiceInfo;", "packageName")
        | ("Landroid/content/pm/ActivityInfo;", "packageName")
        | ("Landroid/content/pm/ProviderInfo;", "packageName") => {
            Some(pseudo_get(vm, h, P_COMPONENT).unwrap_or(V::Ref(0)))
        }
        ("Landroid/content/pm/ProviderInfo;", "authority") => {
            Some(pseudo_get(vm, h, P_FLAGS).unwrap_or(V::Ref(0)))
        }
        ("Landroid/os/Message;", "what") => {
            Some(pseudo_get(vm, h, P_MSG_WHAT).unwrap_or(V::I32(0)))
        }
        ("Landroid/os/Message;", "arg1") => {
            Some(pseudo_get(vm, h, P_MSG_ARG1).unwrap_or(V::I32(0)))
        }
        ("Landroid/os/Message;", "arg2") => {
            Some(pseudo_get(vm, h, P_MSG_ARG2).unwrap_or(V::I32(0)))
        }
        ("Landroid/os/Message;", "obj") => Some(pseudo_get(vm, h, P_MSG_OBJ).unwrap_or(V::Ref(0))),
        ("Landroid/graphics/Point;", "x" | "y")
        | ("Landroid/graphics/RectF;", "left" | "top" | "right" | "bottom")
        | ("Landroid/graphics/PointF;", "x" | "y") => Some(V::I32(0)),
        // Rect edges are real stored ints (set/setBounds/offset write
        // them as pseudo fields) so drawable bounds round-trip.
        ("Landroid/graphics/Rect;", "left") => {
            Some(pseudo_get(vm, h, P_RECT_L).unwrap_or(V::I32(0)))
        }
        ("Landroid/graphics/Rect;", "top") => {
            Some(pseudo_get(vm, h, P_RECT_T).unwrap_or(V::I32(0)))
        }
        ("Landroid/graphics/Rect;", "right") => {
            Some(pseudo_get(vm, h, P_RECT_R).unwrap_or(V::I32(0)))
        }
        ("Landroid/graphics/Rect;", "bottom") => {
            Some(pseudo_get(vm, h, P_RECT_B).unwrap_or(V::I32(0)))
        }
        ("Landroid/util/TypedValue;", "type" | "data" | "resourceId" | "density") => {
            Some(V::I32(0))
        }
        ("Landroid/content/res/Configuration;", "orientation") => Some(V::I32(1)),
        ("Landroid/content/res/Configuration;", "screenWidthDp")
        | ("Landroid/content/res/Configuration;", "screenHeightDp") => Some(V::I32(360)),
        _ => None,
    }
}

// ---------------------------------------------------------------------
// superclass table (transitively closed)

/// All ancestors + interfaces of a framework/JDK descriptor — direct
/// superclass first where one exists. Used by `assignable_desc` /
/// `framework_super` for host objects and guest classes extending JDK
/// types.
pub(crate) fn host_supers(d: &str) -> &'static [&'static str] {
    match d {
        // ----- java.lang
        "Ljava/lang/Object;" => &[],
        "Ljava/lang/String;" => &[
            "Ljava/lang/CharSequence;",
            "Ljava/lang/Comparable;",
            "Ljava/io/Serializable;",
            OBJ,
        ],
        "Ljava/lang/CharSequence;" => &[OBJ],
        "Ljava/lang/Comparable;" => &[OBJ],
        "Ljava/io/Serializable;" => &[OBJ],
        "Ljava/lang/AbstractStringBuilder;" => {
            &["Ljava/lang/Appendable;", "Ljava/lang/CharSequence;", OBJ]
        }
        "Ljava/lang/StringBuilder;" => &[
            "Ljava/lang/AbstractStringBuilder;",
            "Ljava/lang/Appendable;",
            "Ljava/lang/CharSequence;",
            "Ljava/io/Serializable;",
            OBJ,
        ],
        "Ljava/lang/StringBuffer;" => &[
            "Ljava/lang/AbstractStringBuilder;",
            "Ljava/lang/Appendable;",
            "Ljava/lang/CharSequence;",
            "Ljava/io/Serializable;",
            OBJ,
        ],
        "Ljava/lang/Appendable;" => &[OBJ],
        "Ljava/lang/Number;" => &["Ljava/io/Serializable;", OBJ],
        "Ljava/lang/Integer;" => &["Ljava/lang/Number;", "Ljava/lang/Comparable;", OBJ],
        "Ljava/lang/Long;" => &["Ljava/lang/Number;", "Ljava/lang/Comparable;", OBJ],
        "Ljava/lang/Short;" => &["Ljava/lang/Number;", "Ljava/lang/Comparable;", OBJ],
        "Ljava/lang/Byte;" => &["Ljava/lang/Number;", "Ljava/lang/Comparable;", OBJ],
        "Ljava/lang/Double;" => &["Ljava/lang/Number;", "Ljava/lang/Comparable;", OBJ],
        "Ljava/lang/Float;" => &["Ljava/lang/Number;", "Ljava/lang/Comparable;", OBJ],
        "Ljava/math/BigInteger;" => &["Ljava/lang/Number;", "Ljava/lang/Comparable;", OBJ],
        "Ljava/math/BigDecimal;" => &["Ljava/lang/Number;", "Ljava/lang/Comparable;", OBJ],
        "Ljava/lang/Character;" => &["Ljava/lang/Comparable;", "Ljava/io/Serializable;", OBJ],
        "Ljava/lang/Boolean;" => &["Ljava/lang/Comparable;", "Ljava/io/Serializable;", OBJ],
        "Ljava/lang/Thread;" => &["Ljava/lang/Runnable;", OBJ],
        "Ljava/lang/Runnable;" => &[OBJ],
        "Ljava/lang/ThreadLocal;" => &[OBJ],
        "Ljava/lang/Class;" => &[
            "Ljava/lang/reflect/Type;",
            "Ljava/lang/reflect/GenericDeclaration;",
            "Ljava/lang/reflect/AnnotatedElement;",
            "Ljava/io/Serializable;",
            OBJ,
        ],
        "Ljava/lang/ClassLoader;" => &[OBJ],
        "Ljava/lang/Enum;" => &["Ljava/lang/Comparable;", "Ljava/io/Serializable;", OBJ],
        "Ljava/lang/Throwable;" => &["Ljava/io/Serializable;", OBJ],
        "Ljava/lang/StackTraceElement;" => &["Ljava/io/Serializable;", OBJ],
        "Ljava/lang/System;"
        | "Ljava/lang/Math;"
        | "Ljava/lang/StrictMath;"
        | "Ljava/lang/Runtime;"
        | "Ljava/lang/Process;"
        | "Ljava/lang/Void;" => &[OBJ],
        // ----- java.util collection tree
        "Ljava/lang/Iterable;" => &[OBJ],
        "Ljava/util/Collection;" => &["Ljava/lang/Iterable;", OBJ],
        "Ljava/util/List;" => &["Ljava/util/Collection;", "Ljava/lang/Iterable;", OBJ],
        "Ljava/util/Set;" => &["Ljava/util/Collection;", "Ljava/lang/Iterable;", OBJ],
        "Ljava/util/SortedSet;" => &[
            "Ljava/util/Set;",
            "Ljava/util/Collection;",
            "Ljava/lang/Iterable;",
            OBJ,
        ],
        "Ljava/util/NavigableSet;" => &[
            "Ljava/util/SortedSet;",
            "Ljava/util/Set;",
            "Ljava/util/Collection;",
            "Ljava/lang/Iterable;",
            OBJ,
        ],
        "Ljava/util/Queue;" => &["Ljava/util/Collection;", "Ljava/lang/Iterable;", OBJ],
        "Ljava/util/Deque;" => &[
            "Ljava/util/Queue;",
            "Ljava/util/Collection;",
            "Ljava/lang/Iterable;",
            OBJ,
        ],
        "Ljava/util/AbstractCollection;" => {
            &["Ljava/util/Collection;", "Ljava/lang/Iterable;", OBJ]
        }
        "Ljava/util/AbstractList;" => &[
            "Ljava/util/AbstractCollection;",
            "Ljava/util/List;",
            "Ljava/util/Collection;",
            "Ljava/lang/Iterable;",
            OBJ,
        ],
        // Collections.empty*/singleton*/unmodifiable*/synchronized*
        // wrappers — without these a `check-cast`/`instanceof List`
        // on Collections.emptyList() throws (Glide's empty
        // ModelLoader list failed exactly that way).
        "Ljava/util/Collections$EmptyList;" => &[
            "Ljava/util/AbstractList;",
            "Ljava/util/AbstractCollection;",
            "Ljava/util/List;",
            "Ljava/util/RandomAccess;",
            "Ljava/io/Serializable;",
            "Ljava/util/Collection;",
            "Ljava/lang/Iterable;",
            OBJ,
        ],
        "Ljava/util/Collections$EmptySet;" => &[
            "Ljava/util/AbstractSet;",
            "Ljava/util/AbstractCollection;",
            "Ljava/util/Set;",
            "Ljava/io/Serializable;",
            "Ljava/util/Collection;",
            "Ljava/lang/Iterable;",
            OBJ,
        ],
        "Ljava/util/Collections$EmptyMap;" => &[
            "Ljava/util/AbstractMap;",
            "Ljava/util/Map;",
            "Ljava/io/Serializable;",
            OBJ,
        ],
        "Ljava/util/Collections$EmptyIterator;" => &[OBJ, "Ljava/util/Iterator;"],
        "Ljava/util/Collections$EmptyListIterator;" => {
            &[OBJ, "Ljava/util/ListIterator;", "Ljava/util/Iterator;"]
        }
        "Ljava/util/Collections$EmptyEnumeration;" => &[OBJ, "Ljava/util/Enumeration;"],
        "Ljava/util/Collections$SingletonList;" => &[
            "Ljava/util/AbstractList;",
            "Ljava/util/AbstractCollection;",
            "Ljava/util/List;",
            "Ljava/util/RandomAccess;",
            "Ljava/io/Serializable;",
            "Ljava/util/Collection;",
            "Ljava/lang/Iterable;",
            OBJ,
        ],
        "Ljava/util/Collections$SingletonSet;" => &[
            "Ljava/util/AbstractSet;",
            "Ljava/util/AbstractCollection;",
            "Ljava/util/Set;",
            "Ljava/io/Serializable;",
            "Ljava/util/Collection;",
            "Ljava/lang/Iterable;",
            OBJ,
        ],
        "Ljava/util/Collections$SingletonMap;" => &[
            "Ljava/util/AbstractMap;",
            "Ljava/util/Map;",
            "Ljava/io/Serializable;",
            OBJ,
        ],
        "Ljava/util/Arrays$ArrayList;" => &[
            "Ljava/util/AbstractList;",
            "Ljava/util/AbstractCollection;",
            "Ljava/util/List;",
            "Ljava/util/RandomAccess;",
            "Ljava/io/Serializable;",
            "Ljava/util/Collection;",
            "Ljava/lang/Iterable;",
            OBJ,
        ],
        "Ljava/util/Collections$UnmodifiableCollection;" => &[
            OBJ,
            "Ljava/util/Collection;",
            "Ljava/lang/Iterable;",
            "Ljava/io/Serializable;",
        ],
        "Ljava/util/Collections$UnmodifiableList;" => &[
            "Ljava/util/Collections$UnmodifiableCollection;",
            "Ljava/util/List;",
            "Ljava/util/Collection;",
            "Ljava/lang/Iterable;",
            "Ljava/io/Serializable;",
            OBJ,
        ],
        "Ljava/util/Collections$UnmodifiableSet;" => &[
            "Ljava/util/Collections$UnmodifiableCollection;",
            "Ljava/util/Set;",
            "Ljava/util/Collection;",
            "Ljava/lang/Iterable;",
            "Ljava/io/Serializable;",
            OBJ,
        ],
        "Ljava/util/Collections$UnmodifiableMap;" => {
            &[OBJ, "Ljava/util/Map;", "Ljava/io/Serializable;"]
        }
        "Ljava/util/Collections$SynchronizedCollection;" => &[
            OBJ,
            "Ljava/util/Collection;",
            "Ljava/lang/Iterable;",
            "Ljava/io/Serializable;",
        ],
        "Ljava/util/Collections$SynchronizedList;" => &[
            "Ljava/util/Collections$SynchronizedCollection;",
            "Ljava/util/List;",
            "Ljava/util/Collection;",
            "Ljava/lang/Iterable;",
            OBJ,
        ],
        "Ljava/util/Collections$SynchronizedSet;" => &[
            "Ljava/util/Collections$SynchronizedCollection;",
            "Ljava/util/Set;",
            "Ljava/util/Collection;",
            "Ljava/lang/Iterable;",
            OBJ,
        ],
        "Ljava/util/Collections$SynchronizedMap;" => {
            &[OBJ, "Ljava/util/Map;", "Ljava/io/Serializable;"]
        }
        "Ljava/util/Collections$CheckedCollection;" => &[
            OBJ,
            "Ljava/util/Collection;",
            "Ljava/lang/Iterable;",
            "Ljava/io/Serializable;",
        ],
        "Ljava/util/Collections$CheckedList;" => &[
            "Ljava/util/Collections$CheckedCollection;",
            "Ljava/util/List;",
            "Ljava/util/Collection;",
            "Ljava/lang/Iterable;",
            OBJ,
        ],
        "Ljava/util/Collections$CheckedSet;" => &[
            "Ljava/util/Collections$CheckedCollection;",
            "Ljava/util/Set;",
            "Ljava/util/Collection;",
            "Ljava/lang/Iterable;",
            OBJ,
        ],
        "Ljava/util/Collections$CheckedMap;" => &[OBJ, "Ljava/util/Map;"],
        "Ljava/util/AbstractSequentialList;" => &[
            "Ljava/util/AbstractList;",
            "Ljava/util/AbstractCollection;",
            "Ljava/util/List;",
            "Ljava/util/Collection;",
            "Ljava/lang/Iterable;",
            OBJ,
        ],
        "Ljava/util/ArrayList;" => &[
            "Ljava/util/AbstractList;",
            "Ljava/util/List;",
            "Ljava/util/RandomAccess;",
            "Ljava/util/AbstractCollection;",
            "Ljava/util/Collection;",
            "Ljava/lang/Iterable;",
            "Ljava/lang/Cloneable;",
            "Ljava/io/Serializable;",
            OBJ,
        ],
        "Ljava/util/LinkedList;" => &[
            "Ljava/util/AbstractSequentialList;",
            "Ljava/util/AbstractList;",
            "Ljava/util/List;",
            "Ljava/util/Deque;",
            "Ljava/util/Queue;",
            "Ljava/util/Collection;",
            "Ljava/lang/Iterable;",
            "Ljava/lang/Cloneable;",
            "Ljava/io/Serializable;",
            OBJ,
        ],
        "Ljava/util/Vector;" => &[
            "Ljava/util/AbstractList;",
            "Ljava/util/List;",
            "Ljava/util/RandomAccess;",
            "Ljava/lang/Cloneable;",
            "Ljava/io/Serializable;",
            OBJ,
        ],
        "Ljava/util/Stack;" => &[
            "Ljava/util/Vector;",
            "Ljava/util/List;",
            "Ljava/lang/Cloneable;",
            "Ljava/io/Serializable;",
            OBJ,
        ],
        "Ljava/util/AbstractSet;" => &[
            "Ljava/util/AbstractCollection;",
            "Ljava/util/Set;",
            "Ljava/util/Collection;",
            "Ljava/lang/Iterable;",
            OBJ,
        ],
        "Ljava/util/HashSet;" => &[
            "Ljava/util/AbstractSet;",
            "Ljava/util/Set;",
            "Ljava/util/AbstractCollection;",
            "Ljava/util/Collection;",
            "Ljava/lang/Iterable;",
            "Ljava/lang/Cloneable;",
            "Ljava/io/Serializable;",
            OBJ,
        ],
        "Ljava/util/LinkedHashSet;" => &[
            "Ljava/util/HashSet;",
            "Ljava/util/Set;",
            "Ljava/lang/Cloneable;",
            "Ljava/io/Serializable;",
            OBJ,
        ],
        "Ljava/util/TreeSet;" => &[
            "Ljava/util/AbstractSet;",
            "Ljava/util/NavigableSet;",
            "Ljava/util/SortedSet;",
            "Ljava/util/Set;",
            "Ljava/lang/Cloneable;",
            "Ljava/io/Serializable;",
            OBJ,
        ],
        "Ljava/util/ArrayDeque;" => &[
            "Ljava/util/AbstractCollection;",
            "Ljava/util/Deque;",
            "Ljava/util/Queue;",
            "Ljava/lang/Cloneable;",
            "Ljava/io/Serializable;",
            OBJ,
        ],
        "Ljava/util/AbstractQueue;" => &[
            "Ljava/util/AbstractCollection;",
            "Ljava/util/Queue;",
            "Ljava/util/Collection;",
            "Ljava/lang/Iterable;",
            OBJ,
        ],
        "Ljava/util/PriorityQueue;" => &[
            "Ljava/util/AbstractQueue;",
            "Ljava/util/Queue;",
            "Ljava/io/Serializable;",
            OBJ,
        ],
        "Ljava/util/Map;" => &[OBJ],
        "Ljava/util/SortedMap;" => &["Ljava/util/Map;", OBJ],
        "Ljava/util/NavigableMap;" => &["Ljava/util/SortedMap;", "Ljava/util/Map;", OBJ],
        "Ljava/util/concurrent/ConcurrentMap;" => &["Ljava/util/Map;", OBJ],
        "Ljava/util/AbstractMap;" => &["Ljava/util/Map;", OBJ],
        "Ljava/util/HashMap;" => &[
            "Ljava/util/AbstractMap;",
            "Ljava/util/Map;",
            "Ljava/lang/Cloneable;",
            "Ljava/io/Serializable;",
            OBJ,
        ],
        "Ljava/util/LinkedHashMap;" => &["Ljava/util/HashMap;", "Ljava/util/Map;", OBJ],
        "Ljava/util/TreeMap;" => &[
            "Ljava/util/AbstractMap;",
            "Ljava/util/NavigableMap;",
            "Ljava/util/SortedMap;",
            "Ljava/util/Map;",
            "Ljava/lang/Cloneable;",
            "Ljava/io/Serializable;",
            OBJ,
        ],
        "Ljava/util/Hashtable;" => &[
            "Ljava/util/Dictionary;",
            "Ljava/util/Map;",
            "Ljava/lang/Cloneable;",
            "Ljava/io/Serializable;",
            OBJ,
        ],
        "Ljava/util/Properties;" => &["Ljava/util/Hashtable;", "Ljava/util/Map;", OBJ],
        "Ljava/util/Dictionary;" => &[OBJ],
        "Ljava/util/WeakHashMap;" => &["Ljava/util/AbstractMap;", "Ljava/util/Map;", OBJ],
        "Ljava/util/IdentityHashMap;" => &["Ljava/util/AbstractMap;", "Ljava/util/Map;", OBJ],
        "Ljava/util/concurrent/ConcurrentHashMap;" => &[
            "Ljava/util/AbstractMap;",
            "Ljava/util/concurrent/ConcurrentMap;",
            "Ljava/util/Map;",
            OBJ,
        ],
        "Ljava/util/concurrent/ConcurrentLinkedQueue;" => {
            &["Ljava/util/AbstractQueue;", "Ljava/util/Queue;", OBJ]
        }
        "Ljava/util/concurrent/ConcurrentLinkedDeque;" => {
            &["Ljava/util/AbstractCollection;", "Ljava/util/Deque;", OBJ]
        }
        "Ljava/util/concurrent/CopyOnWriteArrayList;" => &[
            "Ljava/util/List;",
            "Ljava/util/Collection;",
            "Ljava/lang/Iterable;",
            OBJ,
        ],
        "Ljava/util/concurrent/CopyOnWriteArraySet;" => {
            &["Ljava/util/AbstractSet;", "Ljava/util/Set;", OBJ]
        }
        "Ljava/util/concurrent/atomic/AtomicReference;" => &[OBJ],
        "Ljava/util/concurrent/atomic/AtomicInteger;" => &["Ljava/lang/Number;", OBJ],
        "Ljava/util/concurrent/atomic/AtomicLong;" => &["Ljava/lang/Number;", OBJ],
        "Ljava/util/concurrent/atomic/AtomicBoolean;" => &[OBJ],
        "Ljava/util/concurrent/atomic/AtomicReferenceArray;" => &[OBJ],
        "Ljava/util/concurrent/atomic/AtomicIntegerArray;" => &[OBJ],
        "Ljava/util/concurrent/atomic/AtomicLongArray;" => &[OBJ],
        "Ljava/util/concurrent/atomic/AtomicReferenceFieldUpdater;" => &[OBJ],
        "Ljava/util/concurrent/atomic/AtomicIntegerFieldUpdater;" => &[OBJ],
        "Ljava/util/concurrent/atomic/AtomicLongFieldUpdater;" => &[OBJ],
        "Ljava/util/concurrent/atomic/AtomicMarkableReference;" => &[OBJ],
        "Ljava/util/concurrent/atomic/AtomicStampedReference;" => &[OBJ],
        "Ljava/util/concurrent/CountDownLatch;" => &[OBJ],
        "Ljava/util/concurrent/Semaphore;" => &["Ljava/io/Serializable;", OBJ],
        "Ljava/util/concurrent/TimeUnit;" => &["Ljava/lang/Enum;", OBJ],
        "Ljava/util/concurrent/Executor;"
        | "Ljava/util/concurrent/ExecutorService;"
        | "Ljava/util/concurrent/ScheduledExecutorService;"
        | "Ljava/util/concurrent/Callable;"
        | "Ljava/util/concurrent/Future;"
        | "Ljava/util/concurrent/BlockingQueue;" => &[OBJ],
        "Ljava/util/Iterator;"
        | "Ljava/util/Enumeration;"
        | "Ljava/util/Comparator;"
        | "Ljava/util/Spliterator;"
        | "Ljava/util/Observer;"
        | "Ljava/util/EventListener;"
        | "Ljava/util/RandomAccess;"
        | "Ljava/lang/Cloneable;" => &[OBJ],
        "Ljava/util/ListIterator;" => &["Ljava/util/Iterator;", OBJ],
        "Ljava/util/Map$Entry;" => &[OBJ],
        "Ljava/util/AbstractMap$SimpleEntry;" => {
            &["Ljava/util/Map$Entry;", "Ljava/io/Serializable;", OBJ]
        }
        "Ljava/util/AbstractMap$SimpleImmutableEntry;" => {
            &["Ljava/util/Map$Entry;", "Ljava/io/Serializable;", OBJ]
        }
        "Ljava/util/Optional;"
        | "Ljava/util/OptionalInt;"
        | "Ljava/util/OptionalLong;"
        | "Ljava/util/OptionalDouble;" => &[OBJ],
        "Ljava/util/Objects;"
        | "Ljava/util/Arrays;"
        | "Ljava/util/Collections;"
        | "Ljava/util/Scanner;" => &[OBJ],
        "Ljava/util/StringTokenizer;" => &["Ljava/util/Enumeration;", OBJ],
        "Ljava/util/Random;" => &["Ljava/io/Serializable;", OBJ],
        "Ljava/security/SecureRandom;" => &["Ljava/util/Random;", OBJ],
        "Ljava/util/UUID;" => &["Ljava/io/Serializable;", "Ljava/lang/Comparable;", OBJ],
        "Ljava/util/Locale;" => &["Ljava/lang/Cloneable;", "Ljava/io/Serializable;", OBJ],
        "Ljava/util/BitSet;" => &["Ljava/lang/Cloneable;", "Ljava/io/Serializable;", OBJ],
        "Ljava/util/Base64;" | "Ljava/util/Base64$Encoder;" | "Ljava/util/Base64$Decoder;" => {
            &[OBJ]
        }
        "Ljava/util/StringJoiner;" => &[OBJ],
        "Ljava/util/Currency;" => &["Ljava/io/Serializable;", OBJ],
        "Ljava/util/EventObject;" => &["Ljava/io/Serializable;", OBJ],
        "Ljava/util/Observable;" => &[OBJ],
        "Ljava/util/Timer;" | "Ljava/util/TimerTask;" => &[OBJ],
        "Ljava/util/Formatter;" => &[
            "Ljava/io/Closeable;",
            "Ljava/io/Flushable;",
            "Ljava/lang/Appendable;",
            OBJ,
        ],
        "Ljava/util/function/Supplier;"
        | "Ljava/util/function/Function;"
        | "Ljava/util/function/BiFunction;"
        | "Ljava/util/function/Consumer;"
        | "Ljava/util/function/BiConsumer;"
        | "Ljava/util/function/Predicate;"
        | "Ljava/util/function/BiPredicate;"
        | "Ljava/util/function/UnaryOperator;"
        | "Ljava/util/function/BinaryOperator;" => &[OBJ],
        // ----- java.util.date/calendar
        "Ljava/util/Date;" => &[
            "Ljava/io/Serializable;",
            "Ljava/lang/Cloneable;",
            "Ljava/lang/Comparable;",
            OBJ,
        ],
        "Ljava/sql/Date;" | "Ljava/sql/Time;" | "Ljava/sql/Timestamp;" => {
            &["Ljava/util/Date;", OBJ]
        }
        "Ljava/util/Calendar;" => &[
            "Ljava/io/Serializable;",
            "Ljava/lang/Cloneable;",
            "Ljava/lang/Comparable;",
            OBJ,
        ],
        "Ljava/util/GregorianCalendar;" => &["Ljava/util/Calendar;", OBJ],
        "Ljava/util/TimeZone;" => &["Ljava/io/Serializable;", "Ljava/lang/Cloneable;", OBJ],
        "Ljava/util/SimpleTimeZone;" => &["Ljava/util/TimeZone;", OBJ],
        // ----- java.time
        "Ljava/time/Instant;" => &["Ljava/lang/Comparable;", "Ljava/io/Serializable;", OBJ],
        "Ljava/time/LocalDate;"
        | "Ljava/time/LocalTime;"
        | "Ljava/time/LocalDateTime;"
        | "Ljava/time/ZonedDateTime;"
        | "Ljava/time/OffsetDateTime;"
        | "Ljava/time/OffsetTime;"
        | "Ljava/time/Duration;"
        | "Ljava/time/Period;"
        | "Ljava/time/Year;"
        | "Ljava/time/YearMonth;"
        | "Ljava/time/MonthDay;"
        | "Ljava/time/ZoneId;"
        | "Ljava/time/ZoneOffset;"
        | "Ljava/time/DayOfWeek;"
        | "Ljava/time/Month;"
        | "Ljava/time/format/DateTimeFormatter;" => &[OBJ],
        // ----- regex
        "Ljava/util/regex/Pattern;" => &["Ljava/io/Serializable;", OBJ],
        "Ljava/util/regex/Matcher;" => &["Ljava/util/regex/MatchResult;", OBJ],
        "Ljava/util/regex/MatchResult;" => &[OBJ],
        // ----- java.text
        "Ljava/text/Format;" => &["Ljava/io/Serializable;", "Ljava/lang/Cloneable;", OBJ],
        "Ljava/text/DateFormat;" => &["Ljava/text/Format;", OBJ],
        "Ljava/text/SimpleDateFormat;" => &["Ljava/text/DateFormat;", "Ljava/text/Format;", OBJ],
        "Ljava/text/NumberFormat;" => &["Ljava/text/Format;", OBJ],
        "Ljava/text/DecimalFormat;" => &["Ljava/text/NumberFormat;", "Ljava/text/Format;", OBJ],
        "Ljava/text/MessageFormat;" => &["Ljava/text/Format;", OBJ],
        "Ljava/text/Collator;" => &["Ljava/util/Comparator;", "Ljava/lang/Cloneable;", OBJ],
        "Ljava/text/ParsePosition;" | "Ljava/text/FieldPosition;" => &[OBJ],
        // ----- java.io tree
        "Ljava/io/Closeable;" => &["Ljava/lang/AutoCloseable;", OBJ],
        "Ljava/lang/AutoCloseable;" => &[OBJ],
        "Ljava/io/Flushable;" | "Ljava/io/Readable;" => &[OBJ],
        "Ljava/io/InputStream;" => &["Ljava/io/Closeable;", OBJ],
        "Ljava/io/OutputStream;" => &["Ljava/io/Closeable;", "Ljava/io/Flushable;", OBJ],
        "Ljava/io/FilterInputStream;" => &["Ljava/io/InputStream;", OBJ],
        "Ljava/io/FilterOutputStream;" => &["Ljava/io/OutputStream;", OBJ],
        "Ljava/io/BufferedInputStream;" => {
            &["Ljava/io/FilterInputStream;", "Ljava/io/InputStream;", OBJ]
        }
        "Ljava/io/BufferedOutputStream;" => &[
            "Ljava/io/FilterOutputStream;",
            "Ljava/io/OutputStream;",
            OBJ,
        ],
        "Ljava/io/DataInputStream;" => &[
            "Ljava/io/FilterInputStream;",
            "Ljava/io/DataInput;",
            "Ljava/io/InputStream;",
            OBJ,
        ],
        "Ljava/io/DataOutputStream;" => &[
            "Ljava/io/FilterOutputStream;",
            "Ljava/io/DataOutput;",
            "Ljava/io/OutputStream;",
            OBJ,
        ],
        "Ljava/io/DataInput;" | "Ljava/io/DataOutput;" => &[OBJ],
        "Ljava/io/ByteArrayInputStream;" => &["Ljava/io/InputStream;", OBJ],
        "Ljava/io/ByteArrayOutputStream;" => &["Ljava/io/OutputStream;", OBJ],
        "Ljava/io/FileInputStream;" | "Ljava/io/FileDescriptor;" => &["Ljava/io/InputStream;", OBJ],
        "Ljava/io/FileOutputStream;" => &["Ljava/io/OutputStream;", OBJ],
        "Ljava/io/Reader;" => &["Ljava/lang/Readable;", "Ljava/io/Closeable;", OBJ],
        "Ljava/io/Writer;" => &[
            "Ljava/lang/Appendable;",
            "Ljava/io/Closeable;",
            "Ljava/io/Flushable;",
            OBJ,
        ],
        "Ljava/io/BufferedReader;" => &["Ljava/io/Reader;", OBJ],
        "Ljava/io/BufferedWriter;" => &["Ljava/io/Writer;", OBJ],
        "Ljava/io/InputStreamReader;" => &["Ljava/io/Reader;", OBJ],
        "Ljava/io/OutputStreamWriter;" => &["Ljava/io/Writer;", OBJ],
        "Ljava/io/StringReader;" => &["Ljava/io/Reader;", OBJ],
        "Ljava/io/StringWriter;" => &["Ljava/io/Writer;", OBJ],
        "Ljava/io/CharArrayReader;" => &["Ljava/io/Reader;", OBJ],
        "Ljava/io/CharArrayWriter;" => &["Ljava/io/Writer;", OBJ],
        "Ljava/io/PrintWriter;" => &["Ljava/io/Writer;", OBJ],
        "Ljava/io/PrintStream;" => &[
            "Ljava/io/FilterOutputStream;",
            "Ljava/io/OutputStream;",
            "Ljava/lang/Appendable;",
            "Ljava/io/Closeable;",
            OBJ,
        ],
        "Ljava/io/FilterReader;" | "Ljava/io/PushbackReader;" => &["Ljava/io/Reader;", OBJ],
        "Ljava/io/PushbackInputStream;" => {
            &["Ljava/io/FilterInputStream;", "Ljava/io/InputStream;", OBJ]
        }
        "Ljava/io/File;" => &["Ljava/io/Serializable;", "Ljava/lang/Comparable;", OBJ],
        "Ljava/io/FileFilter;" | "Ljava/io/FilenameFilter;" => &[OBJ],
        // ----- java.net / nio
        "Ljava/net/URL;" | "Ljava/net/URI;" => &["Ljava/io/Serializable;", OBJ],
        "Ljava/net/URLConnection;" => &[OBJ],
        "Ljava/net/HttpURLConnection;" => &["Ljava/net/URLConnection;", OBJ],
        "Ljava/net/URLEncoder;" | "Ljava/net/URLDecoder;" => &[OBJ],
        "Ljava/net/InetAddress;"
        | "Ljava/net/Socket;"
        | "Ljava/net/ServerSocket;"
        | "Ljava/net/SocketAddress;"
        | "Ljava/net/Proxy;" => &[OBJ],
        "Ljava/net/InetSocketAddress;" => &["Ljava/net/SocketAddress;", OBJ],
        "Ljava/nio/Buffer;" => &[OBJ],
        "Ljava/nio/ByteBuffer;" => &["Ljava/nio/Buffer;", "Ljava/lang/Comparable;", OBJ],
        "Ljava/nio/CharBuffer;" => &[
            "Ljava/nio/Buffer;",
            "Ljava/lang/CharSequence;",
            "Ljava/lang/Appendable;",
            "Ljava/lang/Readable;",
            OBJ,
        ],
        "Ljava/nio/charset/Charset;" => &["Ljava/lang/Comparable;", OBJ],
        "Ljava/nio/charset/StandardCharsets;"
        | "Ljava/nio/charset/CharsetEncoder;"
        | "Ljava/nio/charset/CharsetDecoder;"
        | "Ljava/nio/ByteOrder;" => &[OBJ],
        // ----- throwable tree
        "Ljava/lang/Exception;" => &[THROWABLE, "Ljava/io/Serializable;", OBJ],
        "Ljava/lang/RuntimeException;" => &[
            "Ljava/lang/Exception;",
            THROWABLE,
            "Ljava/io/Serializable;",
            OBJ,
        ],
        "Ljava/lang/Error;" => &[THROWABLE, "Ljava/io/Serializable;", OBJ],
        // ----- android.os
        "Landroid/os/BaseBundle;" => &[OBJ],
        "Landroid/os/Bundle;" => &[
            "Landroid/os/BaseBundle;",
            "Landroid/os/Parcelable;",
            "Ljava/lang/Cloneable;",
            OBJ,
        ],
        "Landroid/os/PersistableBundle;" => {
            &["Landroid/os/BaseBundle;", "Landroid/os/Parcelable;", OBJ]
        }
        "Landroid/os/Parcelable;" | "Landroid/os/Parcel;" | "Landroid/os/IBinder;" => &[OBJ],
        "Landroid/os/Binder;" => &["Landroid/os/IBinder;", OBJ],
        "Landroid/os/Looper;"
        | "Landroid/os/Handler;"
        | "Landroid/os/Message;"
        | "Landroid/os/Messenger;"
        | "Landroid/os/MessageQueue;"
        | "Landroid/os/AsyncTask;"
        | "Landroid/os/Trace;"
        | "Landroid/os/SystemClock;"
        | "Landroid/os/Process;"
        | "Landroid/os/Build;"
        | "Landroid/os/Build$VERSION;"
        | "Landroid/os/Build$VERSION_CODES;"
        | "Landroid/os/PowerManager;"
        | "Landroid/os/Vibrator;"
        | "Landroid/os/VibrationEffect;"
        | "Landroid/os/ResultReceiver;"
        | "Landroid/os/StrictMode;"
        | "Landroid/os/DeadObjectException;"
        | "Landroid/os/Environment;"
        | "Landroid/os/Debug;"
        | "Landroid/os/BatteryManager;"
        | "Landroid/os/CancellationSignal;"
        | "Landroid/os/UserHandle;"
        | "Landroid/os/UserManager;"
        | "Landroid/os/PatternMatcher;"
        | "Landroid/os/StatFs;" => &[OBJ],
        "Landroid/os/HandlerThread;" => &["Ljava/lang/Thread;", OBJ],
        // ----- android.content tree
        "Landroid/content/Context;" => &[OBJ],
        "Landroid/content/ContextWrapper;" => &["Landroid/content/Context;", OBJ],
        "Landroid/content/ContextThemeWrapper;" => &[
            "Landroid/content/ContextWrapper;",
            "Landroid/content/Context;",
            OBJ,
        ],
        "Landroid/app/Activity;" => &[
            "Landroid/content/ContextThemeWrapper;",
            "Landroid/content/ContextWrapper;",
            "Landroid/content/Context;",
            OBJ,
        ],
        "Landroid/app/Application;" => &[
            "Landroid/content/ContextWrapper;",
            "Landroid/content/Context;",
            OBJ,
        ],
        "Landroid/app/Service;" => &[
            "Landroid/content/ContextWrapper;",
            "Landroid/content/Context;",
            OBJ,
        ],
        "Landroid/app/IntentService;" => &["Landroid/app/Service;", OBJ],
        "Landroid/content/Intent;" => &["Landroid/os/Parcelable;", "Ljava/lang/Cloneable;", OBJ],
        "Landroid/content/IntentFilter;" => &["Landroid/os/Parcelable;", OBJ],
        "Landroid/content/ComponentName;" => &[
            "Landroid/os/Parcelable;",
            "Ljava/lang/Cloneable;",
            "Ljava/lang/Comparable;",
            OBJ,
        ],
        "Landroid/content/BroadcastReceiver;" => &[OBJ],
        "Landroid/content/ContentResolver;"
        | "Landroid/content/ContentProvider;"
        | "Landroid/content/ClipboardManager;"
        | "Landroid/content/ClipData;"
        | "Landroid/content/DialogInterface;"
        | "Landroid/content/SharedPreferences;"
        | "Landroid/content/SharedPreferences$Editor;" => &[OBJ],
        "Landroid/content/ContentValues;" => &["Landroid/os/Parcelable;", OBJ],
        "Landroid/content/pm/PackageManager;"
        | "Landroid/content/pm/PackageInfo;"
        | "Landroid/content/pm/ApplicationInfo;"
        | "Landroid/content/pm/ActivityInfo;"
        | "Landroid/content/pm/ResolveInfo;"
        | "Landroid/content/pm/ServiceInfo;"
        | "Landroid/content/pm/ProviderInfo;" => &[OBJ],
        // ----- android.app
        "Landroid/app/Dialog;" => &[OBJ],
        "Landroid/app/AlertDialog;" => &["Landroid/app/Dialog;", OBJ],
        "Landroid/app/ProgressDialog;" => {
            &["Landroid/app/AlertDialog;", "Landroid/app/Dialog;", OBJ]
        }
        "Landroid/app/AlertDialog$Builder;" => &[OBJ],
        "Landroid/app/Fragment;"
        | "Landroid/app/FragmentManager;"
        | "Landroid/app/LoaderManager;"
        | "Landroid/app/PendingIntent;"
        | "Landroid/app/ActivityManager;"
        | "Landroid/app/NotificationManager;"
        | "Landroid/app/Notification;"
        | "Landroid/app/Notification$Builder;"
        | "Landroid/app/NotificationChannel;"
        | "Landroid/app/NotificationChannelGroup;"
        | "Landroid/app/AlarmManager;"
        | "Landroid/app/KeyguardManager;"
        | "Landroid/app/SearchManager;"
        | "Landroid/app/DownloadManager;"
        | "Landroid/app/UiModeManager;"
        | "Landroid/app/WallpaperManager;"
        | "Landroid/app/TaskStackBuilder;"
        | "Landroid/app/Instrumentation;"
        | "Landroid/app/ActivityOptions;"
        | "Landroid/app/PictureInPictureParams;"
        | "Landroid/app/ActivityManager$RunningAppProcessInfo;"
        | "Landroid/app/ActivityManager$MemoryInfo;"
        | "Landroid/app/role/RoleManager;" => &[OBJ],
        // ----- android.view tree
        "Landroid/view/View;" => &[OBJ],
        "Landroid/view/ViewGroup;" => &[
            "Landroid/view/View;",
            "Landroid/view/ViewParent;",
            "Landroid/view/ViewManager;",
            OBJ,
        ],
        "Landroid/view/ViewParent;" | "Landroid/view/ViewManager;" => &[OBJ],
        "Landroid/widget/LinearLayout;" => {
            &["Landroid/view/ViewGroup;", "Landroid/view/View;", OBJ]
        }
        "Landroid/widget/FrameLayout;"
        | "Landroid/widget/RelativeLayout;"
        | "Landroid/widget/AbsoluteLayout;"
        | "Landroid/widget/GridLayout;" => {
            &["Landroid/view/ViewGroup;", "Landroid/view/View;", OBJ]
        }
        // AOSP: TableLayout extends LinearLayout (always vertical),
        // TableRow extends LinearLayout (always horizontal).
        "Landroid/widget/TableLayout;" | "Landroid/widget/TableRow;" => &[
            "Landroid/widget/LinearLayout;",
            "Landroid/view/ViewGroup;",
            "Landroid/view/View;",
            OBJ,
        ],
        "Landroid/widget/ScrollView;" | "Landroid/widget/HorizontalScrollView;" => &[
            "Landroid/widget/FrameLayout;",
            "Landroid/view/ViewGroup;",
            "Landroid/view/View;",
            OBJ,
        ],
        "Landroid/widget/TextView;" => &["Landroid/view/View;", OBJ],
        "Landroid/widget/Button;" | "Landroid/widget/EditText;" => {
            &["Landroid/widget/TextView;", "Landroid/view/View;", OBJ]
        }
        "Landroid/widget/ImageView;" => &["Landroid/view/View;", OBJ],
        "Landroid/widget/ImageButton;" => {
            &["Landroid/widget/ImageView;", "Landroid/view/View;", OBJ]
        }
        "Landroid/widget/CheckBox;" | "Landroid/widget/RadioButton;" => {
            &["Landroid/widget/Button;", "Landroid/widget/TextView;", OBJ]
        }
        "Landroid/widget/CompoundButton;" => {
            &["Landroid/widget/Button;", "Landroid/widget/TextView;", OBJ]
        }
        "Landroid/widget/Switch;" => &[
            "Landroid/widget/CompoundButton;",
            "Landroid/widget/Button;",
            "Landroid/widget/TextView;",
            OBJ,
        ],
        "Landroid/widget/ProgressBar;" => &["Landroid/view/View;", OBJ],
        "Landroid/widget/SeekBar;" | "Landroid/widget/RatingBar;" => {
            &["Landroid/widget/ProgressBar;", "Landroid/view/View;", OBJ]
        }
        "Landroid/widget/AbsListView;" => &[
            "Landroid/widget/AdapterView;",
            "Landroid/view/ViewGroup;",
            OBJ,
        ],
        "Landroid/widget/ListView;" | "Landroid/widget/GridView;" => &[
            "Landroid/widget/AbsListView;",
            "Landroid/widget/AdapterView;",
            "Landroid/view/ViewGroup;",
            OBJ,
        ],
        "Landroid/widget/AdapterView;" => &["Landroid/view/ViewGroup;", "Landroid/view/View;", OBJ],
        "Landroid/widget/Spinner;" | "Landroid/widget/AbsSpinner;" => &[
            "Landroid/widget/AdapterView;",
            "Landroid/view/ViewGroup;",
            OBJ,
        ],
        "Landroid/widget/Adapter;"
        | "Landroid/widget/ListAdapter;"
        | "Landroid/widget/SpinnerAdapter;" => &[OBJ],
        "Landroid/widget/BaseAdapter;" => &[
            "Landroid/widget/ListAdapter;",
            "Landroid/widget/SpinnerAdapter;",
            "Landroid/widget/Adapter;",
            OBJ,
        ],
        "Landroid/widget/ArrayAdapter;" => &["Landroid/widget/BaseAdapter;", OBJ],
        // TextClock/Chronometer/DigitalClock are TextViews — guest
        // widgets (MyTextClock) extend them and must check-cast to View.
        "Landroid/widget/TextClock;"
        | "Landroid/widget/Chronometer;"
        | "Landroid/widget/DigitalClock;" => {
            &["Landroid/widget/TextView;", "Landroid/view/View;", OBJ]
        }
        "Landroid/widget/Space;" => &["Landroid/view/View;", OBJ],
        "Landroid/widget/VideoView;" => &["Landroid/view/SurfaceView;", "Landroid/view/View;", OBJ],
        "Landroid/widget/Toast;" => &[OBJ],
        "Landroid/view/ViewStub;" | "Landroid/view/SurfaceView;" | "Landroid/view/TextureView;" => {
            &["Landroid/view/View;", OBJ]
        }
        "Landroid/webkit/WebView;" => &[
            "Landroid/widget/AbsoluteLayout;",
            "Landroid/view/ViewGroup;",
            "Landroid/view/View;",
            OBJ,
        ],
        "Landroid/view/Window;"
        | "Landroid/view/WindowManager;"
        | "Landroid/view/WindowManager$LayoutParams;"
        | "Landroid/view/Display;"
        | "Landroid/view/Gravity;"
        | "Landroid/view/Menu;"
        | "Landroid/view/MenuItem;"
        | "Landroid/view/SubMenu;"
        | "Landroid/view/ContextMenu;"
        | "Landroid/view/MenuInflater;"
        | "Landroid/view/LayoutInflater;"
        | "Landroid/view/ViewTreeObserver;"
        | "Landroid/view/GestureDetector;"
        | "Landroid/view/ScaleGestureDetector;"
        | "Landroid/view/ViewConfiguration;"
        | "Landroid/view/VelocityTracker;"
        | "Landroid/view/InflateException;"
        | "Landroid/view/Surface;"
        | "Landroid/view/SurfaceHolder;"
        | "Landroid/view/Choreographer;"
        | "Landroid/view/View$OnClickListener;"
        | "Landroid/view/View$OnTouchListener;"
        | "Landroid/view/View$OnLongClickListener;"
        | "Landroid/view/animation/Animation;"
        | "Landroid/view/animation/AnimationUtils;"
        | "Landroid/view/animation/Interpolator;"
        | "Landroid/view/ViewPropertyAnimator;"
        | "Landroid/animation/ValueAnimator;"
        | "Landroid/animation/ObjectAnimator;"
        | "Landroid/animation/Animator;"
        | "Landroid/animation/AnimatorSet;"
        | "Landroid/animation/TimeInterpolator;"
        | "Landroid/animation/ArgbEvaluator;"
        | "Landroid/animation/AnimatorListenerAdapter;" => &[OBJ],
        "Landroid/view/MotionEvent;" | "Landroid/view/KeyEvent;" => {
            &["Landroid/view/InputEvent;", OBJ]
        }
        "Landroid/view/InputEvent;" => &[OBJ],
        // ----- android.widget remaining
        "Landroid/widget/PopupWindow;"
        | "Landroid/widget/PopupMenu;"
        | "Landroid/widget/AutoCompleteTextView;"
        | "Landroid/widget/MultiAutoCompleteTextView;"
        | "Landroid/widget/CheckedTextView;"
        | "Landroid/widget/CalendarView;"
        | "Landroid/widget/DatePicker;"
        | "Landroid/widget/TimePicker;"
        | "Landroid/widget/NumberPicker;"
        | "Landroid/widget/SearchView;"
        | "Landroid/widget/TabHost;"
        | "Landroid/widget/TabWidget;"
        | "Landroid/widget/AnalogClock;"
        | "Landroid/widget/ExpandableListView;"
        | "Landroid/widget/QuickContactBadge;"
        | "Landroid/widget/RadioGroup;"
        | "Landroid/widget/SimpleAdapter;"
        | "Landroid/widget/CursorAdapter;"
        | "Landroid/widget/ResourceCursorAdapter;"
        | "Landroid/widget/SimpleCursorAdapter;"
        | "Landroid/widget/Scroller;"
        | "Landroid/widget/OverScroller;"
        | "Landroid/widget/EdgeEffect;"
        | "Landroid/widget/ZoomControls;"
        | "Landroid/widget/Gallery;"
        | "Landroid/widget/Filter;"
        | "Landroid/widget/Filterable;"
        | "Landroid/widget/AdapterViewFlipper;" => &[OBJ],
        // ----- android.util
        "Landroid/util/Log;"
        | "Landroid/util/Pair;"
        | "Landroid/util/AttributeSet;"
        | "Landroid/util/Xml;"
        | "Landroid/util/Base64;"
        | "Landroid/util/TypedValue;"
        | "Landroid/util/LruCache;"
        | "Landroid/util/Patterns;"
        | "Landroid/util/JsonReader;"
        | "Landroid/util/JsonWriter;"
        | "Landroid/util/MalformedJsonException;"
        | "Landroid/util/Size;"
        | "Landroid/util/SizeF;"
        | "Landroid/util/Rational;"
        | "Landroid/util/Range;"
        | "Landroid/util/LogPrinter;" => &[OBJ],
        "Landroid/util/ArraySet;" => &[
            "Ljava/util/Set;",
            "Ljava/util/Collection;",
            "Ljava/lang/Iterable;",
            OBJ,
        ],
        "Landroid/util/ArrayMap;" => &["Ljava/util/Map;", OBJ],
        "Landroid/util/DisplayMetrics;" => &["Landroid/os/Parcelable;", OBJ],
        // ----- android.text
        "Landroid/text/TextUtils;"
        | "Landroid/text/SpannableStringInternal;"
        | "Landroid/text/TextWatcher;"
        | "Landroid/text/InputFilter;"
        | "Landroid/text/Html;"
        | "Landroid/text/Layout;"
        | "Landroid/text/StaticLayout;"
        | "Landroid/text/Selection;"
        | "Landroid/text/method/TransformationMethod;"
        | "Landroid/text/GetChars;"
        | "Landroid/text/NoCopySpan;" => &[OBJ],
        "Landroid/text/Spanned;" => &["Ljava/lang/CharSequence;", OBJ],
        "Landroid/text/Spannable;" => &["Landroid/text/Spanned;", "Ljava/lang/CharSequence;", OBJ],
        "Landroid/text/Editable;" => &[
            "Landroid/text/Spannable;",
            "Landroid/text/Spanned;",
            "Ljava/lang/CharSequence;",
            "Ljava/lang/Appendable;",
            OBJ,
        ],
        "Landroid/text/SpannableString;" | "Landroid/text/SpannedString;" => {
            &["Landroid/text/Spanned;", "Ljava/lang/CharSequence;", OBJ]
        }
        "Landroid/text/SpannableStringBuilder;" => &[
            "Landroid/text/SpannableStringInternal;",
            "Landroid/text/Editable;",
            "Landroid/text/Spannable;",
            "Ljava/lang/CharSequence;",
            "Ljava/lang/Appendable;",
            OBJ,
        ],
        // ----- android.graphics
        "Landroid/graphics/Paint;"
        | "Landroid/graphics/Canvas;"
        | "Landroid/graphics/Bitmap;"
        | "Landroid/graphics/BitmapFactory;"
        | "Landroid/graphics/Bitmap$Config;"
        | "Landroid/graphics/Color;"
        | "Landroid/graphics/ColorFilter;"
        | "Landroid/graphics/PorterDuff;"
        | "Landroid/graphics/PorterDuff$Mode;"
        | "Landroid/graphics/Xfermode;"
        | "Landroid/graphics/PorterDuffXfermode;"
        | "Landroid/graphics/Matrix;"
        | "Landroid/graphics/Path;"
        | "Landroid/graphics/Region;"
        | "Landroid/graphics/Shader;"
        | "Landroid/graphics/LinearGradient;"
        | "Landroid/graphics/RadialGradient;"
        | "Landroid/graphics/SweepGradient;"
        | "Landroid/graphics/BitmapShader;"
        | "Landroid/graphics/Typeface;"
        | "Landroid/graphics/Point;"
        | "Landroid/graphics/PointF;"
        | "Landroid/graphics/Rect;"
        | "Landroid/graphics/RectF;"
        | "Landroid/graphics/Camera;"
        | "Landroid/graphics/NinePatch;"
        | "Landroid/graphics/Picture;"
        | "Landroid/graphics/ImageDecoder;"
        | "Landroid/graphics/Movie;"
        | "Landroid/graphics/PathMeasure;"
        | "Landroid/graphics/BlurMaskFilter;"
        | "Landroid/graphics/Paint$Style;"
        | "Landroid/graphics/Paint$Align;"
        | "Landroid/graphics/Paint$Cap;"
        | "Landroid/graphics/Paint$Join;"
        | "Landroid/graphics/Paint$FontMetrics;"
        | "Landroid/graphics/drawable/Drawable;"
        | "Landroid/graphics/drawable/Drawable$Callback;"
        | "Landroid/graphics/drawable/PaintDrawable;"
        | "Landroid/graphics/drawable/shapes/Shape;"
        | "Landroid/graphics/drawable/shapes/RectShape;"
        | "Landroid/graphics/drawable/shapes/OvalShape;"
        | "Landroid/graphics/drawable/shapes/RoundRectShape;" => &[OBJ],
        "Landroid/graphics/drawable/VectorDrawable;"
        | "Landroid/graphics/drawable/AnimatedVectorDrawable;"
        | "Landroid/graphics/drawable/ColorDrawable;" => {
            &["Landroid/graphics/drawable/Drawable;", OBJ]
        }
        "Landroid/graphics/drawable/BitmapDrawable;" => {
            &["Landroid/graphics/drawable/Drawable;", OBJ]
        }
        "Landroid/graphics/drawable/GradientDrawable;" => {
            &["Landroid/graphics/drawable/Drawable;", OBJ]
        }
        "Landroid/graphics/drawable/ShapeDrawable;" => {
            &["Landroid/graphics/drawable/Drawable;", OBJ]
        }
        "Landroid/graphics/drawable/DrawableContainer;" => {
            &["Landroid/graphics/drawable/Drawable;", OBJ]
        }
        "Landroid/graphics/drawable/StateListDrawable;"
        | "Landroid/graphics/drawable/LevelListDrawable;"
        | "Landroid/graphics/drawable/LayerDrawable;"
        | "Landroid/graphics/drawable/AnimationDrawable;" => &[
            "Landroid/graphics/drawable/DrawableContainer;",
            "Landroid/graphics/drawable/Drawable;",
            OBJ,
        ],
        "Landroid/graphics/drawable/ClipDrawable;"
        | "Landroid/graphics/drawable/InsetDrawable;"
        | "Landroid/graphics/drawable/RotateDrawable;"
        | "Landroid/graphics/drawable/ScaleDrawable;"
        | "Landroid/graphics/drawable/RippleDrawable;"
        | "Landroid/graphics/drawable/NinePatchDrawable;" => {
            &["Landroid/graphics/drawable/Drawable;", OBJ]
        }
        // ----- android.content.res
        "Landroid/content/res/Resources;"
        | "Landroid/content/res/AssetManager;"
        | "Landroid/content/res/Configuration;"
        | "Landroid/content/res/TypedArray;"
        | "Landroid/content/res/Resources$Theme;"
        | "Landroid/content/res/ColorStateList;"
        | "Landroid/content/res/AssetFileDescriptor;" => &[OBJ],
        "Landroid/content/res/XmlResourceParser;" => &[
            "Lorg/xmlpull/v1/XmlPullParser;",
            "Landroid/util/AttributeSet;",
            "Ljava/lang/AutoCloseable;",
            "Ljava/io/Closeable;",
            OBJ,
        ],
        // AOSP: NotFoundException extends RuntimeException.
        "Landroid/content/res/Resources$NotFoundException;" => &[
            "Ljava/lang/RuntimeException;",
            "Ljava/lang/Exception;",
            THROWABLE,
            OBJ,
        ],
        // ----- android.net
        "Landroid/net/Uri;" => &["Landroid/os/Parcelable;", "Ljava/lang/Comparable;", OBJ],
        "Landroid/net/Uri$Builder;" => &[OBJ],
        "Landroid/net/ConnectivityManager;"
        | "Landroid/net/NetworkInfo;"
        | "Landroid/net/Network;"
        | "Landroid/net/NetworkCapabilities;"
        | "Landroid/net/LinkProperties;"
        | "Landroid/net/NetworkRequest;"
        | "Landroid/net/Proxy;"
        | "Landroid/net/wifi/WifiManager;"
        | "Landroid/net/wifi/WifiInfo;"
        | "Landroid/net/LocalSocket;" => &[OBJ],
        // ----- android.media / hardware
        "Landroid/media/MediaPlayer;"
        | "Landroid/media/MediaRecorder;"
        | "Landroid/media/AudioManager;"
        | "Landroid/media/SoundPool;"
        | "Landroid/media/ToneGenerator;"
        | "Landroid/media/MediaMetadataRetriever;"
        | "Landroid/media/AudioTrack;"
        | "Landroid/media/AudioRecord;"
        | "Landroid/media/Image;"
        | "Landroid/hardware/Camera;"
        | "Landroid/hardware/camera2/CameraManager;"
        | "Landroid/hardware/Sensor;"
        | "Landroid/hardware/SensorManager;"
        | "Landroid/hardware/SensorEvent;"
        | "Landroid/hardware/display/DisplayManager;"
        | "Landroid/bluetooth/BluetoothAdapter;"
        | "Landroid/bluetooth/BluetoothDevice;"
        | "Landroid/bluetooth/BluetoothSocket;"
        | "Landroid/location/Location;"
        | "Landroid/location/LocationManager;"
        | "Landroid/location/LocationListener;"
        | "Landroid/telephony/TelephonyManager;"
        | "Landroid/nfc/NfcAdapter;" => &[OBJ],
        // ----- android.database / provider
        // `Cursor`/`SQLiteClosable` implement `Closeable` — kotlin's
        // `use`-extension close helpers reject anything else.
        "Landroid/database/Cursor;"
        | "Landroid/database/MatrixCursor;"
        | "Landroid/database/CursorWrapper;"
        | "Landroid/database/sqlite/SQLiteDatabase;"
        | "Landroid/database/sqlite/SQLiteOpenHelper;"
        | "Landroid/database/sqlite/SQLiteStatement;"
        | "Landroid/database/sqlite/SQLiteProgram;"
        | "Landroid/database/sqlite/SQLiteQuery;"
        | "Landroid/database/sqlite/SQLiteClosable;" => {
            &["Ljava/io/Closeable;", "Ljava/lang/AutoCloseable;", OBJ]
        }
        "Landroid/database/DatabaseUtils;"
        | "Landroid/database/ContentObserver;"
        | "Landroid/database/DataSetObserver;"
        | "Landroid/database/sqlite/SQLiteQueryBuilder;"
        | "Landroid/database/CursorIndexOutOfBoundsException;" => &[OBJ],
        // android.database exceptions — SQLite*Exception extends
        // SQLException extends RuntimeException (AOSP).
        "Landroid/database/SQLException;" => &[
            "Ljava/lang/RuntimeException;",
            "Ljava/lang/Exception;",
            THROWABLE,
            OBJ,
        ],
        "Landroid/database/sqlite/SQLiteException;"
        | "Landroid/database/sqlite/SQLiteDatabaseCorruptException;"
        | "Landroid/database/sqlite/SQLiteConstraintException;"
        | "Landroid/database/sqlite/SQLiteDiskIOException;"
        | "Landroid/database/sqlite/SQLiteFullException;"
        | "Landroid/database/sqlite/SQLiteReadOnlyDatabaseException;"
        | "Landroid/database/sqlite/SQLiteTableLockedException;"
        | "Landroid/database/sqlite/SQLiteDatabaseLockedException;"
        | "Landroid/database/sqlite/SQLiteAbortException;"
        | "Landroid/database/sqlite/SQLiteAccessPermException;"
        | "Landroid/database/sqlite/SQLiteBindOrColumnIndexOutOfRangeException;"
        | "Landroid/database/sqlite/SQLiteBlobTooBigException;"
        | "Landroid/database/sqlite/SQLiteCantOpenDatabaseException;"
        | "Landroid/database/sqlite/SQLiteDoneException;"
        | "Landroid/database/sqlite/SQLiteMisuseException;"
        | "Landroid/database/sqlite/SQLiteOutOfMemoryException;"
        | "Landroid/database/StaleDataException;"
        | "Landroid/database/CursorWindowAllocationException;" => &[
            "Landroid/database/SQLException;",
            "Ljava/lang/RuntimeException;",
            "Ljava/lang/Exception;",
            THROWABLE,
            OBJ,
        ],
        "Landroid/provider/Settings;"
        | "Landroid/provider/Settings$System;"
        | "Landroid/provider/Settings$Secure;"
        | "Landroid/provider/Settings$Global;"
        | "Landroid/provider/MediaStore;"
        | "Landroid/provider/ContactsContract;"
        | "Landroid/provider/CalendarContract;"
        | "Landroid/provider/DocumentsContract;"
        | "Landroid/provider/OpenableColumns;"
        | "Landroid/provider/Browser;" => &[OBJ],
        // ----- android.webkit / opengl / renderscript
        "Landroid/webkit/WebSettings;"
        | "Landroid/webkit/WebViewClient;"
        | "Landroid/webkit/WebChromeClient;"
        | "Landroid/webkit/CookieManager;"
        | "Landroid/webkit/JavascriptInterface;"
        | "Landroid/webkit/WebResourceRequest;"
        | "Landroid/webkit/WebResourceResponse;"
        | "Landroid/webkit/MimeTypeMap;"
        | "Landroid/webkit/URLUtil;"
        | "Landroid/webkit/ValueCallback;"
        | "Landroid/opengl/Matrix;"
        | "Landroid/opengl/GLUtils;"
        | "Landroid/opengl/EGL14;"
        | "Landroid/opengl/GLES20;"
        | "Landroid/opengl/GLES30;" => &[OBJ],
        "Landroid/opengl/GLSurfaceView;" => {
            &["Landroid/view/SurfaceView;", "Landroid/view/View;", OBJ]
        }
        // ----- androidx (treated as host-model classes too)
        "Landroidx/appcompat/app/AppCompatActivity;" => &[
            "Landroidx/fragment/app/FragmentActivity;",
            "Landroidx/activity/ComponentActivity;",
            "Landroid/app/Activity;",
            "Landroid/content/ContextThemeWrapper;",
            "Landroid/content/ContextWrapper;",
            "Landroid/content/Context;",
            OBJ,
        ],
        "Landroidx/fragment/app/FragmentActivity;" => &[
            "Landroidx/activity/ComponentActivity;",
            "Landroid/app/Activity;",
            OBJ,
        ],
        "Landroidx/activity/ComponentActivity;" => &["Landroid/app/Activity;", OBJ],
        "Landroidx/recyclerview/widget/RecyclerView;" => {
            &["Landroid/view/ViewGroup;", "Landroid/view/View;", OBJ]
        }
        "Landroidx/viewpager/widget/ViewPager;" => {
            &["Landroid/view/ViewGroup;", "Landroid/view/View;", OBJ]
        }
        "Landroidx/fragment/app/Fragment;"
        | "Landroidx/fragment/app/FragmentManager;"
        | "Landroidx/appcompat/app/AlertDialog$Builder;"
        | "Landroidx/appcompat/widget/Toolbar;"
        | "Landroidx/core/app/NotificationCompat;"
        | "Landroidx/core/content/ContextCompat;"
        | "Landroidx/core/app/ActivityCompat;"
        | "Landroidx/lifecycle/ViewModel;"
        | "Landroidx/lifecycle/LiveData;"
        | "Landroidx/lifecycle/MutableLiveData;"
        | "Landroidx/lifecycle/ViewModelProvider;"
        | "Landroidx/preference/PreferenceManager;"
        | "Landroidx/work/WorkManager;"
        | "Landroidx/core/util/Pair;"
        | "Landroidx/collection/ArrayMap;"
        | "Landroidx/collection/ArraySet;"
        | "Landroidx/collection/SparseArrayCompat;"
        | "Landroidx/core/app/NotificationManagerCompat;"
        | "Landroidx/localbroadcastmanager/content/LocalBroadcastManager;" => &[OBJ],
        // ----- kotlin stdlib (host-modeled)
        "Lkotlin/Unit;"
        | "Lkotlin/Pair;"
        | "Lkotlin/Triple;"
        | "Lkotlin/KotlinNullPointerException;"
        | "Lkotlin/jvm/internal/Intrinsics;"
        | "Lkotlin/jvm/internal/DefaultConstructorMarker;"
        | "Lkotlin/jvm/internal/Ref$ObjectRef;"
        | "Lkotlin/jvm/internal/Ref$IntRef;"
        | "Lkotlin/jvm/internal/Ref$LongRef;"
        | "Lkotlin/jvm/internal/Ref$BooleanRef;"
        | "Lkotlin/jvm/internal/Ref$ByteRef;"
        | "Lkotlin/jvm/internal/Ref$ShortRef;"
        | "Lkotlin/jvm/internal/Ref$FloatRef;"
        | "Lkotlin/jvm/internal/Ref$DoubleRef;"
        | "Lkotlin/jvm/internal/Ref$CharRef;"
        | "Lkotlin/text/StringsKt;"
        | "Lkotlin/collections/CollectionsKt;"
        | "Lkotlin/collections/ArraysKt;"
        | "Lkotlin/ExceptionsKt;" => &[OBJ],
        "Lkotlin/jvm/internal/Lambda;" => &["Ljava/lang/Object;"],
        // ----- org.json
        "Lorg/json/JSONObject;"
        | "Lorg/json/JSONArray;"
        | "Lorg/json/JSONTokener;"
        | "Lorg/json/JSONStringer;"
        | "Lorg/json/JSONObject$Null;" => &[OBJ],
        // ----- android.window
        "Landroid/window/OnBackAnimationCallback;" => &["Landroid/window/OnBackInvokedCallback;"],
        "Landroid/window/OnBackInvokedCallback;" => &[OBJ],
        // ----- misc
        "Ldalvik/system/DexClassLoader;"
        | "Ldalvik/system/PathClassLoader;"
        | "Ldalvik/system/BaseDexClassLoader;" => &["Ljava/lang/ClassLoader;", OBJ],
        "Ljava/lang/ref/Reference;" => &[OBJ],
        "Ljava/lang/ref/WeakReference;"
        | "Ljava/lang/ref/SoftReference;"
        | "Ljava/lang/ref/PhantomReference;" => &["Ljava/lang/ref/Reference;", OBJ],
        "Ljava/security/MessageDigest;"
        | "Ljava/security/KeyStore;"
        | "Ljava/security/Signature;"
        | "Ljava/security/cert/Certificate;"
        | "Ljava/security/NoSuchAlgorithmException;"
        | "Ljavax/crypto/Cipher;"
        | "Ljavax/crypto/Mac;"
        | "Ljavax/crypto/KeyGenerator;"
        | "Ljavax/crypto/SecretKey;"
        | "Ljavax/crypto/SecretKeyFactory;"
        | "Ljavax/crypto/spec/SecretKeySpec;"
        | "Ljavax/crypto/spec/IvParameterSpec;"
        | "Ljavax/crypto/spec/PBEKeySpec;"
        | "Ljavax/crypto/spec/GCMParameterSpec;" => &[OBJ],
        _ => &[],
    }
}

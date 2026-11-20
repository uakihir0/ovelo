//! ovelo-dexrt — DEX interpreter (design §04, P1 scope).
//!
//! Executes verified DEX: typed heap handles, frame stack, int/long/float/
//! double arithmetic with Android's wrapping and trap semantics, arrays with
//! bounds checks, virtual/interface dispatch, `<clinit>` once-per-class, and
//! exception propagation through `try`/`catch`. Single UI-looper thread;
//! monitors are no-ops on one thread. Unimplemented opcodes stop with
//! `DexUnsupported` — never silently.

mod aot;
mod jdk;
pub mod res;
mod res_attrs;
mod value;

pub use aot::Aot;
pub use jdk::view::DexViewInfo;
pub use value::{Heap, HostState, Obj, V};

use aot::{AotFn, TRAP_DIV_ZERO, TRAP_REM_ZERO};

use ovelo_dex::{decode_all, ClassData, ClassDef, DexFile, EncodedMethod, Insn, TryItem};
use ovelo_error::{ErrorCategory, OveloError, Phase, Result};
use std::collections::HashMap;

fn err(cause: &str) -> OveloError {
    OveloError::new(ErrorCategory::DexUnsupported, Phase::DexExec, cause)
}
fn errd(cause: &str, detail: String) -> OveloError {
    OveloError::new(ErrorCategory::DexUnsupported, Phase::DexExec, cause)
        .with_detail(detail)
}

/// Internal control flow: a guest `throw` carries a heap handle, VM faults
/// carry an [`OveloError`].
enum E {
    Vm(OveloError),
    Throw(u32),
}
type Rt<T> = std::result::Result<T, E>;
fn vm<T>(c: &'static str) -> Rt<T> {
    Err(E::Vm(err(c)))
}

/// `(params)ret` descriptor for a proto index local to `d`.
fn proto_sig(d: &DexFile, pidx: u32) -> Result<String> {
    let p = d.proto(pidx)?;
    let mut sig = String::from("(");
    for t in d.proto_params(p)? {
        sig.push_str(&d.type_descriptor(t as u32)?);
    }
    sig.push(')');
    sig.push_str(&d.type_descriptor(p.return_type_idx)?);
    Ok(sig)
}

/// Global index: `(dex_i << 32) | local_idx`. Type/method/field indexes
/// are per-DEX; the VM merges all loaded dexes into one index space so a
/// class defined in classes2.dex resolves from an invoke in classes.dex.
pub(crate) type GIdx = u64;
/// `class_idx`/`type_idx` sentinel for descriptors absent from every dex.
pub(crate) const GX_NONE: GIdx = u64::MAX;
pub(crate) fn gx(di: usize, i: u32) -> GIdx {
    ((di as u64) << 32) | i as u64
}
pub(crate) fn gd(g: GIdx) -> usize {
    (g >> 32) as usize
}
pub(crate) fn gl(g: GIdx) -> u32 {
    g as u32
}

/// Runtime state of one loaded class.
#[derive(Default)]
struct ClassRt {
    /// Class statics by global field index — enum constants live here.
    statics: HashMap<GIdx, V>,
    /// 0 = pending, 1 = running (reentrancy guard), 2 = done.
    clinit: u8,
}

/// A resolved method: where it is declared plus its code.
#[derive(Clone)]
struct Loc {
    /// Owning class (global `type_idx`) the method was found on.
    class_idx: GIdx,
    /// The method's global `method_idx`.
    midx: GIdx,
    /// The method table entry (`method_idx` inside stays dex-local).
    em: EncodedMethod,
}

/// A host-implemented framework method. Registered per `method_idx`;
/// `call_builtin` consults this table before the builtin stubs.
/// Single-threaded. A host callback may itself dispatch guest code that
/// re-enters the *same* registered callback (e.g. an initializer's
/// `create` calling `initializeComponent`) — the dispatcher answers a
/// `dexrt.host.reentrant` error there instead of panicking on the
/// RefCell's second mutable borrow.
pub type HostCall = std::rc::Rc<
    std::cell::RefCell<dyn FnMut(&mut Vm, &[V]) -> Result<Option<V>>>,
>;

/// DEX interpreter over one or more [`DexFile`]s (multi-dex apps).
pub struct Vm {
    dexes: Vec<DexFile>,
    /// Object heap (public for tests / host plumbing).
    pub heap: Heap,
    classes: HashMap<GIdx, ClassRt>,
    /// Global method_idx → defining (global class_idx, EncodedMethod).
    defined: HashMap<GIdx, (GIdx, EncodedMethod)>,
    /// Class descriptor → (dex_i, class_def index) — a type_idx in dex N
    /// may name a class defined in dex M, so defs key off descriptors.
    defs: HashMap<String, (usize, usize)>,
    /// `(owner desc, name, sig)` → global method_idx of the definition —
    /// cross-dex invoke resolution (a ref in dex N, def in dex M).
    methods_by_sig: HashMap<(String, String, String), GIdx>,
    /// `(owner desc, name, type desc)` → first-seen global field_idx —
    /// canonical key for instance/static field storage across dexes.
    fields_by_sig: HashMap<(String, String, String), GIdx>,
    /// Global method_idx → host implementation (framework bridge).
    hosts: HashMap<GIdx, HostCall>,
    /// (name, sig) → candidate host fns with the global class idx they
    /// were registered on; used when the DEX references a framework
    /// method through a subclass descriptor. Only a reference whose
    /// class is the owner or a subclass of it may resolve — see
    /// `host_named`.
    hosts_named: HashMap<(String, String), Vec<(GIdx, HostCall)>>,
    /// AOT-compiled method bodies (`None` until `enable_aot`).
    aot: Option<Aot>,
    /// Package name of the guest app, set by the launcher so
    /// `Context.getPackageName` etc. answer correctly.
    pub guest_package: Option<String>,
    /// Guest-visible filesystem root: `java.io.File` paths resolve
    /// under this directory, never onto the host fs directly.
    pub guest_root: std::path::PathBuf,
    /// Live interpreter call depth — unbounded guest or builtin
    /// recursion would overflow the native stack; cap it instead.
    depth: u32,
    /// Method refs of live frames, for recursion diagnosis.
    stack: Vec<GIdx>,
    /// handle → guest method that `athrow`ed it (for uncaught reports).
    throw_sites: HashMap<u32, String>,
    /// Last framework stub calls (callee only), for uncaught reports —
    /// a crash usually follows a stub that returned a placeholder.
    stub_trace: Vec<String>,
    /// Interned main-thread object — `Thread.currentThread()` and
    /// `Looper.getThread()` must compare equal (androidx checks `==`).
    pub(crate) main_thread_h: u32,
    /// Thread handle `Thread.currentThread()` returns right now — the
    /// worker handle while an executor job runs, main otherwise.
    pub(crate) cur_thread: u32,
    /// The background thread object (see `worker_thread`).
    pub(crate) worker_thread_h: u32,
    /// Interned main `Looper` — `Looper.myLooper()` and
    /// `Handler.getLooper()` must return the same handle.
    pub(crate) main_looper_h: u32,
    /// Interned `Application` — `getApplication[Context]` must return a
    /// real Application-typed object (callers `check-cast` it).
    pub(crate) app_h: u32,
    /// sun.misc.Unsafe model: offset tokens minted by `objectFieldOffset`,
    /// `(object handle, offset)` -> stored value (CAS/volatile get/put).
    pub(crate) unsafe_offsets: HashMap<(u32, i64), V>,
    pub(crate) unsafe_next_offset: i64,
    /// `OVELO_TRACE_FIELD` — debug-only iget/iput tracing for a field
    /// name substring (e.g. `readyAsyncCalls`).
    trace_field: Option<String>,
    /// `OVELO_TRACE_THROW` — log every thrown/caught throwable with the
    /// throwing method and pc (debug aid for swallowed guest errors).
    trace_throw: bool,
    /// `OVELO_TRACE_CALL` — log invoke sites whose resolved method name
    /// contains this substring (debug aid).
    trace_call: Option<String>,
    /// Roots of every inflated view tree — `findViewById` falls back to
    /// them when the receiver's own subtree misses (appcompat decor is
    /// built dex-side, outside our synthetic window decor).
    pub(crate) view_roots: Vec<u32>,
    /// `SharedPreferences` are file-backed: every `getSharedPreferences`
    /// call for the same file must see the same map, so intern the host
    /// objects per file name.
    pub(crate) prefs_files: HashMap<String, u32>,
    /// Activity currently being dispatched — owns any content/decor
    /// roots attached while it runs, so a second activity's trees
    /// never leak into the first's lookups.
    pub(crate) cur_activity: u32,
    /// The view attached under `android.R.id.content` per activity —
    /// the app's real content root no matter which path installed it.
    pub(crate) content_views: HashMap<u32, u32>,
    /// Root of the tree that owns each activity's content frame —
    /// appcompat's real decor when it built one dex-side.
    pub(crate) decor_roots: HashMap<u32, u32>,
    /// Installed-APK resource index for `LayoutInflater.inflate` — `None`
    /// degrades inflation to bare placeholder views.
    pub apk_res: Option<res::ApkResources>,
    /// Guest SQLite connections by database name
    /// (`android.database.sqlite` — see `jdk::db`).
    pub(crate) dbs: HashMap<String, std::rc::Rc<std::cell::RefCell<jdk::db::Db>>>,
    /// Database name -> its `SQLiteDatabase` object handle.
    pub(crate) db_handles: HashMap<String, u32>,
    /// Object handle (db or statement) -> database name it talks to.
    pub(crate) obj_db: HashMap<u32, String>,
    /// The Activity/theme res id (`android:theme` from the manifest) that
    /// theme-scoped `obtainStyledAttributes` resolves attributes against.
    pub theme_id: u32,
    /// Total interpreter steps across all `run()` calls — `OVELO_STALLDBG`
    /// uses it to distinguish a still-running guest from a host-side loop.
    pub total_steps: u64,
    /// `(owner_descriptor, field_name)` → global field idx — memoized
    /// because `DexFile::field_index_of` re-decodes every field's
    /// strings and updater-style guests (`AtomicReferenceFieldUpdater`,
    /// `Unsafe`) issue thousands of lookups.
    field_idx_cache: std::cell::RefCell<HashMap<(String, String), Option<GIdx>>>,
    /// Deferred async work — `(runnable, on_main)`. `Thread.start`,
    /// `Executor.execute`/`submit` and `Handler.post*` enqueue here
    /// instead of running inline (AOSP is async; inline execution
    /// deadlocks on self-rescheduling executors). Drained at the
    /// outermost `call_virtual` boundary.
    pub pending_runnables: std::collections::VecDeque<(u32, bool)>,
    /// Reentrancy guard for `drain_runnables`.
    pub(crate) draining: bool,
    /// Nesting depth of inline drains issued from `park` — bounds the
    /// recursion when a task inside such a drain parks again.
    pub(crate) park_depth: u32,
    /// Runnables that re-parked inside a drain — when the queue holds
    /// only these, draining them again just makes two waiting workers
    /// hand the slot back and forth until the instruction budget
    /// kills one (and its task is lost). Cleared whenever fresh work
    /// is enqueued — it may target those workers' internal queues.
    reparked: std::collections::HashSet<u32>,
    /// Consecutive `park_drain` calls that drained only re-parked
    /// workers — bounded so mutual waiters yield to the outer drain
    /// (which re-runs them with a fresh budget) instead of spinning
    /// inside one `run()` until the budget kills the task.
    park_spins: u32,
    /// Activity a guest `startActivity` asked to launch — (class
    /// descriptor, the Intent the caller passed). The runner drains
    /// it into a real second-Activity dispatch.
    pub pending_activity: Option<(String, V)>,
    /// canonical class_idx → `java.lang.Class` mirror handle — Class
    /// objects are singletons (`Foo.class == x.getClass()`).
    class_mirrors: HashMap<GIdx, u32>,
    /// Permission names currently granted to the guest package —
    /// seeded from the manifest's normal-level `<uses-permission>`
    /// entries and grown by `requestPermissions` (user grants all).
    pub granted_permissions: std::collections::HashSet<String>,
}

/// Permissions Android grants automatically at install time
/// (`PROTECTION_NORMAL`). Dangerous/signature permissions start denied
/// even when declared — they need `requestPermissions` or a grant the
/// runtime cannot infer.
pub const NORMAL_PERMISSIONS: &[&str] = &[
    "android.permission.INTERNET",
    "android.permission.ACCESS_NETWORK_STATE",
    "android.permission.ACCESS_WIFI_STATE",
    "android.permission.CHANGE_NETWORK_STATE",
    "android.permission.CHANGE_WIFI_STATE",
    "android.permission.CHANGE_WIFI_MULTICAST_STATE",
    "android.permission.VIBRATE",
    "android.permission.WAKE_LOCK",
    "android.permission.RECEIVE_BOOT_COMPLETED",
    "android.permission.FOREGROUND_SERVICE",
    "android.permission.FOREGROUND_SERVICE_DATA_SYNC",
    "android.permission.FOREGROUND_SERVICE_SPECIAL_USE",
    "android.permission.SCHEDULE_EXACT_ALARM",
    "android.permission.USE_EXACT_ALARM",
    "android.permission.REORDER_TASKS",
    "android.permission.SET_WALLPAPER",
    "android.permission.SET_WALLPAPER_HINTS",
    "android.permission.EXPAND_STATUS_BAR",
    "android.permission.MODIFY_AUDIO_SETTINGS",
    "android.permission.NFC",
    "android.permission.BLUETOOTH",
    "android.permission.BLUETOOTH_ADMIN",
];

impl Vm {
    /// Static field value stored on class `class_idx` (`None` when the
    /// class is unloaded or the field unset). Used by `Enum.values()`.
    pub(crate) fn static_of(&self, class_idx: GIdx, field_idx: GIdx) -> Option<V> {
        self.classes
            .get(&self.canon(class_idx))
            .and_then(|cr| cr.statics.get(&self.canon_field(field_idx)).copied())
    }

    /// Wrap a verified DEX file; add more with [`Vm::add_dex`].
    pub fn new(dex: DexFile) -> Result<Self> {
        let mut v = Self {
            dexes: Vec::new(),
            heap: Heap::default(),
            classes: HashMap::new(),
            defined: HashMap::new(),
            defs: HashMap::new(),
            methods_by_sig: HashMap::new(),
            fields_by_sig: HashMap::new(),
            hosts: HashMap::new(),
            hosts_named: HashMap::new(),
            aot: None,
            guest_package: None,
            guest_root: std::env::temp_dir().join("ovelo-guest"),
            depth: 0,
            stack: Vec::new(),
            throw_sites: HashMap::new(),
            stub_trace: Vec::new(),
            main_thread_h: 0,
            cur_thread: 0,
            worker_thread_h: 0,
            main_looper_h: 0,
            app_h: 0,
            unsafe_offsets: HashMap::new(),
            unsafe_next_offset: 0,
            trace_field: std::env::var("OVELO_TRACE_FIELD").ok(),
            trace_throw: std::env::var_os("OVELO_TRACE_THROW").is_some(),
            trace_call: std::env::var("OVELO_TRACE_CALL").ok(),
            view_roots: Vec::new(),
            prefs_files: HashMap::new(),
            cur_activity: 0,
            content_views: HashMap::new(),
            decor_roots: HashMap::new(),
            apk_res: None,
            dbs: HashMap::new(),
            db_handles: HashMap::new(),
            obj_db: HashMap::new(),
            theme_id: 0,
            total_steps: 0,
            pending_activity: None,
            pending_runnables: std::collections::VecDeque::new(),
            draining: false,
            park_depth: 0,
            reparked: std::collections::HashSet::new(),
            park_spins: 0,
            field_idx_cache: std::cell::RefCell::new(HashMap::new()),
            class_mirrors: HashMap::new(),
            granted_permissions: std::collections::HashSet::new(),
        };
        v.add_dex(dex)?;
        Ok(v)
    }

    /// Seed install-time grants: the manifest's `<uses-permission>`
    /// entries that are normal-level auto-grant; the rest stay denied
    /// until the app calls `requestPermissions`.
    pub fn grant_manifest_permissions(&mut self, declared: &[String]) {
        for p in declared {
            if NORMAL_PERMISSIONS.contains(&p.as_str()) {
                self.granted_permissions.insert(p.clone());
            }
        }
    }

    /// Merge another DEX (classes2.dex, …) into the shared index space.
    /// A class defined in a later dex resolves from invokes in earlier
    /// ones — same semantics as ART's unified class path.
    pub fn add_dex(&mut self, dex: DexFile) -> Result<()> {
        // A new dex can resolve names previous lookups missed.
        self.field_idx_cache.borrow_mut().clear();
        let di = self.dexes.len();
        for (class_idx, em) in dex.methods()? {
            self.defined
                .insert(gx(di, em.method_idx), (gx(di, class_idx), em));
            if let Ok(mid) = dex.method(em.method_idx) {
                if let (Ok(owner), Ok(name), Ok(sig)) = (
                    dex.type_descriptor(mid.class_idx as u32),
                    dex.string(mid.name_idx),
                    proto_sig(&dex, mid.proto_idx as u32),
                ) {
                    self.methods_by_sig
                        .entry((owner, name, sig))
                        .or_insert(gx(di, em.method_idx));
                }
            }
        }
        for (i, def) in dex.class_defs().iter().enumerate() {
            if let Ok(desc) = dex.type_descriptor(def.class_idx) {
                self.defs.insert(desc, (di, i));
            }
        }
        for fidx in 0..dex.field_ids_len() as u32 {
            let Ok(f) = dex.field(fidx) else { continue };
            if let (Ok(owner), Ok(name), Ok(ty)) = (
                dex.type_descriptor(f.class_idx as u32),
                dex.string(f.name_idx),
                dex.type_descriptor(f.type_idx as u32),
            ) {
                self.fields_by_sig
                    .entry((owner, name, ty))
                    .or_insert(gx(di, fidx));
            }
        }
        self.dexes.push(dex);
        Ok(())
    }

    /// Canonical class identity: same descriptor ⇒ same GIdx (first dex
    /// that names it). `classes` runtime state and object `class_idx`
    /// keys must agree regardless of which dex referenced the class.
    fn canon(&self, g: GIdx) -> GIdx {
        match self.type_desc_of(g) {
            Ok(desc) => self.type_index_of(&desc).unwrap_or(g),
            Err(_) => g,
        }
    }

    /// Canonical field identity: `(owner, name, type)` first-seen key,
    /// so a field written via dex M's ref reads back through dex N's.
    pub(crate) fn canon_field(&self, g: GIdx) -> GIdx {
        let Some(d) = self.dex_of(g) else {
            return g;
        };
        let Ok(f) = d.field(gl(g)) else {
            return g;
        };
        let sig = (
            d.type_descriptor(f.class_idx as u32).unwrap_or_default(),
            d.string(f.name_idx).unwrap_or_default(),
            d.type_descriptor(f.type_idx as u32).unwrap_or_default(),
        );
        self.fields_by_sig.get(&sig).copied().unwrap_or(g)
    }

    /// (canonical GIdx of the class itself, its &ClassDef) — `def`
    /// fields (`superclass_idx`, `interfaces_off`, …) are local to the
    /// DEFINING dex, whose index is `gd(canon_g)`.
    fn def_loc_of(&self, class_idx: GIdx) -> Option<(GIdx, &ClassDef)> {
        let desc = self.type_desc_of(class_idx).ok()?;
        let &(di, i) = self.defs.get(&desc)?;
        let def = &self.dexes[di].class_defs()[i];
        Some((gx(di, def.class_idx), def))
    }

    /// The dex a global index belongs to.
    fn dex_of(&self, g: GIdx) -> Option<&DexFile> {
        self.dexes.get(gd(g))
    }

    /// Loaded dexes, in index order (dex 0 first — same order `add_dex`
    /// received them).
    pub(crate) fn all_dexes(&self) -> &[DexFile] {
        &self.dexes
    }

    /// `type_idx` lookup across every loaded dex (global result).
    pub(crate) fn type_index_of(&self, desc: &str) -> Option<GIdx> {
        for (di, d) in self.dexes.iter().enumerate() {
            if let Ok(Some(i)) = d.type_index_of(desc) {
                return Some(gx(di, i));
            }
        }
        None
    }

    /// `field_idx` lookup across every loaded dex (global result).
    /// Memoized — dex field tables don't change after `add_dex`.
    pub(crate) fn field_index_of(&self, owner: &str, name: &str) -> Option<GIdx> {
        let key = (owner.to_string(), name.to_string());
        if let Some(hit) = self.field_idx_cache.borrow().get(&key) {
            return *hit;
        }
        let mut out = None;
        for (di, d) in self.dexes.iter().enumerate() {
            if let Some(i) = d.field_index_of(owner, name) {
                out = Some(gx(di, i));
                break;
            }
        }
        self.field_idx_cache.borrow_mut().insert(key, out);
        out
    }

    /// Descriptor for a global `type_idx`.
    /// Descriptor of a class referenced by a `java.lang.Class` handle —
    /// `Obj::Class` mirror or a host mirror holding a descriptor string.
    pub fn class_descriptor_of(&self, h: u32) -> Option<String> {
        match self.heap.get(h) {
            Some(Obj::Class { type_idx }) => self.type_desc_of(*type_idx).ok(),
            Some(Obj::Host {
                state: HostState::StrBuf(d),
                ..
            }) => Some(d.clone()),
            _ => None,
        }
    }

    /// Runtime class descriptor for any heap object handle (debug use).
    pub fn dbg_class_of(&self, h: u32) -> String {
        jdk::class_of(self, V::Ref(h))
    }

    /// Debug: the `Class.field` name for a canonical global field index.
    pub fn dbg_field_name(&self, g: u64) -> String {
        self.dex_of(g)
            .and_then(|d| {
                let f = d.field(gl(g)).ok()?;
                let name = d.string(f.name_idx).ok()?;
                let owner = d.type_descriptor(f.class_idx as u32).ok()?;
                Some(format!("{owner}.{name}"))
            })
            .unwrap_or_else(|| format!("g{g}"))
    }

    /// Debug: the superclass chain of `desc`, used by view debugging.
    pub fn dbg_super_chain(&self, desc: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut cur = desc.to_string();
        for _ in 0..16 {
            let Some(s) = self.super_desc(&cur) else {
                break;
            };
            if s == cur {
                break;
            }
            out.push(s.clone());
            cur = s;
        }
        out
    }

    /// True when `h` is (a subclass of) a `java.util.Collection` guest
    /// object — wrappers around a host collection whose elements are
    /// copied out by `list_snapshot`.
    pub(crate) fn is_guest_collection(&self, h: u32) -> bool {
        let Some(Obj::Instance { class_idx, .. }) = self.heap.get(h) else {
            return false;
        };
        // No `java.util.Collection` in the dex type pool at all → the
        // app never names the interface, so a guest object can still be
        // a collection wrapper. Be permissive there; the backing-field
        // search is the real test.
        self.type_index_of("Ljava/util/Collection;")
            .map(|t| self.assignable(*class_idx, t))
            .unwrap_or(true)
    }

    /// A `java.lang.Class` mirror handle for `desc` (`Ljava/lang/Foo;`).
    /// Mirrors are interned — the same class returns the same handle.
    pub fn class_handle(&mut self, desc: &str) -> u32 {
        let t = self.type_index_of(desc).unwrap_or(GX_NONE);
        self.class_mirror(t)
    }

    /// Interned `Obj::Class` handle for a (possibly non-canonical)
    /// type_idx — canonicalized so `const-class` from dex N and
    /// `getClass` on an instance defined in dex M coincide.
    pub(crate) fn class_mirror(&mut self, type_idx: GIdx) -> u32 {
        let c = self.canon(type_idx);
        if let Some(&h) = self.class_mirrors.get(&c) {
            if self.heap.get(h).is_some() {
                return h;
            }
        }
        let h = self.heap.alloc(Obj::Class { type_idx: c });
        self.class_mirrors.insert(c, h);
        h
    }

    /// Write `v` into a guest-visible named field of a host/instance
    /// object — the same channel `iget` reads through.
    pub fn set_object_field(&mut self, h: u32, owner: &str, name: &str, v: V) {
        if let Some(g) = self.field_index_of(owner, name) {
            let f = self.canon_field(g);
            if let Some(m) = jdk::fields_mut(self, h) {
                m.insert(f, v);
            }
        }
    }

    /// Companion to `set_object_field` — resolves the canonical field
    /// id and reads it off the object's field map.
    pub fn get_object_field(&self, h: u32, owner: &str, name: &str) -> Option<V> {
        let f = self.canon_field(self.field_index_of(owner, name)?);
        match self.heap.get(h) {
            Some(Obj::Instance { fields, .. }) | Some(Obj::Host { fields, .. }) => {
                fields.get(&f).copied()
            }
            _ => None,
        }
    }

    pub(crate) fn type_desc_of(&self, g: GIdx) -> Result<String> {
        self.dex_of(g)
            .ok_or_else(|| err("dexrt.dex.idx"))?
            .type_descriptor(gl(g))
    }

    /// `Lowner;.name(params)ret` for a global method ref, or None when
    /// the index doesn't resolve (sentinel).
    fn method_sig(&self, midx: GIdx) -> Result<(String, String, String)> {
        let d = self.dex_of(midx).ok_or_else(|| err("dexrt.method.id"))?;
        let mid = d.method(gl(midx))?;
        let class = d.type_descriptor(mid.class_idx as u32)?;
        let name = d.string(mid.name_idx)?;
        let p = d.proto(mid.proto_idx as u32)?;
        let mut sig = String::from("(");
        for t in d.proto_params(p)? {
            sig.push_str(&d.type_descriptor(t as u32)?);
        }
        sig.push(')');
        let ret = d.type_descriptor(p.return_type_idx)?;
        sig.push_str(&ret);
        Ok((class, name, sig))
    }



    /// Register a host implementation for the method reference
    /// `class_desc.name proto_desc` (e.g.
    /// `Landroid/app/Activity;` `setContentView`
    /// `(Landroid/view/View;)V`). Returns `false` when the DEX holds no
    /// such method reference — the app never calls it.
    pub fn register_host(
        &mut self,
        class_desc: &str,
        name: &str,
        proto_desc: &str,
        f: HostCall,
    ) -> Result<bool> {
        let mut found = false;
        for (di, d) in self.dexes.iter().enumerate() {
            for midx in 0..d.method_ids_len() as u32 {
                let mid = d.method(midx)?;
                let class = d.type_descriptor(mid.class_idx as u32)?;
                if class != class_desc {
                    continue;
                }
                if d.string(mid.name_idx)? != name {
                    continue;
                }
                if proto_sig(d, mid.proto_idx as u32)? == proto_desc {
                    self.hosts.insert(gx(di, midx), f.clone());
                    found = true;
                }
            }
            // Defined methods too: a call may reference only the base
            // class's descriptor (`MenuInflater.inflate`) while virtual
            // resolution lands on the dex-defined override
            // (`SupportMenuInflater.inflate`) that nothing references.
            // Best-effort: one unparseable class must not abort the scan.
            for def in d.class_defs() {
                let Ok(desc) = d.type_descriptor(def.class_idx) else {
                    continue;
                };
                if desc != class_desc {
                    continue;
                }
                let Ok(cd) = d.class_data(def) else {
                    continue;
                };
                for m in cd.direct_methods.iter().chain(cd.virtual_methods.iter()) {
                    let Ok(mid) = d.method(m.method_idx) else {
                        continue;
                    };
                    if d.string(mid.name_idx).map(|n| n == name).unwrap_or(false)
                        && proto_sig(d, mid.proto_idx as u32)
                            .map(|s| s == proto_desc)
                            .unwrap_or(false)
                    {
                        self.hosts.insert(gx(di, m.method_idx), f.clone());
                        found = true;
                    }
                }
            }
        }
        if found {
            // A DEX may reference the same method through a subclass
            // descriptor; fall back to (name, sig) in call_builtin.
            let owner = self.type_index_of(class_desc).unwrap_or(GX_NONE);
            self.hosts_named
                .entry((name.to_string(), proto_desc.to_string()))
                .or_default()
                .push((owner, f));
        }
        Ok(found)
    }

    /// Heap borrow.
    pub fn heap(&self) -> &Heap {
        &self.heap
    }

    /// AOT-compile every eligible method to host code (design AOT:
    /// integer/branch methods first; everything else keeps running on
    /// the interpreter). Returns how many methods were compiled.
    pub fn enable_aot(&mut self) -> usize {
        let Some(mut aot) = Aot::new() else {
            return 0;
        };
        let mids: Vec<GIdx> = self.defined.keys().copied().collect();
        for midx in mids {
            if !self.sig_int_typed(midx) {
                continue;
            }
            let em = &self.defined[&midx].1;
            let Ok(Some(code)) = self
                .dex_of(midx)
                .ok_or_else(|| err("dexrt.dex.idx"))
                .and_then(|d| d.code_item(em.code_off))
            else {
                continue;
            };
            aot.compile(midx, &code);
        }
        let n = aot.len();
        self.aot = Some(aot);
        n
    }

    /// How many methods were AOT-compiled (`enable_aot` return).
    pub fn aot_len(&self) -> usize {
        self.aot.as_ref().map_or(0, |a| a.len())
    }

    /// Signature gate for AOT: every param and the return must be
    /// integer-typed so the i64-slot ABI matches `V`.
    fn sig_int_typed(&self, midx: GIdx) -> bool {
        let Some(d) = self.dex_of(midx) else {
            return false;
        };
        let Ok(mid) = d.method(gl(midx)) else {
            return false;
        };
        let Ok(p) = d.proto(mid.proto_idx as u32) else {
            return false;
        };
        let Ok(ret) = d.type_descriptor(p.return_type_idx) else {
            return false;
        };
        if !aot::int_typed(&ret) {
            return false;
        }
        let Ok(params) = d.proto_params(p) else {
            return false;
        };
        params.iter().all(|t| {
            d.type_descriptor(*t as u32)
                .map(|d| aot::int_typed(&d))
                .unwrap_or(false)
        })
    }

    /// Entry point: invoke `<class>.<method>(args)` as a static call.
    /// Returns `Some(value)` or `None` for void. An uncaught guest exception
    /// surfaces as `dexrt.exception.uncaught`.
    pub fn run(&mut self, class_desc: &str, name: &str, args: &[V]) -> Result<Option<V>> {
        let midx = self
            .find_method_by_name(class_desc, name, args.len())?
            .ok_or_else(|| errd("dexrt.method.missing", format!("{class_desc}.{name}")))?;
        match self.invoke(midx, args.to_vec(), InvokeKind::Static) {
            Ok(v) => Ok(v),
            Err(E::Vm(e)) => Err(e),
            Err(E::Throw(r)) => {
                let d = self.describe_throw(r);
                Err(errd("dexrt.exception.uncaught", d))
            }
        }
    }

    /// Instantiate `class_desc`: allocates the instance and invokes its
    /// `<init>` (a class without one gets `Object.<init>`'s no-op).
    /// Returns the heap handle.
    pub fn instantiate(&mut self, class_desc: &str) -> Result<u32> {
        let ty = self
            .type_index_of(class_desc)
            .ok_or_else(|| errd("dexrt.class.missing", class_desc.to_string()))?;
        if self.def_of(ty).is_none() {
            return Err(errd("dexrt.class.missing", class_desc.to_string()));
        }
        match self.ensure_init(ty) {
            Ok(()) => {}
            Err(E::Vm(e)) => return Err(e),
            Err(E::Throw(r)) => {
                let d = self.describe_throw(r);
                return Err(errd("dexrt.exception.uncaught", d));
            }
        }
        let h = self.heap.alloc(Obj::Instance {
            class_idx: self.canon(ty),
            fields: HashMap::new(),
        });
        // A real guest Application subclass IS the app object —
        // `getApplication[Context]` must hand it back so onCreate state
        // and custom methods are reachable. Overrides the interned
        // placeholder `app_object` may have minted earlier.
        if let Some(app_ty) = self.type_index_of("Landroid/app/Application;") {
            if self.assignable(ty, app_ty) {
                self.app_h = h;
            }
        }
        if let Some(midx) = self.find_method_by_name(class_desc, "<init>", 0)? {
            match self.invoke(midx, vec![V::Ref(h)], InvokeKind::Direct) {
                Ok(_) => {}
                Err(E::Vm(e)) => return Err(e),
                Err(E::Throw(r)) => {
                    let d = self.describe_throw(r);
                    return Err(errd("dexrt.exception.uncaught", d));
                }
            }
        }
        Ok(h)
    }

    /// Allocate an instance and run the `<init>` overload matching
    /// `args`' slot count. Used by the layout inflater: custom view
    /// classes (and bundled libs like material's NavigationView) get
    /// real constructor state, not an opaque host stub.
    pub(crate) fn instantiate_with(&mut self, class_desc: &str, args: &[V]) -> Result<u32> {
        let ty = self
            .type_index_of(class_desc)
            .ok_or_else(|| errd("dexrt.class.missing", class_desc.to_string()))?;
        if self.def_of(ty).is_none() {
            return Err(errd("dexrt.class.missing", class_desc.to_string()));
        }
        match self.ensure_init(ty) {
            Ok(()) => {}
            Err(E::Vm(e)) => return Err(e),
            Err(E::Throw(r)) => {
                let d = self.describe_throw(r);
                return Err(errd("dexrt.exception.uncaught", d));
            }
        }
        let h = self.heap.alloc(Obj::Instance {
            class_idx: self.canon(ty),
            fields: HashMap::new(),
        });
        let slots: usize = args
            .iter()
            .map(|a| if matches!(a, V::I64(_) | V::F64(_)) { 2 } else { 1 })
            .sum();
        if let Some(midx) = self.find_method_by_name(class_desc, "<init>", slots)? {
            let mut a = Vec::with_capacity(args.len() + 1);
            a.push(V::Ref(h));
            a.extend_from_slice(args);
            match self.invoke(midx, a, InvokeKind::Direct) {
                Ok(_) => {}
                Err(E::Vm(e)) => return Err(e),
                Err(E::Throw(r)) => {
                    let d = self.describe_throw(r);
                    return Err(errd("dexrt.exception.uncaught", d));
                }
            }
        }
        Ok(h)
    }

    /// Every `<init>` overload declared on `class_desc` as
    /// `(method ref, param type descriptors)` — the inflater scores them
    /// to pick the best-fit view constructor.
    pub(crate) fn ctor_list(&self, class_desc: &str) -> Vec<(GIdx, Vec<String>)> {
        let mut out = Vec::new();
        for (di, d) in self.dexes.iter().enumerate() {
            for def in d.class_defs() {
                if d.type_descriptor(def.class_idx).ok().as_deref() != Some(class_desc) {
                    continue;
                }
                let Ok(cd) = d.class_data(def) else {
                    return out;
                };
                for m in cd.direct_methods.iter() {
                    let Ok(mid) = d.method(m.method_idx) else {
                        continue;
                    };
                    if d.string(mid.name_idx).ok().as_deref() != Some("<init>") {
                        continue;
                    }
                    let params: Vec<String> = d
                        .proto(mid.proto_idx as u32)
                        .and_then(|p| d.proto_params(p))
                        .map(|list| {
                            list.iter()
                                .filter_map(|t| d.type_descriptor(*t as u32).ok())
                                .collect()
                        })
                        .unwrap_or_default();
                    out.push((gx(di, m.method_idx), params));
                }
                return out;
            }
        }
        out
    }

    /// Allocate an instance and run the specific `<init>` from
    /// [`Vm::ctor_list`] — proto-aware construction when several
    /// overloads share a slot count.
    pub(crate) fn instantiate_ctor(
        &mut self,
        class_desc: &str,
        midx: GIdx,
        args: &[V],
    ) -> Result<u32> {
        let ty = self
            .type_index_of(class_desc)
            .ok_or_else(|| errd("dexrt.class.missing", class_desc.to_string()))?;
        match self.ensure_init(ty) {
            Ok(()) => {}
            Err(E::Vm(e)) => return Err(e),
            Err(E::Throw(r)) => {
                let d = self.describe_throw(r);
                return Err(errd("dexrt.exception.uncaught", d));
            }
        }
        let h = self.heap.alloc(Obj::Instance {
            class_idx: self.canon(ty),
            fields: HashMap::new(),
        });
        let mut a = Vec::with_capacity(args.len() + 1);
        a.push(V::Ref(h));
        a.extend_from_slice(args);
        match self.invoke(midx, a, InvokeKind::Direct) {
            Ok(_) => Ok(h),
            Err(E::Vm(e)) => Err(e),
            Err(E::Throw(r)) => {
                let d = self.describe_throw(r);
                Err(errd("dexrt.exception.uncaught", d))
            }
        }
    }

    /// `field_index_of` walking the superclass chain — atomic field
    /// updaters resolve against the target's runtime class, but the
    /// field is usually declared on a base class (JobSupport's
    /// `_state`); missing it reads a fallback slot that's always null.
    pub(crate) fn field_index_on_chain(&self, owner: &str, name: &str) -> Option<GIdx> {
        let mut cur = Some(owner.to_string());
        for _ in 0..32 {
            let d = cur?;
            if let Some(g) = self.field_index_of(&d, name) {
                return Some(g);
            }
            cur = self.super_desc(&d);
        }
        None
    }

    /// Superclass descriptor for a class by name — the dex class def
    /// when present, else the framework `host_supers` table.
    pub(crate) fn super_desc(&self, desc: &str) -> Option<String> {
        if let Some(ty) = self.type_index_of(desc) {
            if let Some((cg, def)) = self.def_loc_of(ty) {
                if def.superclass_idx == u32::MAX {
                    return None;
                }
                return self.type_desc_of(gx(gd(cg), def.superclass_idx)).ok();
            }
        }
        jdk::host_supers(desc).first().map(|s| (*s).to_string())
    }

    /// Whether `desc` is, or descends from, a class whose simple name is
    /// `simple` — `extends_named("Lorg/foo/MainPager;", "ViewPager")` is
    /// true when `MainPager extends ViewPager`. Walks dex class_defs
    /// first, then the host_supers chain (via `super_desc`).
    pub fn extends_named(&self, desc: &str, simple: &str) -> bool {
        let mut d = desc.to_string();
        for _ in 0..32 {
            if d.rsplit('/').next().unwrap_or(&d).trim_end_matches(';') == simple {
                return true;
            }
            let Some(next) = self.super_desc(&d) else {
                return false;
            };
            if next == d {
                return false;
            }
            d = next;
        }
        false
    }

    /// Allocate an instance without running its `<init>` — for
    /// `Unsafe.allocateInstance`, which must not execute constructors.
    pub(crate) fn alloc_instance_raw(&mut self, class_desc: &str) -> Result<u32> {
        let ty = self
            .type_index_of(class_desc)
            .ok_or_else(|| errd("dexrt.class.missing", class_desc.to_string()))?;
        match self.ensure_init(ty) {
            Ok(()) => {}
            Err(E::Vm(e)) => return Err(e),
            Err(E::Throw(r)) => {
                let d = self.describe_throw(r);
                return Err(errd("dexrt.exception.uncaught", d));
            }
        }
        Ok(self.heap.alloc(Obj::Instance {
            class_idx: self.canon(ty),
            fields: HashMap::new(),
        }))
    }

    /// `clinit` reached state 2 (done), or the class needs no init.
    pub(crate) fn class_initialized(&self, class_idx: GIdx) -> bool {
        let class_idx = self.canon(class_idx);
        self.classes
            .get(&class_idx)
            .map(|c| c.clinit == 2)
            .unwrap_or(self.def_of(class_idx).is_none())
    }

    /// pub(crate) gate for jdk builtins (Unsafe.ensureClassInitialized).
    pub(crate) fn ensure_class_init(&mut self, class_idx: GIdx) -> Rt<()> {
        self.ensure_init(class_idx)
    }

    /// Read/write a class's static table (Unsafe staticFieldBase model).
    pub(crate) fn static_read(&self, class_idx: GIdx, field_idx: GIdx) -> Option<V> {
        self.classes
            .get(&self.canon(class_idx))
            .and_then(|c| c.statics.get(&self.canon_field(field_idx)).copied())
    }

    pub(crate) fn static_write(&mut self, class_idx: GIdx, field_idx: GIdx, v: V) {
        let k = self.canon(class_idx);
        let f = self.canon_field(field_idx);
        self.classes.entry(k).or_default().statics.insert(f, v);
    }

    /// Mark which activity's code is dispatching — content/decor roots
    /// attached while it runs are recorded under it.
    pub fn set_activity(&mut self, activity: u32) {
        self.cur_activity = activity;
    }

    /// Invoke `name` on the object at `this` — resolves the method on
    /// its class chain by name and arg count. `Ok(None)` when the
    /// object's class doesn't declare it — the platform base class
    /// (not in this DEX) provides the default.
    pub fn call_virtual(&mut self, this: u32, name: &str, args: &[V]) -> Result<Option<V>> {
        let ci = self
            .heap
            .class_of(this)
            .ok_or_else(|| errd("dexrt.bad_this", name.to_string()))?;
        let Some(midx) = self.find_method_on_chain(ci, name, args.len())? else {
            return Ok(None);
        };
        let mut a = Vec::with_capacity(args.len() + 1);
        a.push(V::Ref(this));
        a.extend_from_slice(args);
        let out = match self.invoke(midx, a, InvokeKind::Virtual) {
            Ok(v) => Ok(v),
            Err(E::Vm(e)) => Err(e),
            Err(E::Throw(r)) => {
                let d = self.describe_throw(r);
                Err(errd("dexrt.exception.uncaught", d))
            }
        };
        if self.depth == 0 {
            self.drain_runnables(500);
        }
        out
    }

    /// `call_virtual` for dispatch that happens INSIDE guest code
    /// (builtin callbacks like `SQLiteOpenHelper.onCreate`): a guest
    /// throw propagates as `E::Throw` so the app's own catch/finally
    /// sees it, instead of becoming a fatal VM error.
    pub(crate) fn call_virtual_rt(
        &mut self,
        this: u32,
        name: &str,
        args: &[V],
    ) -> Rt<Option<V>> {
        let ci = self
            .heap
            .class_of(this)
            .ok_or_else(|| E::Vm(errd("dexrt.bad_this", name.to_string())))?;
        let Some(midx) = self.find_method_on_chain(ci, name, args.len()).map_err(E::Vm)? else {
            return Ok(None);
        };
        let mut a = Vec::with_capacity(args.len() + 1);
        a.push(V::Ref(this));
        a.extend_from_slice(args);
        let out = self.invoke(midx, a, InvokeKind::Virtual);
        if self.depth == 0 {
            self.drain_runnables(500);
        }
        out
    }

    /// `LockSupport.park` semantics on a single shared queue: the
    /// parked Java frame can't resume later, so run queued work
    /// inline (the task that completes the wait unparks / stores the
    /// result) and let the caller's event loop re-check its own
    /// queue. Nested drains are bounded by `park_depth`; past the cap
    /// we fall back to the preempt-and-requeue unwind, which is
    /// correct for queue-scanning worker loops (they re-scan on
    /// re-entry) but makes runBlock-style callers redo work — hence
    /// the generous cap.
    pub(crate) fn park_drain(&mut self) -> std::result::Result<(), E> {
        // Nothing queued anywhere: an inline drain can't complete the
        // wait, so yield by unwinding — the drain loop requeues the
        // runnable (queue-scanning workers rescan on re-entry).
        if self.pending_runnables.is_empty() || self.park_depth >= 8 {
            return Err(E::Vm(err("ovelo.park")));
        }
        // Every queued runnable already re-parked — draining them
        // makes no progress when two waiting workers just requeue each
        // other. Allow a few retries (fresh work may have reached a
        // reparked worker's internal queue), then yield to the outer
        // drain instead of burning this run()'s whole budget spinning.
        if !self
            .pending_runnables
            .iter()
            .any(|(r, _)| !self.reparked.contains(r))
        {
            self.park_spins += 1;
            if self.park_spins > 4 {
                return Err(E::Vm(err("ovelo.park")));
            }
        } else {
            self.park_spins = 0;
        }
        self.park_depth += 1;
        let was = self.draining;
        self.draining = false;
        self.drain_runnables(64);
        self.draining = was;
        self.park_depth -= 1;
        Ok(())
    }

    /// Enqueue a runnable (`Thread.start`/`Executor.execute`/
    /// `Handler.post*`). Fresh work may target a re-parked worker's
    /// internal queue, so all `reparked` tags are cleared — a waiting
    /// parker must retry those workers rather than assume they are
    /// still idle.
    pub(crate) fn enqueue_runnable(&mut self, r: u32, on_main: bool) {
        self.reparked.clear();
        self.pending_runnables.push_back((r, on_main));
    }

    /// Run enqueued async work FIFO — `Thread.start`/`Executor.execute`
    /// jobs on a worker thread, `Handler.post*` on the main one.
    /// Reentrant-safe; `max` bounds total runnables per drain so a
    /// self-rescheduling queue can't wedge the caller forever.
    pub fn drain_runnables(&mut self, max: usize) {
        if self.draining {
            return;
        }
        self.draining = true;
        let dbg = std::env::var_os("OVELO_DRAINDBG").is_some();
        for _ in 0..max {
            let Some((r, on_main)) = self.pending_runnables.pop_front() else {
                break;
            };
            let out = if on_main {
                jdk::run_on_main(self, r)
            } else {
                jdk::run_on_worker(self, r)
            };
            if dbg {
                match &out {
                    Ok(_) => eprintln!("[drain] ran h={r} main={on_main}"),
                    Err(e) => eprintln!(
                        "[drain] h={r} main={on_main} err={:?}",
                        e
                    ),
                }
            }
            match &out {
                Err(e) if e.cause() == "ovelo.park" => {
                    // A parked worker yields its slot and reschedules —
                    // the runnable re-enters `run()` on a later drain
                    // and re-scans its own work queue.
                    self.reparked.insert(r);
                    self.pending_runnables.push_back((r, on_main));
                }
                _ => {
                    self.reparked.remove(&r);
                    self.park_spins = 0;
                }
            }
        }
        self.draining = false;
    }

    /// Attach `child` under `parent` in the inflated-view tree — used
    /// by the framework guest bridge when a `ViewGroup.addView` call
    /// lands on a dex-inflated view rather than a host `GuestView`.
    pub fn view_tree_add(&mut self, parent: u32, child: u32) {
        if parent != 0 && child != 0 {
            jdk::view::attach(self, parent, V::Ref(child));
        }
    }

    /// Record `content` as `activity`'s content view in the view-tree
    /// model (fallback for content the guest bridge doesn't map).
    pub fn view_set_content(&mut self, activity: u32, content: u32) {
        jdk::view::set_content_view(self, activity, V::Ref(content));
    }

    /// The handle `activity` last set as its content view, if any.
    pub fn activity_content(&self, activity: u32) -> Option<u32> {
        jdk::view::activity_content(self, activity)
    }

    /// Snapshot one node of the dex view tree for host rendering —
    /// text, colors, layout params, visibility, listener, children.
    pub fn dex_view(&self, h: u32) -> Option<jdk::view::DexViewInfo> {
        jdk::view::dex_view_info(self, h)
    }

    /// The options `Menu` attached to an Activity — the runner hands it
    /// to `onCreateOptionsMenu`/`onPrepareOptionsMenu` so guest menu
    /// code lands on the same store `getMenu`/`findItem` reach.
    pub fn menu_for(&mut self, activity: u32) -> V {
        jdk::view::menu_of(self, activity)
    }

    /// Route the androidx `MenuBuilder`/`SubMenuBuilder` and
    /// `MenuInflater.inflate` surface through the host menu model —
    /// they ship in the APK as dex classes, so without overrides their
    /// methods run guest code against fields the host never populated
    /// (`findItem` returning null, `size` 0).
    pub fn register_menu_overrides(&mut self) {
        jdk::view::register_menu_overrides(self);
    }

    /// Whether the guest called `finish()` on this Activity — remaining
    /// lifecycle callbacks (onStart/onResume/…) should be skipped.
    pub fn is_finishing(&self, activity: u32) -> bool {
        jdk::view::is_finishing(self, activity)
    }
    /// Record the launch Intent on an Activity the runner is about to
    /// start — `Activity.getIntent` hands it back to guest code.
    pub fn set_activity_intent(&mut self, activity: u32, intent: V) {
        jdk::view::set_activity_intent(self, activity, intent);
    }

    fn describe_throw(&self, h: u32) -> String {
        self.describe_throw_inner(h, &mut std::collections::HashSet::new())
    }

    fn describe_throw_inner(&self, h: u32, seen: &mut std::collections::HashSet<u32>) -> String {
        // Guest exceptions sometimes store themselves (or a cycle of
        // wrappers) as their own cause — bound the walk.
        if !seen.insert(h) {
            return format!("object #{h}");
        }
        let site = self
            .throw_sites
            .get(&h)
            .map(|s| format!(" thrown in {s}"))
            .unwrap_or_default();
        let mut base = match self.heap.get(h) {
            Some(Obj::Throwable { descriptor, detail }) => {
                format!("{descriptor}: {detail}{site}")
            }
            Some(Obj::Instance { class_idx, fields }) => {
                let base = self
                    .type_desc_of(*class_idx)
                    .unwrap_or_else(|_| format!("object #{h}"));
                // Guest wrappers (RuntimeException(cause)) bury the real
                // failure — surface stored messages and wrapped throwables.
                let mut extra = String::new();
                for v in fields.values() {
                    if let V::Ref(fh) = v {
                        match self.heap.get(*fh) {
                            Some(Obj::Str(t)) if !t.is_empty() => {
                                extra.push_str(&format!(": {t}"));
                            }
                            Some(Obj::Throwable { descriptor, detail }) => {
                                extra.push_str(&format!(
                                    " caused by {descriptor}: {detail}"
                                ));
                            }
                            _ => {}
                        }
                    }
                }
                format!("guest throwable {base}{extra}{site}")
            }
            Some(Obj::Host { desc, .. }) => {
                let msg = jdk::pseudo_get(self, h, jdk::P_DATA)
                    .and_then(|v| match v {
                        V::Ref(r) => self.heap.str_val(r),
                        _ => None,
                    })
                    .map(|t| format!(": {t}"))
                    .unwrap_or_default();
                let cause = match jdk::pseudo_get(self, h, jdk::P_TARGET) {
                    Some(V::Ref(c)) if c != 0 && c != h => {
                        format!(" caused by {}", self.describe_throw_inner(c, seen))
                    }
                    _ => String::new(),
                };
                format!("host throwable {desc}{msg}{cause}{site}")
            }
            _ => format!("object #{h}"),
        };
        // The stub tail explains most guest crashes: the app read a
        // placeholder where the real framework hands back data.
        if !self.stub_trace.is_empty() {
            let n = self.stub_trace.len();
            let tail = &self.stub_trace[n.saturating_sub(16)..];
            base.push_str(&format!("; stubs: {}", tail.join(", ")));
        }
        base
    }

    /// `Lcls;.name(params)ret` for error details — an unresolved call
    /// must name the API it needed (compat reports key off this).
    fn describe_method(&self, midx: GIdx) -> String {
        self.method_sig(midx)
            .map(|(class, name, sig)| format!("{class}.{name}{sig}"))
            .unwrap_or_else(|_| format!("method@{midx}"))
    }

    /// Live call stack as `Lcls;.name(sig)ret` strings, newest frame
    /// last — jdk shims read it to build guest stack traces.
    pub(crate) fn describe_stack(&self) -> Vec<String> {
        self.stack.iter().map(|&m| self.describe_method(m)).collect()
    }

    /// `Lcls;.name` for a field ref — error details identify the field.
    fn describe_field(&self, fidx: GIdx) -> String {
        let fmt = || -> Result<String> {
            let d = self.dex_of(fidx).ok_or_else(|| err("dexrt.field.id"))?;
            let f = d.field(gl(fidx))?;
            let class = d.type_descriptor(f.class_idx as u32)?;
            let name = d.string(f.name_idx)?;
            Ok(format!("{class}.{name}"))
        };
        fmt().unwrap_or_else(|_| format!("field@{fidx}"))
    }

    /// `find_method_by_name` over the class's superclass chain — an
    /// inherited callback (e.g. onCreate on a DEX parent) still runs.
    fn find_method_on_chain(
        &self,
        mut class_idx: GIdx,
        name: &str,
        nslots: usize,
    ) -> Result<Option<GIdx>> {
        for _ in 0..64 {
            let Some((cg, def)) = self.def_loc_of(class_idx) else {
                return Ok(None);
            };
            let sup = def.superclass_idx;
            let desc = self.type_desc_of(cg)?;
            if let Some(m) = self.find_method_by_name(&desc, name, nslots)? {
                return Ok(Some(m));
            }
            if sup == u32::MAX {
                return Ok(None);
            }
            class_idx = gx(gd(cg), sup);
        }
        Ok(None)
    }

    /// First method `name` on `class_desc` whose params occupy exactly
    /// `nslots` registers (J/D params take two) — args pack as a register
    /// image, so a long/double param counts double.
    fn find_method_by_name(
        &self,
        class_desc: &str,
        name: &str,
        nslots: usize,
    ) -> Result<Option<GIdx>> {
        for (di, d) in self.dexes.iter().enumerate() {
            for def in d.class_defs() {
                if d.type_descriptor(def.class_idx)? != class_desc {
                    continue;
                }
                let cd = d.class_data(def)?;
                for m in cd.direct_methods.iter().chain(cd.virtual_methods.iter()) {
                    let mid = d.method(m.method_idx)?;
                    if d.string(mid.name_idx)? != name {
                        continue;
                    }
                    let p = d.proto(mid.proto_idx as u32)?;
                    let mut slots = 0usize;
                    for t in d.proto_params(p)? {
                        let ty = d.type_descriptor(t as u32)?;
                        slots += if ty.starts_with('J') || ty.starts_with('D') {
                            2
                        } else {
                            1
                        };
                    }
                    if slots == nslots {
                        return Ok(Some(gx(di, m.method_idx)));
                    }
                }
            }
        }
        Ok(None)
    }

    fn def_of(&self, class_idx: GIdx) -> Option<&ClassDef> {
        self.def_loc_of(class_idx).map(|(_, d)| d)
    }

    fn class_data_of(&self, class_idx: GIdx) -> Result<Option<ClassData>> {
        match self.def_loc_of(class_idx) {
            Some((cg, d)) => Ok(Some(
                self.dex_of(cg).ok_or_else(|| err("dexrt.dex.idx"))?.class_data(d)?,
            )),
            None => Ok(None),
        }
    }

    /// `sub` assignable to `sup` through the superclass chain, with
    /// interfaces walked transitively (a class may implement a
    /// sub-interface of `sup` or reach `sup` through another interface).
    pub(crate) fn assignable(&self, sub: GIdx, sup: GIdx) -> bool {
        // Cross-dex identity is by descriptor: the same class may be a
        // different GIdx in every dex that references it.
        let sup_desc = self.type_desc_of(sup).unwrap_or_default();
        let same = |g: GIdx| -> bool {
            match self.type_desc_of(g) {
                Ok(d) => d == sup_desc,
                Err(_) => g == sup,
            }
        };
        if same(sub) {
            return true;
        }
        let mut ifaces: Vec<GIdx> = Vec::new();
        let mut seen: Vec<String> = Vec::new();
        let mut cur = sub;
        'cls: for _ in 0..64 {
            match self.def_loc_of(cur) {
                Some((cg, def)) => {
                    if def.interfaces_off != 0 {
                        if let Some(d) = self.dex_of(cg) {
                            if let Ok(list) = d.type_list(def.interfaces_off) {
                                for i in list {
                                    let gi = gx(gd(cg), i as u32);
                                    let id = self.type_desc_of(gi).unwrap_or_default();
                                    if !seen.contains(&id) {
                                        seen.push(id);
                                        ifaces.push(gi);
                                    }
                                }
                            }
                        }
                    }
                    let sup_l = def.superclass_idx;
                    if sup_l == u32::MAX {
                        break;
                    }
                    cur = gx(gd(cg), sup_l);
                }
                None => {
                    // Host framework class — no class_def; walk the
                    // builtin hierarchy by descriptor. Intermediate
                    // classes may be absent from the DEX's type table
                    // (e.g. CustomButton extends Button but never names
                    // TextView), so keep going until `sup` or a dead end.
                    let Ok(mut d) = self.type_desc_of(cur) else {
                        break;
                    };
                    loop {
                        if d == sup_desc || jdk::host_supers(&d).iter().any(|s| *s == sup_desc)
                        {
                            return true;
                        }
                        let Some(next) = Self::framework_super(&d) else {
                            // Dead end on the host side — the collected
                            // interfaces may still satisfy `sup`.
                            break 'cls;
                        };
                        if next == d {
                            // Self-referential supers entry would loop
                            // forever — treat as a dead end.
                            break 'cls;
                        }
                        match self.type_index_of(next) {
                            Some(next) => {
                                cur = next;
                                break;
                            }
                            _ => d = next.to_string(),
                        }
                    }
                }
            }
            if same(cur) {
                return true;
            }
        }
        // Transitive interface walk: an interface's own `interfaces_off`
        // lists its super-interfaces.
        while let Some(i) = ifaces.pop() {
            if same(i) {
                return true;
            }
            if let Some((cg, def)) = self.def_loc_of(i) {
                if def.interfaces_off != 0 {
                    if let Some(d) = self.dex_of(cg) {
                        if let Ok(list) = d.type_list(def.interfaces_off) {
                            for j in list {
                                let gj = gx(gd(cg), j as u32);
                                let id = self.type_desc_of(gj).unwrap_or_default();
                                if !seen.contains(&id) {
                                    seen.push(id);
                                    ifaces.push(gj);
                                }
                            }
                        }
                    }
                }
            } else if let Ok(id) = self.type_desc_of(i) {
                // JDK interface absent from the dex (java.util.Set,
                // Iterable, …) — its super-interfaces live in the
                // builtin/host tables.
                if self.assignable_desc(&id, &sup_desc) {
                    return true;
                }
            }
        }
        false
    }

    /// Superclass descriptor for a host-provided framework class (the
    /// first entry of [`jdk::host_supers`]; `None` at `Object`/unknown).
    fn framework_super(d: &str) -> Option<&'static str> {
        jdk::host_supers(d).first().copied()
    }

    /// Known supers of builtin throwable descriptors.
    fn builtin_supers(d: &str) -> &'static [&'static str] {
        match d {
            "Ljava/lang/NullPointerException;"
            | "Ljava/lang/ArrayIndexOutOfBoundsException;"
            | "Ljava/lang/IndexOutOfBoundsException;"
            | "Ljava/lang/ArithmeticException;"
            | "Ljava/lang/ClassCastException;"
            | "Ljava/lang/NegativeArraySizeException;"
            | "Ljava/util/NoSuchElementException;" => &[
                "Ljava/lang/RuntimeException;",
                "Ljava/lang/Exception;",
                "Ljava/lang/Throwable;",
                "Ljava/lang/Object;",
            ],
            "Ljava/lang/RuntimeException;" => &[
                "Ljava/lang/Exception;",
                "Ljava/lang/Throwable;",
                "Ljava/lang/Object;",
            ],
            "Ljava/lang/NoClassDefFoundError;" => &[
                "Ljava/lang/LinkageError;",
                "Ljava/lang/Error;",
                "Ljava/lang/Throwable;",
                "Ljava/lang/Object;",
            ],
            "Ljava/lang/ExceptionInInitializerError;" => &[
                "Ljava/lang/LinkageError;",
                "Ljava/lang/Error;",
                "Ljava/lang/Throwable;",
                "Ljava/lang/Object;",
            ],
            "Ljava/lang/LinkageError;" => &[
                "Ljava/lang/Error;",
                "Ljava/lang/Throwable;",
                "Ljava/lang/Object;",
            ],
            "Ljava/lang/Exception;" | "Ljava/lang/Error;" => &[
                "Ljava/lang/Throwable;",
                "Ljava/lang/Object;",
            ],
            "Ljava/lang/Throwable;" => &["Ljava/lang/Object;"],
            _ => &[],
        }
    }

    /// Allocate a host-provided object: `class_idx` is `GX_NONE` when no
    /// dex references the type (builtin-created helpers).
    pub(crate) fn alloc_host(&mut self, desc: &str, state: HostState) -> u32 {
        let class_idx = self.type_index_of(desc).unwrap_or(GX_NONE);
        self.heap.alloc(Obj::Host {
            class_idx,
            desc: desc.to_string(),
            fields: HashMap::new(),
            state,
        })
    }

    /// Builtin state for a host-provided class, chosen by descriptor.
    fn host_state_for(desc: &str) -> HostState {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        // Desugared `j$.*` collections mirror the `java.util` family.
        let canon;
        let desc = if desc.starts_with("Lj$/util/") {
            canon = desc.replacen("Lj$/util/", "Ljava/util/", 1);
            canon.as_str()
        } else {
            desc
        };
        match desc {
            "Ljava/lang/StringBuilder;" | "Ljava/lang/StringBuffer;" => {
                HostState::StrBuf(String::new())
            }
            "Ljava/util/ArrayList;" | "Ljava/util/LinkedList;" | "Ljava/util/Vector;"
            | "Ljava/util/Stack;" | "Ljava/util/HashSet;" | "Ljava/util/EnumSet;"
            | "Ljava/util/LinkedHashSet;"
            | "Ljava/util/TreeSet;" | "Ljava/util/ArrayDeque;" | "Ljava/util/BitSet;"
            | "Ljava/util/PriorityQueue;" | "Ljava/util/AbstractCollection;"
            | "Ljava/util/AbstractList;" | "Ljava/util/AbstractSet;"
            | "Ljava/util/AbstractQueue;" | "Ljava/util/AbstractSequentialList;"
            | "Ljava/util/Arrays$ArrayList;" | "Ljava/util/Collections$EmptyList;"
            | "Ljava/util/Collections$EmptySet;" | "Ljava/util/Collections$SingletonList;"
            | "Ljava/util/Collections$SingletonSet;"
            | "Ljava/util/concurrent/CopyOnWriteArrayList;"
            | "Ljava/util/concurrent/CopyOnWriteArraySet;"
            | "Ljava/util/concurrent/ConcurrentLinkedQueue;"
            | "Ljava/util/concurrent/ConcurrentLinkedDeque;"
            | "Ljava/util/concurrent/LinkedBlockingQueue;"
            | "Ljava/util/concurrent/LinkedBlockingDeque;"
            | "Ljava/util/concurrent/ArrayBlockingQueue;"
            | "Ljava/util/concurrent/PriorityBlockingQueue;" | "Landroid/util/ArraySet;" => {
                HostState::List(Vec::new())
            }
            "Ljava/util/HashMap;" | "Ljava/util/LinkedHashMap;" | "Ljava/util/Hashtable;"
            | "Ljava/util/TreeMap;" | "Ljava/util/WeakHashMap;" | "Ljava/util/IdentityHashMap;"
            | "Ljava/util/EnumMap;" | "Ljava/util/Properties;" | "Ljava/util/AbstractMap;"
            | "Ljava/util/Collections$EmptyMap;" | "Ljava/util/Collections$SingletonMap;"
            | "Ljava/util/concurrent/ConcurrentHashMap;"
            | "Ljava/util/concurrent/ConcurrentSkipListMap;" | "Landroid/util/ArrayMap;"
            | "Landroid/util/SparseArray;" | "Landroid/util/SparseIntArray;"
            | "Landroid/util/SparseBooleanArray;" | "Landroid/util/LongSparseArray;"
            | "Landroid/util/LruCache;" | "Landroid/os/Bundle;" | "Landroid/os/BaseBundle;"
            | "Landroid/os/PersistableBundle;" | "Landroid/content/Intent;" => {
                HostState::Map(Vec::new())
            }
            "Ljava/util/Calendar;" | "Ljava/util/GregorianCalendar;" | "Ljava/util/Date;"
            | "Ljava/time/Instant;" | "Ljava/sql/Date;" | "Ljava/sql/Timestamp;"
            | "Ljava/sql/Time;" => HostState::Millis(now_ms),
            "Ljava/util/Random;" | "Ljava/security/SecureRandom;"
            | "Ljava/util/concurrent/ThreadLocalRandom;" | "Ljava/util/SplittableRandom;" => {
                HostState::Slot(V::I64(0x9E3779B97F4A7C15_u64 as i64))
            }
            "Ljava/lang/Integer;" | "Ljava/lang/Long;" | "Ljava/lang/Short;"
            | "Ljava/lang/Byte;" | "Ljava/lang/Float;" | "Ljava/lang/Double;"
            | "Ljava/lang/Boolean;" | "Ljava/lang/Character;" | "Ljava/lang/Void;"
            | "Ljava/lang/Thread;" | "Ljava/lang/ThreadLocal;"
            | "Ljava/lang/InheritableThreadLocal;" | "Ljava/lang/ref/WeakReference;"
            | "Ljava/lang/ref/SoftReference;" | "Ljava/lang/ref/PhantomReference;"
            | "Ljava/util/Optional;" | "Ljava/util/OptionalInt;" | "Ljava/util/OptionalLong;"
            | "Ljava/util/OptionalDouble;" | "Landroid/util/Pair;" | "Landroid/util/MutableInt;"
            | "Ljava/util/concurrent/atomic/AtomicReference;"
            | "Ljava/util/concurrent/atomic/AtomicInteger;"
            | "Ljava/util/concurrent/atomic/AtomicLong;"
            | "Ljava/util/concurrent/atomic/AtomicBoolean;"
            | "Ljava/util/concurrent/atomic/AtomicMarkableReference;"
            | "Ljava/util/concurrent/atomic/AtomicStampedReference;"
            | "Ljava/util/concurrent/atomic/AtomicReferenceFieldUpdater;"
            | "Ljava/util/concurrent/atomic/AtomicIntegerFieldUpdater;"
            | "Ljava/util/concurrent/atomic/AtomicLongFieldUpdater;"
            | "Ljava/util/concurrent/CountDownLatch;" | "Ljava/util/concurrent/Semaphore;"
            | "Landroid/os/Message;" => HostState::Slot(V::I32(0)),
            "Ljava/io/File;" => HostState::Path(String::new()),
            "Lorg/json/JSONObject;" => HostState::Map(Vec::new()),
            "Lorg/json/JSONArray;" => HostState::List(Vec::new()),
            "Ljava/util/regex/Pattern;" | "Ljava/util/regex/Matcher;"
            | "Ljava/text/SimpleDateFormat;" | "Ljava/text/DateFormat;"
            | "Ljava/text/DecimalFormat;" | "Ljava/text/NumberFormat;"
            | "Ljava/text/MessageFormat;" | "Ljava/time/format/DateTimeFormatter;" => {
                HostState::Regex(String::new())
            }
            _ => HostState::Empty,
        }
    }

    /// A static field on a host-provided class (called when the owner has
    /// no class_def and no stored value — `Build.VERSION.SDK_INT`,
    /// `StandardCharsets.UTF_8`, `Boolean.TRUE`, …).
    fn builtin_sfield(&mut self, f: GIdx) -> Option<V> {
        let d = self.dex_of(f)?;
        let fd = d.field(gl(f)).ok()?;
        let owner = d.type_descriptor(fd.class_idx as u32).ok()?;
        let name = d.string(fd.name_idx).ok()?;
        let fty = d
            .type_descriptor(fd.type_idx as u32)
            .unwrap_or_default();
        jdk::sfield(self, &owner, &name, &fty)
    }

    /// A named field exposed by a host object (`PackageInfo.versionCode`,
    /// `Message.what`, `Pair.first`…) — read when `iget` misses `fields`.
    fn builtin_ifield(&self, h: u32, f: GIdx) -> Option<V> {
        let Some(Obj::Host { .. }) = self.heap.get(h) else {
            return None;
        };
        let d = self.dex_of(f)?;
        let fd = d.field(gl(f)).ok()?;
        let owner = d.type_descriptor(fd.class_idx as u32).ok()?;
        let name = d.string(fd.name_idx).ok()?;
        jdk::ifield(self, h, &owner, &name)
    }

    /// Descriptor-level assignability for host objects whose class_idx is
    /// absent from the DEX: consults the builtin super tables.
    fn assignable_desc(&self, sub: &str, sup: &str) -> bool {
        if sub == sup || sup == "Ljava/lang/Object;" {
            return true;
        }
        if Self::builtin_supers(sub).contains(&sup) || jdk::host_supers(sub).contains(&sup) {
            return true;
        }
        let mut d = sub.to_string();
        for _ in 0..32 {
            let Some(next) = Self::framework_super(&d) else {
                return false;
            };
            if next == sup {
                return true;
            }
            d = next.to_string();
        }
        false
    }

    /// Element-type assignability for array covariance: `sub[]` casts to
    /// `sup[]` when each element of `sub` is assignable to `sup`. Only
    /// reference types are covariant — `[I` never matches `[J` or
    /// `[Ljava/lang/Object;`.
    fn elem_assignable(&self, sub: &str, sup: &str) -> bool {
        if sub == sup {
            return true;
        }
        match (sub.strip_prefix('['), sup.strip_prefix('[')) {
            (Some(s), Some(p)) => self.elem_assignable(s, p),
            (Some(_), None) | (None, Some(_)) => false,
            (None, None) => {
                if !sub.starts_with('L') || !sup.starts_with('L') {
                    return false; // primitive element: equality only
                }
                if sup == "Ljava/lang/Object;"
                    || sup == "Ljava/lang/Cloneable;"
                    || sup == "Ljava/io/Serializable;"
                {
                    return true;
                }
                // Dex-class element → walk its hierarchy for the sup.
                if let (Some(a), Some(b)) =
                    (self.type_index_of(sub), self.type_index_of(sup))
                {
                    if self.assignable(a, b) {
                        return true;
                    }
                }
                self.assignable_desc(sub, sup)
            }
        }
    }

    /// Is `h` (non-null) an instance of `type_idx`'s descriptor?
    fn is_instance(&self, h: u32, type_idx: GIdx) -> bool {
        let Ok(sup) = self.type_desc_of(type_idx) else {
            return false;
        };
        match self.heap.get(h) {
            Some(Obj::Instance { class_idx, .. }) => self.assignable(*class_idx, type_idx),
            Some(Obj::Host {
                class_idx,
                desc,
                fields,
                ..
            }) => {
                // Inflated views may carry a class the hierarchy model
                // can't trace (custom tags, unknown androidx widgets); the
                // inflater marks their real kind on pseudo fields.
                let marked = |key: u32| fields.contains_key(&(key as u64));
                (*class_idx != GX_NONE && self.assignable(*class_idx, type_idx))
                    || self.assignable_desc(desc, &sup)
                    || (sup == "Landroid/view/View;" && marked(jdk::view::P_IS_VIEW))
                    || (sup == "Landroid/view/ViewGroup;" && marked(jdk::view::P_IS_GROUP))
            }
            Some(Obj::Throwable { descriptor, .. }) => {
                if descriptor == &sup {
                    return true;
                }
                // builtin_supers is the java.* table — framework
                // exception hierarchies (android.database.*) live in
                // host_supers and are reachable via assignable_desc.
                Self::builtin_supers(descriptor).contains(&sup.as_str())
                    || self.assignable_desc(descriptor, &sup)
            }
            Some(Obj::Str(_)) => {
                sup == "Ljava/lang/String;"
                    || sup == "Ljava/lang/Object;"
                    || sup == "Ljava/lang/CharSequence;"
                    || sup == "Ljava/lang/Comparable;"
                    || sup == "Ljava/io/Serializable;"
            }
            Some(Obj::Class { .. }) => {
                sup == "Ljava/lang/Class;" || sup == "Ljava/lang/Object;"
            }
            Some(Obj::Array { elem, .. }) => {
                sup == "Ljava/lang/Object;"
                    || sup == "Ljava/lang/Cloneable;"
                    || sup == "Ljava/io/Serializable;"
                    || (sup.starts_with('[') && self.elem_assignable(elem, &sup[1..]))
            }
            None => false,
        }
    }

    /// Allocate a builtin throwable.
    pub(crate) fn throw_builtin(&mut self, descriptor: &str, detail: &str) -> E {
        let h = self.heap.alloc(Obj::Throwable {
            descriptor: descriptor.to_string(),
            detail: detail.to_string(),
        });
        // Runtime-generated throws carry no `throw` insn — credit the
        // frame that produced it so diagnostics name the method.
        if let Some(&midx) = self.stack.last() {
            let site = self.describe_method(midx);
            self.throw_sites.insert(h, site);
        }
        E::Throw(h)
    }

    /// Run `<clinit>` once per class, superclass first (reentrancy-safe).
    /// `clinit` states: 0 = new, 1 = running, 2 = done, 3 = failed.
    fn ensure_init(&mut self, class_idx: GIdx) -> Rt<()> {
        let class_idx = self.canon(class_idx);
        let rt = self.classes.entry(class_idx).or_default();
        match rt.clinit {
            2 => return Ok(()),
            1 => return Ok(()), // recursive init: in progress
            3 => {
                return Err(self.throw_builtin(
                    "Ljava/lang/NoClassDefFoundError;",
                    "class initialization previously failed",
                ))
            }
            _ => {}
        }
        self.classes.get_mut(&class_idx).unwrap().clinit = 1;
        let fail = |this: &mut Self, e: E| -> Rt<()> {
            this.classes.get_mut(&class_idx).unwrap().clinit = 3;
            Err(e)
        };
        // Superclasses initialize before their subclasses. `sup` is a
        // type_idx local to the defining dex — canon resolves it across
        // dexes by descriptor.
        let sup = self
            .def_loc_of(class_idx)
            .map(|(cg, d)| gx(gd(cg), d.superclass_idx))
            .filter(|&s| gl(s) != u32::MAX)
            .map(|s| self.canon(s));
        if let Some(sup) = sup.filter(|&s| s != class_idx) {
            if let Err(e) = self.ensure_init(sup) {
                return fail(self, e);
            }
        }
        let cd = match self.class_data_of(class_idx) {
            Ok(cd) => cd,
            Err(_) => {
                self.classes.get_mut(&class_idx).unwrap().clinit = 2;
                return Ok(());
            }
        };
        if let Some(cd) = cd {
            let di = self
                .def_loc_of(class_idx)
                .map(|(cg, _)| gd(cg))
                .unwrap_or(usize::MAX);
            let Some(d) = self.dexes.get(di) else {
                self.classes.get_mut(&class_idx).unwrap().clinit = 2;
                return Ok(());
            };
            for m in &cd.direct_methods {
                let mid = d.method(m.method_idx).map_err(E::Vm)?;
                if d.string(mid.name_idx).map_err(E::Vm)? == "<clinit>" {
                    if let Err(e) = self.exec(
                        Loc {
                            class_idx,
                            midx: gx(di, m.method_idx),
                            em: *m,
                        },
                        Vec::new(),
                    ) {
                        return fail(self, e);
                    }
                    break;
                }
            }
        }
        self.classes.get_mut(&class_idx).unwrap().clinit = 2;
        Ok(())
    }

    /// Method lookup by method table index. The ref's dex and the
    /// defining dex may differ: fall back to `(class, name, sig)` match.
    fn resolve_defined(&self, midx: GIdx) -> Rt<Loc> {
        if let Some(&(c, em)) = self.defined.get(&midx) {
            return Ok(Loc {
                class_idx: c,
                midx,
                em,
            });
        }
        if let Ok((class, name, sig)) = self.method_sig(midx) {
            if let Some(&g) = self.methods_by_sig.get(&(class, name, sig)) {
                if let Some(&(c, em)) = self.defined.get(&g) {
                    return Ok(Loc {
                        class_idx: c,
                        midx: g,
                        em,
                    });
                }
            }
        }
        Err(E::Vm(err("dexrt.invoke.unresolved")))
    }

    /// Virtual/interface dispatch: find the override in `start_class`'s
    /// virtual table matching the target's (name, proto). Name and proto
    /// indexes are dex-local — compare descriptors, never raw indexes.
    fn resolve_virtual(&self, midx: GIdx, start_class: GIdx, fallback: bool) -> Rt<Loc> {
        let Ok((_, name, sig)) = self.method_sig(midx) else {
            return Err(E::Vm(err("dexrt.invoke.unresolved")));
        };
        let mut cur = Some(start_class);
        for _ in 0..64 {
            let Some(ci) = cur else { break };
            let Some((cg, def)) = self.def_loc_of(ci) else {
                break;
            };
            let Some(d) = self.dex_of(cg) else { break };
            if let Ok(cd) = d.class_data(def) {
                for m in &cd.virtual_methods {
                    let Ok(om) = d.method(m.method_idx) else { continue };
                    let oname = d.string(om.name_idx).unwrap_or_default();
                    if oname == name
                        && proto_sig(d, om.proto_idx as u32).unwrap_or_default() == sig
                    {
                        return Ok(Loc {
                            class_idx: cg,
                            midx: gx(gd(cg), m.method_idx),
                            em: *m,
                        });
                    }
                }
            }
            cur = (def.superclass_idx != u32::MAX).then(|| gx(gd(cg), def.superclass_idx));
        }
        // Fall back to the declared method itself (e.g. dispatch target is the
        // declared class — a class that doesn't override).
        if fallback {
            self.resolve_defined(midx)
        } else {
            Err(E::Vm(err("dexrt.invoke.unresolved")))
        }
    }

    /// Builtin method table: (class descriptor, name) — never silent success,
    /// only listed signatures are served. Registered host methods win.
    /// Host stub lookup by `(name, proto)` — the DEX may reference a
    /// framework method through a subclass descriptor the exact-midx
    /// registration doesn't cover (e.g. `LSubAct;.setContentView`).
    fn host_named(&self, midx: GIdx) -> Rt<Option<HostCall>> {
        let Some(d) = self.dex_of(midx) else {
            return Ok(None);
        };
        let mid = *d.method(gl(midx)).map_err(E::Vm)?;
        let name = d.string(mid.name_idx).map_err(E::Vm)?;
        let sig = proto_sig(d, mid.proto_idx as u32).map_err(E::Vm)?;
        // The reference's declaring class must be the registered owner
        // or a subclass of it — otherwise an unrelated class naming the
        // same method (e.g. `Dialog.setContentView` when only
        // `Activity.setContentView` is bridged) would steal the call.
        let key = (name.to_string(), sig);
        let owner_ref = gx(gd(midx), mid.class_idx as u32);
        for (owner, f) in self.hosts_named.get(&key).into_iter().flatten() {
            if *owner == owner_ref || self.assignable(owner_ref, *owner) {
                return Ok(Some(f.clone()));
            }
        }
        Ok(None)
    }

    /// Compact `args` for a builtin: DEX passes `J`/`D` params as
    /// two mirrored register slots, and float bits arrive as `I32`.
    /// Builtins index args per declared parameter, so rebuild the
    /// list walking the proto — receiver first when present.
    fn compact_args(&self, midx: GIdx, args: &[V], has_receiver: bool) -> Vec<V> {
        let mut out: Vec<V> = Vec::with_capacity(args.len());
        let mut i = 0;
        if has_receiver {
            out.push(args.first().copied().unwrap_or(V::Ref(0)));
            i = 1;
        }
        let Some(d) = self.dex_of(midx) else {
            return args.to_vec();
        };
        let Ok(mid) = d.method(gl(midx)) else {
            return args.to_vec();
        };
        let Ok(p) = d.proto(mid.proto_idx as u32) else {
            return args.to_vec();
        };
        let Ok(ps) = d.proto_params(p) else {
            return args.to_vec();
        };
        for ty in ps {
            let Ok(desc) = d.type_descriptor(ty as u32) else {
                return args.to_vec();
            };
            let v = args.get(i).copied().unwrap_or(V::I32(0));
            let v = match desc.as_str() {
                // const f32 bits live in an int register at the boundary.
                "F" => match v {
                    V::I32(b) => V::F32(f32::from_bits(b as u32)),
                    x => x,
                },
                "D" => match v {
                    V::I64(b) => V::F64(f64::from_bits(b as u64)),
                    V::I32(b) => V::F64(f64::from_bits(b as u32 as u64)),
                    x => x,
                },
                "J" => match v {
                    V::I32(b) => V::I64(b as i64),
                    x => x,
                },
                _ => v,
            };
            out.push(v);
            i += if matches!(desc.as_str(), "J" | "D") {
                2
            } else {
                1
            };
        }
        out
    }

    fn call_builtin(&mut self, midx: GIdx, args: &[V], has_receiver: bool) -> Rt<Option<V>> {
        let args = self.compact_args(midx, args, has_receiver);
        let args = args.as_slice();
        let host = self
            .hosts
            .get(&midx)
            .cloned()
            .or(self.host_named(midx)?);
        if let Some(f) = host {
            let Ok(mut g) = f.try_borrow_mut() else {
                return vm("dexrt.host.reentrant");
            };
            return g(self, args).map_err(E::Vm);
        }
        let Some(d) = self.dex_of(midx) else {
            return vm("dexrt.invoke.unresolved");
        };
        let mid = *d.method(gl(midx)).map_err(E::Vm)?;
        let class = d.type_descriptor(mid.class_idx as u32).map_err(E::Vm)?;
        let name = d.string(mid.name_idx).map_err(E::Vm)?;
        match (class.as_str(), name.as_str()) {
            ("Ljava/lang/Object;", "<init>") => Ok(None),
            ("Ljava/lang/Object;", "getClass") => {
                let this = args.first().copied().unwrap_or(V::Ref(0));
                let ci = match this {
                    V::Ref(h) if h != 0 => self.heap.class_of(h).unwrap_or(0),
                    _ => 0,
                };
                Ok(Some(V::Ref(self.class_mirror(ci))))
            }
            ("Ljava/lang/String;", "length") => {
                if let V::Ref(h) = args.first().copied().unwrap_or(V::Ref(0)) {
                    if let Some(s) = self.heap.str_val(h) {
                        return Ok(Some(V::I32(s.chars().count() as i32)));
                    }
                }
                Err(self.throw_builtin("Ljava/lang/NullPointerException;", "String.length"))
            }
            _ => {
                let ret_desc;
                {
                    let p = d.proto(mid.proto_idx as u32).map_err(E::Vm)?;
                    ret_desc = d.type_descriptor(p.return_type_idx).map_err(E::Vm)?;
                }
                let sig = proto_sig(d, mid.proto_idx as u32).map_err(E::Vm)?;
                // JDK/framework builtins cover real behavior for the
                // common java.* / android.* surface.
                if let Some(r) = jdk::call(self, &class, &name, &sig, args)? {
                    return Ok(r);
                }
                // A method whose class isn't in this DEX is a framework
                // stub: the host models the base behavior, so return the
                // proto's default (None for void) instead of failing —
                // e.g. Activity.<init> or super.onCreate().
                // Only constructors and void methods on non-DEX classes are
                // host no-ops — a value-returning stub (Math.abs) answering
                // with a default would let the app compute on a lie.
                if self.def_of(gx(gd(midx), mid.class_idx as u32)).is_none() {
                    self.stub_placeholder(midx, &ret_desc, &name, &class, args)
                } else {
                    vm("dexrt.invoke.unresolved").map_err(|e| match e {
                        E::Vm(v) => E::Vm(v.with_detail(self.describe_method(midx))),
                        t => t,
                    })
                }
            }
        }
    }

    /// Placeholder return for a call that cannot produce a real result —
    /// an unresolved non-dex method or a bodiless declaration (native /
    /// external / abstract decl invoked statically). Same rules either
    /// way: void → None, refs get a typed host/array placeholder,
    /// primitives a typed zero.
    fn stub_placeholder(
        &mut self,
        midx: GIdx,
        ret: &str,
        name: &str,
        class: &str,
        args: &[V],
    ) -> Rt<Option<V>> {
        let trace = self.describe_method(midx);
        if self.stub_trace.len() >= 32 {
            self.stub_trace.remove(0);
        }
        self.stub_trace.push(trace);
        // Throwable.<init>(message): keep the message on the
        // host object so describe_throw can report *why*.
        if name == "<init>"
            && (class.ends_with("Exception;")
                || class.ends_with("Error;")
                || class.ends_with("Throwable;"))
        {
            if let (V::Ref(this), Some(&msg)) =
                (args.first().copied().unwrap_or(V::Ref(0)), args.get(1))
            {
                jdk::pseudo_set(self, this, jdk::P_DATA, msg);
            }
        }
        match ret {
            "V" => Ok(None),
            // Object/array returns get a typed placeholder so
            // chained framework calls cascade (`getLogger(x)
            // .info(y)`); methods on it hit this same stub
            // path. Strings come back empty — a placeholder
            // would break the String builtins.
            "Ljava/lang/String;" => Ok(Some(V::Ref(self.heap.intern_str("")))),
            r if r.starts_with('[') => {
                // Array return: must be a real Obj::Array or
                // array-length/a*get on the value type-checks.
                let h = self.heap.alloc(Obj::Array {
                    elem: r[1..].to_string(),
                    data: Vec::new(),
                });
                Ok(Some(V::Ref(h)))
            }
            r if r.starts_with('L') => {
                let desc = r.to_string();
                let ci = self.type_index_of(&desc).unwrap_or(GX_NONE);
                let h = self.heap.alloc(Obj::Host {
                    class_idx: ci,
                    desc,
                    fields: HashMap::new(),
                    state: HostState::Empty,
                });
                Ok(Some(V::Ref(h)))
            }
            // Primitive returns answer a typed zero — service
            // getters like `areNotificationsEnabled()` read as
            // "not enabled", which is honest here.
            "J" => Ok(Some(V::I64(0))),
            "F" => Ok(Some(V::F32(0.0))),
            "D" => Ok(Some(V::F64(0.0))),
            _ => Ok(Some(V::I32(0))),
        }
    }

    /// Invoke `midx` per `kind`; `args` includes the receiver for non-static.
    fn invoke(&mut self, midx: GIdx, args: Vec<V>, kind: InvokeKind) -> Rt<Option<V>> {
        let loc = match kind {
            InvokeKind::Static | InvokeKind::Direct => {
                match self.resolve_defined(midx) {
                    Ok(l) => l,
                    Err(_) => {
                        return self.call_builtin(midx, &args, matches!(kind, InvokeKind::Direct));
                    }
                }
            }
            InvokeKind::Virtual | InvokeKind::Interface => {
                let recv = match args.first() {
                    Some(&V::Ref(h)) if h != 0 => h,
                    _ => {
                        return Err(self.throw_builtin(
                            "Ljava/lang/NullPointerException;",
                            &format!(
                                "null receiver calling {}",
                                self.describe_method(midx)
                            ),
                        ))
                    }
                };
                let ci = self.heap.class_of(recv).unwrap_or(GX_NONE);
                match self.resolve_virtual(midx, ci, true) {
                    Ok(l) => l,
                    Err(_) => return self.call_builtin(midx, &args, true),
                }
            }
            InvokeKind::Super { caller_class } => {
                let sup = self
                    .def_loc_of(caller_class)
                    .map(|(cg, d)| gx(gd(cg), d.superclass_idx))
                    .filter(|&s| gl(s) != u32::MAX)
                    .unwrap_or(GX_NONE);
                // No defined-method fallback: an operand that names the
                // caller's own method would recurse forever.
                match self.resolve_virtual(midx, sup, false) {
                    Ok(l) => l,
                    Err(_) => return self.call_builtin(midx, &args, true),
                }
            }
        };
        // A registered host override short-circuits even a dex-defined
        // method — used for compat shims on library code that can't run
        // here (e.g. appcompat's vector-drawable path).
        if let Some(f) = self.hosts.get(&loc.midx).cloned() {
            let has_recv = !matches!(kind, InvokeKind::Static);
            let args = self.compact_args(loc.midx, &args, has_recv);
            let Ok(mut g) = f.try_borrow_mut() else {
                return vm("dexrt.host.reentrant");
            };
            return g(self, &args).map_err(E::Vm);
        }
        if matches!(kind, InvokeKind::Static) {
            self.ensure_init(loc.class_idx)?;
        }
        if loc.em.code_off == 0 {
            // Bodiless declaration (native / external / abstract decl):
            // jdk shims may still answer it; otherwise a typed
            // placeholder keeps a compat run moving — real JNI bridging
            // arrives with the native stack.
            let (class, name, sig, ret_desc) = {
                let d = self
                    .dex_of(loc.midx)
                    .ok_or_else(|| E::Vm(err("dexrt.dex.idx")))?;
                let mid = *d.method(gl(loc.midx)).map_err(E::Vm)?;
                (
                    d.type_descriptor(mid.class_idx as u32).map_err(E::Vm)?,
                    d.string(mid.name_idx).map_err(E::Vm)?,
                    proto_sig(d, mid.proto_idx as u32).map_err(E::Vm)?,
                    d.proto(mid.proto_idx as u32)
                        .and_then(|p| d.type_descriptor(p.return_type_idx))
                        .map_err(E::Vm)?,
                )
            };
            let args =
                self.compact_args(loc.midx, &args, !matches!(kind, InvokeKind::Static));
            if let Some(r) = jdk::call(self, &class, &name, &sig, &args)? {
                return Ok(r);
            }
            return self.stub_placeholder(loc.midx, &ret_desc, &name, &class, &args);
        }
        if let Some(f) = self.aot.as_ref().and_then(|a| a.get(loc.midx)) {
            return self.run_native(f, loc.midx, loc.em.code_off, args);
        }
        self.exec(loc, args)
    }

    /// Run a compiled method under the shared register/ABI contract
    /// (see `aot` module docs). A `TRAP_DIV_ZERO` flag becomes the same
    /// `ArithmeticException` the interpreter raises.
    fn run_native(&mut self, f: AotFn, midx: GIdx, code_off: u32, args: Vec<V>) -> Rt<Option<V>> {
        let code = self
            .dex_of(midx)
            .ok_or_else(|| E::Vm(err("dexrt.dex.idx")))?
            .code_item(code_off)
            .map_err(E::Vm)?
            .ok_or_else(|| E::Vm(err("dexrt.method.no_code")))?;
        let nregs = code.registers_size as usize;
        let start = nregs.saturating_sub(code.ins_size as usize);
        let mut regs = vec![0i64; nregs];
        for (i, a) in args.iter().enumerate() {
            if start + i >= nregs {
                break;
            }
            regs[start + i] = match a {
                V::I32(v) => *v as i64,
                V::I64(v) => *v,
                V::F32(v) => v.to_bits() as i64,
                V::F64(v) => v.to_bits() as i64,
                V::Ref(h) => *h as i64,
            };
        }
        let mut trap: u8 = 0;
        self.stack.push(midx);
        #[allow(unsafe_code)]
        let ret = unsafe { f(regs.as_mut_ptr(), &mut trap) };
        self.stack.pop();
        let site = self.describe_method(midx);
        let trap_throw = |v: &mut Self, desc: &str| {
            let h = v.heap.alloc(crate::value::Obj::Throwable {
                descriptor: "Ljava/lang/ArithmeticException;".into(),
                detail: desc.into(),
            });
            v.throw_sites.insert(h, site.clone());
            Err(E::Throw(h))
        };
        match trap {
            TRAP_DIV_ZERO => return trap_throw(self, "divide by zero"),
            TRAP_REM_ZERO => return trap_throw(self, "rem by zero"),
            _ => {}
        }
        let ret_desc = self
            .dex_of(midx)
            .and_then(|d| {
                d.method(gl(midx))
                    .ok()
                    .and_then(|m| d.proto(m.proto_idx as u32).ok().map(|p| p.return_type_idx))
                    .and_then(|t| d.type_descriptor(t).ok())
            })
            .unwrap_or_else(|| "V".into());
        Ok(match ret_desc.as_str() {
            "V" => None,
            "J" => Some(V::I64(ret)),
            _ => Some(V::I32(ret as i32)),
        })
    }

    /// Execute one frame to return or throw.
    fn exec(&mut self, loc: Loc, args: Vec<V>) -> Rt<Option<V>> {
        if self.depth >= 512 {
            return Err(self.throw_builtin(
                "Ljava/lang/StackOverflowError;",
                "call depth exceeded",
            ));
        }
        self.depth += 1;
        self.stack.push(loc.midx);
        let r = self.exec_frame(loc, args);
        self.stack.pop();
        self.depth -= 1;
        r
    }

    fn exec_frame(&mut self, loc: Loc, args: Vec<V>) -> Rt<Option<V>> {
        let code = self
            .dex_of(loc.midx)
            .ok_or_else(|| E::Vm(err("dexrt.dex.idx")))?
            .code_item(loc.em.code_off)
            .map_err(E::Vm)?
            .ok_or_else(|| E::Vm(err("dexrt.method.no_code")))?;
        let nregs = code.registers_size as usize;
        let mut fr = Frame {
            regs: vec![V::I32(0); nregs],
            result: None,
            last_call: String::new(),
            exc: None,
            table: decode_all(&code.insns).map_err(E::Vm)?,
            loc,
            code,
        };
        // Copy args into the last `ins_size` registers.
        let ins = fr.code.ins_size as usize;
        let start = nregs.saturating_sub(ins);
        for (i, a) in args.iter().enumerate() {
            if start + i < nregs {
                fr.regs[start + i] = *a;
            }
        }
        let mut pc: usize = 0;
        // Instruction budget — a guest (or stub-driven) loop that never
        // terminates must fail loudly, not hang the host process.
        let stalldbg = std::env::var_os("OVELO_STALLDBG").is_some();
        let mut steps: u64 = 0;
        loop {
            steps += 1;
            if stalldbg && steps.is_multiple_of(5_000_000) {
                eprintln!(
                    "[stall] {} +{pc:04x} steps={steps}",
                    self.describe_method(fr.loc.midx)
                );
            }
            self.total_steps += 1;
            if stalldbg && self.total_steps.is_multiple_of(1_000_000) {
                eprintln!(
                    "[stall-total] {} +{pc:04x} total={}",
                    self.describe_method(fr.loc.midx),
                    self.total_steps
                );
            }
            if steps > 20_000_000 {
                return Err(self.throw_builtin(
                    "Ljava/lang/InternalError;",
                    &format!("instruction budget exceeded at +{pc:04x}"),
                ));
            }
            let res = self.step(&mut fr, pc);
            match res {
                Ok(Out::Next(n)) => pc = n,
                Ok(Out::Return(v)) => return Ok(v),
                Err(E::Vm(e)) => {
                    if self.trace_throw {
                        eprintln!(
                            "[vmerr] {} +{pc:04x}: {e}",
                            self.describe_method(fr.loc.midx)
                        );
                    }
                    return Err(E::Vm(e));
                }
                Err(E::Throw(r)) => match self.find_handler(&fr, pc, r) {
                    _ if self.trace_throw => {
                        let d = match self.heap.get(r) {
                            Some(Obj::Throwable { descriptor, detail }) => {
                                format!("{descriptor} {detail}")
                            }
                            Some(Obj::Host { desc, .. }) => desc.clone(),
                            Some(Obj::Instance { class_idx, .. }) => self
                                .type_desc_of(*class_idx)
                                .unwrap_or_else(|_| format!("instance(class_idx={class_idx})")),
                            _ => format!("h{r}"),
                        };
                        eprintln!(
                            "[throw] {} +{pc:04x} -> {d}",
                            self.describe_method(fr.loc.midx)
                        );
                        match self.find_handler(&fr, pc, r) {
                            Some(addr) => {
                                pc = addr as usize;
                                fr.exc = Some(r);
                            }
                            None => return Err(E::Throw(r)),
                        }
                    }
                    Some(addr) => {
                        pc = addr as usize;
                        fr.exc = Some(r);
                    }
                    None => return Err(E::Throw(r)),
                },
            }
        }
    }

    /// Locate a catch handler for `pc` throwing object `h`.
    fn find_handler(&self, fr: &Frame, pc: usize, h: u32) -> Option<u32> {
        for t in &fr.code.tries {
            let TryItem {
                start_addr,
                insn_count,
                handler_idx,
            } = *t;
            let end = start_addr + insn_count as u32;
            if (pc as u32) < start_addr || (pc as u32) >= end {
                continue;
            }
            let hnd = fr.code.handlers.get(handler_idx)?;
            // Handler type indexes are local to the frame's dex.
            let di = gd(fr.loc.midx);
            for (ty, addr) in &hnd.handlers {
                if self.is_instance(h, gx(di, *ty)) {
                    return Some(*addr);
                }
            }
            if let Some(a) = hnd.catch_all_addr {
                return Some(a);
            }
        }
        None
    }
}

/// What the dispatcher decided for the current pc.
enum Out {
    /// Next pc in code units.
    Next(usize),
    /// Method returned.
    Return(Option<V>),
}

struct Frame {
    regs: Vec<V>,
    /// `move-result` source: set by the last invoke.
    result: Option<Option<V>>,
    /// Method the last invoke targeted — names the callee when a
    /// `move-result` reads a void result.
    last_call: String,
    /// Pending exception for `move-exception`.
    exc: Option<u32>,
    loc: Loc,
    code: ovelo_dex::CodeItem,
    /// Decoded instructions (payloads included), in offset order.
    table: Vec<Insn>,
}

#[derive(Clone, Copy)]
enum InvokeKind {
    Static,
    Direct,
    Virtual,
    Interface,
    /// Dispatch begins at the caller's superclass.
    Super { caller_class: GIdx },
}

impl Vm {
    /// Set a possibly-wide value: mirrors into `regs[i+1]` for `I64`/`F64`.
    fn set(fr: &mut Frame, i: usize, v: V) {
        if let Some(r) = fr.regs.get_mut(i) {
            *r = v;
        }
        if matches!(v, V::I64(_) | V::F64(_)) {
            if let Some(r2) = fr.regs.get_mut(i + 1) {
                *r2 = v;
            }
        }
    }

    fn reg(fr: &Frame, i: usize) -> Rt<V> {
        fr.regs
            .get(i)
            .copied()
            .ok_or_else(|| E::Vm(err("dexrt.reg.bounds")))
    }

    fn i32v(v: V) -> Rt<i32> {
        match v {
            V::I32(i) => Ok(i),
            _ => vm("dexrt.type"),
        }
    }
    fn i64v(v: V) -> Rt<i64> {
        match v {
            V::I64(i) => Ok(i),
            _ => vm("dexrt.type"),
        }
    }
    fn f32v(v: V) -> Rt<f32> {
        match v {
            V::F32(f) => Ok(f),
            V::I32(i) => Ok(f32::from_bits(i as u32)),
            _ => vm("dexrt.type"),
        }
    }
    fn f64v(v: V) -> Rt<f64> {
        match v {
            V::F64(f) => Ok(f),
            V::I64(i) => Ok(f64::from_bits(i as u64)),
            _ => vm("dexrt.type"),
        }
    }
    fn refv(v: V) -> Rt<u32> {
        match v {
            V::Ref(h) => Ok(h),
            // `const/4 vx, 0` is how dex puts a null reference in a register.
            V::I32(0) => Ok(0),
            _ => vm("dexrt.type"),
        }
    }

    fn null_check(&mut self, h: u32, what: &str) -> Rt<()> {
        if h == 0 {
            Err(self.throw_builtin("Ljava/lang/NullPointerException;", what))
        } else {
            Ok(())
        }
    }

    /// One instruction at `pc` in `fr`.
    fn step(&mut self, fr: &mut Frame, pc: usize) -> Rt<Out> {
        let pos = fr
            .table
            .iter()
            .position(|i| i.offset as usize == pc)
            .ok_or_else(|| E::Vm(err("dexrt.pc.bounds")))?;
        let ins = fr.table[pos].clone();
        let next = fr
            .table
            .get(pos + 1)
            .map(|i| i.offset as usize)
            .unwrap_or(pc + 1);
        let op = ins.opcode;
        let regs = &ins.regs;
        let r = |i: usize| Self::reg(fr, regs.get(i).copied().unwrap_or(0) as usize);
        // Instruction indexes are dex-local; `di` is the frame's dex.
        let di = gd(fr.loc.midx);
        let gi = |i: u32| gx(di, i);

        // helpers for typed results
        macro_rules! set {
            ($i:expr, $v:expr) => {
                Self::set(fr, $i as usize, $v)
            };
        }

        match op {
            0x00 => {} // nop
            // ---- moves ----
            0x01..=0x03 | 0x07..=0x09 => {
                let (a, b) = (regs[0] as usize, regs[1] as usize);
                let v = Self::reg(fr, b)?;
                Self::set(fr, a, v);
            }
            0x04..=0x06 => {
                let (a, b) = (regs[0] as usize, regs[1] as usize);
                let v = Self::reg(fr, b)?;
                Self::set(fr, a, v);
            }
            0x0a..=0x0c => {
                let v = fr
                    .result
                    .take()
                    .ok_or_else(|| E::Vm(err("dexrt.result.missing")))?
                    .ok_or_else(|| {
                        E::Vm(err(&format!(
                            "dexrt.result.void {}",
                            fr.last_call
                        )))
                    })?;
                set!(regs[0], v);
            }
            0x0d => {
                let e = fr
                    .exc
                    .take()
                    .ok_or_else(|| E::Vm(err("dexrt.exc.missing")))?;
                set!(regs[0], V::Ref(e));
            }
            // ---- returns ----
            0x0e => return Ok(Out::Return(None)),
            0x0f..=0x11 => return Ok(Out::Return(Some(r(0)?))),
            // ---- consts ----
            0x12..=0x14 => set!(regs[0], V::I32(ins.lit.unwrap_or(0) as i32)),
            0x15 => set!(regs[0], V::I32((ins.lit.unwrap_or(0) << 16) as i32)),
            0x16..=0x18 => set!(regs[0], V::I64(ins.lit.unwrap_or(0))),
            0x19 => set!(regs[0], V::I64(ins.lit.unwrap_or(0) << 48)),
            0x1a | 0x1b => {
                let s = self.dexes[di]
                    .string(ins.idx.unwrap_or(0))
                    .map_err(E::Vm)?;
                let h = self.heap.intern_str(&s);
                set!(regs[0], V::Ref(h));
            }
            0x1c => {
                let h = self.class_mirror(gi(ins.idx.unwrap_or(0)));
                set!(regs[0], V::Ref(h));
            }
            // ---- monitors: single-threaded no-ops (null still faults) ----
            0x1d | 0x1e => {
                let h = Self::refv(r(0)?)?;
                self.null_check(h, "monitor")?;
            }
            // ---- casts ----
            0x1f => {
                let h = Self::refv(r(0)?)?;
                // Opaque placeholders (empty stubs we minted to satisfy a
                // return type) carry no verifiable class — let the cast
                // pass. `instanceof`/catch matching stay strict.
                let opaque_stub = matches!(
                    self.heap.get(h),
                    Some(Obj::Host {
                        state: HostState::Empty,
                        ..
                    })
                );
                if h != 0 && !opaque_stub && !self.is_instance(h, gi(ins.idx.unwrap_or(0))) {
                    let have = match self.heap.get(h) {
                        Some(Obj::Array { elem, .. }) => format!("[{elem}"),
                        Some(Obj::Host { desc, .. }) => desc.clone(),
                        Some(Obj::Throwable { descriptor, .. }) => descriptor.clone(),
                        Some(Obj::Str(_)) => "Ljava/lang/String;".to_string(),
                        Some(Obj::Class { .. }) => "Ljava/lang/Class;".to_string(),
                        Some(Obj::Instance { class_idx, .. }) => self
                            .type_desc_of(*class_idx)
                            .unwrap_or_else(|_| "?".to_string()),
                        _ => "?".to_string(),
                    };
                    let want = self
                        .type_desc_of(gi(ins.idx.unwrap_or(0)))
                        .unwrap_or_else(|_| "?".to_string());
                    return Err(self.throw_builtin(
                        "Ljava/lang/ClassCastException;",
                        &format!("check-cast {have} -> {want}"),
                    ));
                }
            }
            0x20 => {
                let h = Self::refv(r(1)?)?;
                let res = h != 0 && self.is_instance(h, gi(ins.idx.unwrap_or(0)));
                set!(regs[0], V::I32(res as i32));
            }
            0x21 => {
                let h = Self::refv(r(1)?)?;
                self.null_check(h, "array-length")?;
                let n = match self.heap.get(h) {
                    Some(Obj::Array { data, .. }) => data.len() as i32,
                    _ => return vm("dexrt.type"),
                };
                set!(regs[0], V::I32(n));
            }
            0x22 => {
                let ty = gi(ins.idx.unwrap_or(0));
                self.ensure_init(ty)?;
                // A type with no class_def is a host-provided framework/JDK
                // class: allocate a Host object whose builtin state matches
                // the descriptor (StringBuilder buffer, collection, …) —
                // methods dispatch to builtins via call_builtin.
                let h = if self.def_of(ty).is_none() {
                    let desc = self.type_desc_of(ty).map_err(E::Vm)?;
                    let state = Self::host_state_for(&desc);
                    self.heap.alloc(Obj::Host {
                        class_idx: self.canon(ty),
                        desc,
                        fields: HashMap::new(),
                        state,
                    })
                } else {
                    self.heap.alloc(Obj::Instance {
                        class_idx: self.canon(ty),
                        fields: HashMap::new(),
                    })
                };
                set!(regs[0], V::Ref(h));
            }
            0x23 => {
                let len = Self::i32v(r(1)?)?;
                if len < 0 {
                    return Err(self.throw_builtin(
                        "Ljava/lang/NegativeArraySizeException;",
                        "new-array",
                    ));
                }
                let desc = self
                    .dexes[di]
                    .type_descriptor(ins.idx.unwrap_or(0))
                    .map_err(E::Vm)?;
                let elem = desc.trim_start_matches('[').to_string();
                let h = self.heap.alloc(Obj::Array {
                    data: vec![V::default_for(&elem); len as usize],
                    elem,
                });
                set!(regs[0], V::Ref(h));
            }
            0x24 | 0x25 => {
                // filled-new-array {args}, type@idx — `/range` carries
                // (count, first_reg), the 35c form lists registers.
                let desc = self
                    .dexes[di]
                    .type_descriptor(ins.idx.unwrap_or(0))
                    .map_err(E::Vm)?;
                let elem = desc.trim_start_matches('[').to_string();
                let arg_regs: Vec<usize> = if ins.opcode == 0x25 {
                    // {vCCCC..+AA}: `regs`=[range start], `lit`=reg count
                    let (first, n) = (regs[0] as usize, ins.lit.unwrap_or(0) as usize);
                    (first..first + n).collect()
                } else {
                    regs.iter().map(|&i| i as usize).collect()
                };
                let data: Vec<V> = arg_regs
                    .iter()
                    .map(|&i| Self::reg(fr, i))
                    .collect::<Rt<_>>()?;
                let h = self.heap.alloc(Obj::Array { elem, data });
                fr.result = Some(Some(V::Ref(h)));
            }
            0x26 => {
                // fill-array-data vAA, +off — payload at `target`
                let h = Self::refv(r(0)?)?;
                self.null_check(h, "fill-array-data")?;
                let toff = ins.target.ok_or_else(|| E::Vm(err("dexrt.payload")))?;
                self.fill_array(h, &fr.table, toff as usize)?;
            }
            0x27 => {
                let h = Self::refv(r(0)?)?;
                if h == 0 {
                    return Err(self.throw_builtin(
                        "Ljava/lang/NullPointerException;",
                        "throw null",
                    ));
                }
                let site = self.describe_method(fr.loc.midx);
                self.throw_sites.insert(h, site);
                return Err(E::Throw(h));
            }
            // ---- branches ----
            0x28..=0x2a => {
                return Ok(Out::Next(
                    ins.target.ok_or_else(|| E::Vm(err("dexrt.branch")))? as usize,
                ))
            }
            0x2b | 0x2c => {
                let v = Self::i32v(r(0)?)?;
                let toff = ins.target.ok_or_else(|| E::Vm(err("dexrt.branch")))? as usize;
                let tgt = self.switch_target(&fr.table, toff, v, op == 0x2b)?;
                // Payload targets are relative to the switch insn's offset.
                return Ok(Out::Next(
                    tgt.map(|t| (pc as i64 + t as i64) as usize)
                        .unwrap_or(next),
                ));
            }
            0x2d..=0x30 => {
                let (a, b): (f64, f64) = if op >= 0x2f {
                    (Self::f64v(r(1)?)?, Self::f64v(r(2)?)?)
                } else {
                    (Self::f32v(r(1)?)? as f64, Self::f32v(r(2)?)? as f64)
                };
                let less_bias = op == 0x2d || op == 0x2f; // cmpl → NaN is -1
                let c = if a.is_nan() || b.is_nan() {
                    if less_bias { -1 } else { 1 }
                } else if a < b {
                    -1
                } else if a > b {
                    1
                } else {
                    0
                };
                set!(regs[0], V::I32(c));
            }
            0x31 => {
                let (a, b) = (Self::i64v(r(1)?)?, Self::i64v(r(2)?)?);
                set!(regs[0], V::I32(a.cmp(&b) as i32));
            }
            0x32..=0x37 => {
                let (va, vb) = (r(0)?, r(1)?);
                let t = match op {
                    // if-eq/if-ne also compare references by identity.
                    0x32 | 0x33 => {
                        // `const/4 v, 0` puts a null reference in a register,
                        // so Ref may be compared against I32(0).
                        let refv = |v: V| match v {
                            V::Ref(h) => Some(h),
                            V::I32(0) => Some(0),
                            _ => None,
                        };
                        let eq = match (refv(va), refv(vb)) {
                            (Some(a), Some(b)) => a == b,
                            _ => Self::i32v(va)? == Self::i32v(vb)?,
                        };
                        if op == 0x32 {
                            eq
                        } else {
                            !eq
                        }
                    }
                    _ => {
                        let (a, b) = (Self::i32v(va)?, Self::i32v(vb)?);
                        match op {
                            0x34 => a < b,
                            0x35 => a >= b,
                            0x36 => a > b,
                            _ => a <= b,
                        }
                    }
                };
                if t {
                    return Ok(Out::Next(
                        ins.target.ok_or_else(|| E::Vm(err("dexrt.branch")))? as usize,
                    ));
                }
            }
            0x38..=0x3d => {
                let va = r(0)?;
                // if-eqz/if-nez also test references against null.
                let a = match va {
                    V::Ref(h) => h as i32,
                    _ => Self::i32v(va)?,
                };
                let t = match op {
                    0x38 => a == 0,
                    0x39 => a != 0,
                    0x3a => a < 0,
                    0x3b => a >= 0,
                    0x3c => a > 0,
                    _ => a <= 0,
                };
                if t {
                    return Ok(Out::Next(
                        ins.target.ok_or_else(|| E::Vm(err("dexrt.branch")))? as usize,
                    ));
                }
            }
            // ---- arrays ----
            0x44..=0x4a => {
                let (h, i) = (Self::refv(r(1)?)?, Self::i32v(r(2)?)?);
                let v = self.aget(h, i)?;
                set!(regs[0], v);
            }
            0x4b..=0x51 => {
                let (h, i, v) = (Self::refv(r(1)?)?, Self::i32v(r(2)?)?, r(0)?);
                self.aput(h, i, v)?;
            }
            // ---- instance fields ----
            0x52..=0x58 => {
                let h = Self::refv(r(1)?)?;
                if h == 0 {
                    let d = format!(
                        "iget {} in {}",
                        self.describe_field(gi(ins.idx.unwrap_or(0))),
                        self.describe_method(fr.loc.midx),
                    );
                    return Err(self.throw_builtin("Ljava/lang/NullPointerException;", &d));
                }
                let f = self.canon_field(gi(ins.idx.unwrap_or(0)));
                let stored = match self.heap.get(h) {
                    Some(Obj::Instance { fields, .. }) | Some(Obj::Host { fields, .. }) => {
                        fields.get(&f).copied()
                    }
                    _ => return vm("dexrt.type"),
                };
                if self.trace_field.as_deref().is_some_and(|t| {
                    self.describe_field(gi(ins.idx.unwrap_or(0))).contains(t)
                }) {
                    eprintln!(
                        "[ftrace] iget {} obj={} -> {:?} in {}",
                        self.describe_field(gi(ins.idx.unwrap_or(0))),
                        h,
                        stored,
                        self.describe_method(fr.loc.midx)
                    );
                }
                let v = match stored {
                    Some(v) => v,
                    // Host objects expose named fields through builtins
                    // (PackageInfo.versionCode, ApplicationInfo.dataDir);
                    // guest objects fall back to the field's type default.
                    None => self
                        .builtin_ifield(h, f)
                        .unwrap_or_else(|| self.field_default(f).unwrap_or(V::I32(0))),
                };
                set!(regs[0], v);
            }
            0x59..=0x5f => {
                let h = Self::refv(r(1)?)?;
                if h == 0 {
                    let d = format!(
                        "iput {} in {}",
                        self.describe_field(gi(ins.idx.unwrap_or(0))),
                        self.describe_method(fr.loc.midx),
                    );
                    return Err(self.throw_builtin("Ljava/lang/NullPointerException;", &d));
                }
                let f = self.canon_field(gi(ins.idx.unwrap_or(0)));
                if self.trace_field.as_deref().is_some_and(|t| {
                    self.describe_field(gi(ins.idx.unwrap_or(0))).contains(t)
                }) {
                    eprintln!(
                        "[ftrace] iput {} obj={} <- {:?} in {}",
                        self.describe_field(gi(ins.idx.unwrap_or(0))),
                        h,
                        r(0).ok(),
                        self.describe_method(fr.loc.midx)
                    );
                }
                match self.heap.get_mut(h) {
                    Some(Obj::Instance { fields, .. }) | Some(Obj::Host { fields, .. }) => {
                        fields.insert(f, r(0)?);
                    }
                    _ => return vm("dexrt.type"),
                }
            }
            // ---- static fields ----
            0x60..=0x66 => {
                let fl = ins.idx.unwrap_or(0);
                let f = self.canon_field(gi(fl));
                let owner = self.canon(gi(
                    self.dexes[di]
                        .field(fl)
                        .map_err(E::Vm)?
                        .class_idx as u32,
                ));
                self.ensure_init(owner)?;
                let stored = self
                    .classes
                    .entry(owner)
                    .or_default()
                    .statics
                    .get(&f)
                    .copied();
                let v = match stored {
                    Some(v) => v,
                    // A static on a host-provided class (Build.VERSION,
                    // Charset constants) comes from the builtin table —
                    // the type default would silently lie (SDK_INT == 0
                    // flips every `>= O` gate to the legacy path).
                    None if self.def_of(owner).is_none() => {
                        let v = self
                            .builtin_sfield(f)
                            .unwrap_or_else(|| self.field_default(f).unwrap_or(V::I32(0)));
                        // Cache reference constants — enum singletons and
                        // Charset/Locale objects rely on `==` identity.
                        if matches!(v, V::Ref(h) if h != 0) {
                            self.classes
                                .entry(owner)
                                .or_default()
                                .statics
                                .insert(f, v);
                        }
                        v
                    }
                    None => {
                        // A static on a guest class carries its
                        // initializer in the dex `static_values` array —
                        // that's where every `R.*` constant lives.
                        let v = self
                            .encoded_static(owner, f)
                            .unwrap_or_else(|| {
                                self.field_default(f).unwrap_or(V::I32(0))
                            });
                        if matches!(v, V::Ref(h) if h != 0) {
                            self.classes
                                .entry(owner)
                                .or_default()
                                .statics
                                .insert(f, v);
                        }
                        v
                    }
                };
                set!(regs[0], v);
            }
            0x67..=0x6d => {
                let fl = ins.idx.unwrap_or(0);
                let f = self.canon_field(gi(fl));
                let owner = self.canon(gi(
                    self.dexes[di]
                        .field(fl)
                        .map_err(E::Vm)?
                        .class_idx as u32,
                ));
                self.ensure_init(owner)?;
                self.classes
                    .entry(owner)
                    .or_default()
                    .statics
                    .insert(f, r(0)?);
            }
            // ---- invokes ----
            0x6e..=0x72 | 0x74..=0x78 => {
                let kind = match op {
                    0x6e | 0x74 => InvokeKind::Virtual,
                    0x6f | 0x75 => InvokeKind::Super {
                        caller_class: fr.loc.class_idx,
                    },
                    0x70 | 0x76 => InvokeKind::Direct,
                    0x71 | 0x77 => InvokeKind::Static,
                    _ => InvokeKind::Interface,
                };
                let midx = gi(ins.idx.unwrap_or(0));
                let arg_regs: Vec<u16> = if ins.format == ovelo_dex::Format::F3rc {
                    // {vCCCC..+AA}: `regs`=[range start], `lit`=reg count
                    let (first, n) = (regs[0] as usize, ins.lit.unwrap_or(0) as usize);
                    (first..first + n).map(|i| i as u16).collect()
                } else {
                    regs.clone()
                };
                let args: Vec<V> = arg_regs
                    .iter()
                    .map(|&i| Self::reg(fr, i as usize))
                    .collect::<Rt<_>>()?;
                let this_arg = args.first().copied();
                let res = self.invoke(midx, args, kind).map_err(|e| match e {
                    E::Vm(v) => {
                        let d = format!("{v} in {}", self.describe_method(fr.loc.midx));
                        E::Vm(v.with_detail(d))
                    }
                    E::Throw(h) => {
                        let caller = self.describe_method(fr.loc.midx);
                        if let Some(Obj::Throwable { detail, .. }) = self.heap.get_mut(h) {
                            *detail = format!("{} in {}", detail, caller);
                        }
                        E::Throw(h)
                    }
                })?;
                fr.last_call = self.describe_method(midx);
                if let Some(p) = self.trace_call.as_deref() {
                    if fr.last_call.contains(p) {
                        eprintln!(
                            "[call] {} +{pc:04x} -> {} this={:?} ({:?})",
                            self.describe_method(fr.loc.midx),
                            fr.last_call,
                            this_arg,
                            res
                        );
                    }
                }
                fr.result = Some(res);
            }
            // ---- unary ops ----
            0x7b..=0x80 => {
                let v = r(1)?;
                let out = match op {
                    0x7b => V::I32(Self::i32v(v)?.wrapping_neg()),
                    0x7c => V::I32(!Self::i32v(v)?),
                    0x7d => V::I64(Self::i64v(v)?.wrapping_neg()),
                    0x7e => V::I64(!Self::i64v(v)?),
                    0x7f => V::F32(-Self::f32v(v)?),
                    _ => V::F64(-Self::f64v(v)?),
                };
                set!(regs[0], out);
            }
            0x81..=0x8f => {
                let v = r(1)?;
                let out = match op {
                    0x81 => V::I64(Self::i32v(v)? as i64),
                    0x82 => V::F32(Self::i32v(v)? as f32),
                    0x83 => V::F64(Self::i32v(v)? as f64),
                    0x84 => V::I32(Self::i64v(v)? as i32),
                    0x85 => V::F32(Self::i64v(v)? as f32),
                    0x86 => V::F64(Self::i64v(v)? as f64),
                    0x87 => V::I32(Self::f32v(v)? as i32),
                    0x88 => V::I64(Self::f32v(v)? as i64),
                    0x89 => V::F64(Self::f32v(v)? as f64),
                    0x8a => V::I32(Self::f64v(v)? as i32),
                    0x8b => V::I64(Self::f64v(v)? as i64),
                    0x8c => V::F32(Self::f64v(v)? as f32),
                    0x8d => V::I32(Self::i32v(v)? as i8 as i32),
                    0x8e => V::I32(Self::i32v(v)? as u16 as i32),
                    _ => V::I32(Self::i32v(v)? as i16 as i32),
                };
                set!(regs[0], out);
            }
            // ---- binary ops (3 args: a = b op c) ----
            0x90..=0xaf => {
                let (b, c) = (r(1)?, r(2)?);
                let out = self.binop(op, b, c)?;
                set!(regs[0], out);
            }
            // ---- binary ops (2addr: a op= b) ----
            0xb0..=0xcf => {
                let (a, b) = (r(0)?, r(1)?);
                let out = self.binop(op - 0x20, a, b)?; // 2addr shares 0x90 base
                set!(regs[0], out);
            }
            // ---- int literal ops ----
            0xd0..=0xd7 => {
                let (b, lit) = (r(1)?, ins.lit.unwrap_or(0) as i32);
                let out = self.binop_lit(op, Self::i32v(b)?, lit)?;
                set!(regs[0], V::I32(out));
            }
            0xd8..=0xe2 => {
                let (b, lit) = (r(1)?, ins.lit.unwrap_or(0) as i32);
                let out = self.binop_lit(op, Self::i32v(b)?, lit)?;
                set!(regs[0], V::I32(out));
            }
            _ => return vm("dexrt.op.unsupported"),
        }
        Ok(Out::Next(next))
    }

    fn field_default(&self, field_idx: GIdx) -> Option<V> {
        let d = self.dex_of(field_idx)?;
        let f = d.field(gl(field_idx)).ok()?;
        let desc = d.type_descriptor(f.type_idx as u32).ok()?;
        Some(V::default_for(&desc))
    }

    /// A static field's `static_values` initial value (the constants
    /// `R.*` classes carry) mapped to a V, or `None` when the field
    /// has no encoded value or one we can't materialize.
    fn encoded_static(&mut self, owner: GIdx, field: GIdx) -> Option<V> {
        let (cg, def) = self.def_loc_of(owner)?;
        // The encoded array lives in the dex that DEFINES the class,
        // indexed by that dex's local field ids. When the canonical
        // index belongs to a referencing dex (multi-dex apps), match
        // the static field by name instead of the raw index.
        let local_idx = if gd(field) == gd(cg) {
            gl(field)
        } else {
            let (name, ty) = self
                .dex_of(field)
                .and_then(|d| {
                    let f = d.field(gl(field)).ok()?;
                    Some((d.string(f.name_idx).ok()?, d.type_descriptor(f.type_idx as u32).ok()?))
                })?;
            let cd = self.dex_of(cg)?.class_data(def).ok()?;
            let mut found = None;
            for f in &cd.static_fields {
                let Ok(fr) = self.dex_of(cg)?.field(f.field_idx) else {
                    continue;
                };
                let Ok(n) = self.dex_of(cg)?.string(fr.name_idx) else {
                    continue;
                };
                let Ok(t) = self.dex_of(cg)?.type_descriptor(fr.type_idx as u32) else {
                    continue;
                };
                if n == name && t == ty {
                    found = Some(f.field_idx);
                    break;
                }
            }
            found?
        };
        let v = self
            .dex_of(cg)?
            .static_value(def, local_idx)
            .ok()
            .flatten()?;
        use ovelo_dex::EncodedValue as EV;
        match v {
            EV::Byte(x) => Some(V::I32(x as i32)),
            EV::Short(x) => Some(V::I32(x as i32)),
            EV::Char(x) => Some(V::I32(x as i32)),
            EV::Int(x) => Some(V::I32(x)),
            EV::Bool(b) => Some(V::I32(b as i32)),
            EV::Long(x) => Some(V::I64(x)),
            EV::Float(x) => Some(V::F32(x)),
            EV::Double(x) => Some(V::F64(x)),
            EV::String(i) => {
                let s = self.dex_of(cg)?.string(i).ok()?;
                Some(jdk::jstr(self, &s))
            }
            EV::Type(i) => Some(V::Ref(self.class_mirror(gx(gd(cg), i)))),
            EV::Array(items) => {
                let vals: Vec<V> = items
                    .iter()
                    .map(|it| match it {
                        EV::Int(x) => V::I32(*x),
                        EV::Long(x) => V::I64(*x),
                        EV::Bool(b) => V::I32(*b as i32),
                        _ => V::Ref(0),
                    })
                    .collect();
                let h = self.heap.alloc(Obj::Array {
                    elem: "Ljava/lang/Object;".to_string(),
                    data: vals,
                });
                Some(V::Ref(h))
            }
            _ => None,
        }
    }

    fn aget(&mut self, h: u32, i: i32) -> Rt<V> {
        self.null_check(h, "aget")?;
        match self.heap.get(h) {
            Some(Obj::Array { data, .. }) => {
                let i = i as usize;
                if i >= data.len() {
                    return Err(self.throw_builtin(
                        "Ljava/lang/ArrayIndexOutOfBoundsException;",
                        "aget",
                    ));
                }
                Ok(data[i])
            }
            _ => vm("dexrt.type"),
        }
    }

    fn aput(&mut self, h: u32, i: i32, v: V) -> Rt<()> {
        self.null_check(h, "aput")?;
        match self.heap.get_mut(h) {
            Some(Obj::Array { data, .. }) => {
                if i < 0 || i as usize >= data.len() {
                    let d = format!("aput idx {i} len {}", data.len());
                    return Err(self.throw_builtin(
                        "Ljava/lang/ArrayIndexOutOfBoundsException;",
                        &d,
                    ));
                }
                data[i as usize] = v;
                Ok(())
            }
            _ => vm("dexrt.type"),
        }
    }

    /// fill-array-data payload: `raw` layout is ident, elem_width, size(u32),
    /// data… — copy element-wise into the array (widths 1,2,4,8 bytes).
    fn fill_array(&mut self, h: u32, table: &[Insn], off: usize) -> Rt<()> {
        let p = table
            .iter()
            .find(|i| i.offset as usize == off)
            .ok_or_else(|| E::Vm(err("dexrt.payload")))?;
        let raw = &p.raw;
        if raw.len() < 4 || raw[0] != 0x0300 {
            return vm("dexrt.payload");
        }
        let width = raw[1] as usize;
        let count = (raw[3] as usize) << 16 | raw[2] as usize;
        let mut vals = Vec::with_capacity(count);
        for k in 0..count {
            let bo = 8 + k * width;
            // raw is u16 units; byte offset within it
            let u = bo / 2;
            let sh = (bo % 2) * 8;
            let mut val: u64 = 0;
            for b in 0..width {
                let idx = u + b / 2;
                let shift = ((b % 2) * 8 + sh) % 16;
                let byte = ((raw.get(idx).copied().unwrap_or(0) >> shift) & 0xff) as u64;
                val |= byte << (b * 8);
            }
            vals.push(match width {
                8 => V::I64(val as i64),
                _ => V::I32(val as i32),
            });
        }
        match self.heap.get_mut(h) {
            Some(Obj::Array { data, .. }) if data.len() == vals.len() => {
                *data = vals;
                Ok(())
            }
            Some(Obj::Array { .. }) => {
                Err(self.throw_builtin("Ljava/lang/ArrayIndexOutOfBoundsException;", "fill-array-data"))
            }
            _ => vm("dexrt.type"),
        }
    }

    /// Switch payload → case target (code units) or None for default.
    fn switch_target(&self, table: &[Insn], off: usize, v: i32, packed: bool) -> Rt<Option<u32>> {
        let p = table
            .iter()
            .find(|i| i.offset as usize == off)
            .ok_or_else(|| E::Vm(err("dexrt.payload")))?;
        let raw = &p.raw;
        if packed {
            if raw.len() < 4 || raw[0] != 0x0100 {
                return vm("dexrt.payload");
            }
            let size = raw[1] as usize;
            let first = ((raw[3] as i32) << 16) | raw[2] as i32;
            let k = v.wrapping_sub(first);
            if k < 0 || k as usize >= size {
                return Ok(None);
            }
            let i = 4 + k as usize * 2;
            return Ok(Some(
                (((raw[i + 1] as i32) << 16) | raw[i] as i32) as u32,
            ));
        }
        if raw.len() < 2 || raw[0] != 0x0200 {
            return vm("dexrt.payload");
        }
        let size = raw[1] as usize;
        for k in 0..size {
            let ki = 2 + k * 2;
            let key = ((raw[ki + 1] as i32) << 16) | raw[ki] as i32;
            if key == v {
                let ti = 2 + size * 2 + k * 2;
                return Ok(Some(
                    (((raw[ti + 1] as i32) << 16) | raw[ti] as i32) as u32,
                ));
            }
            if key > v {
                break;
            }
        }
        Ok(None)
    }

    /// int binary op with a literal operand (lit16/lit8 families).
    fn binop_lit(&mut self, op: u8, b: i32, lit: i32) -> Rt<i32> {
        Ok(match op {
            0xd0 | 0xd8 => b.wrapping_add(lit),
            0xd1 | 0xd9 => lit.wrapping_sub(b),
            0xd2 | 0xda => b.wrapping_mul(lit),
            0xd3 | 0xdb => {
                if lit == 0 {
                    return Err(self.throw_builtin(
                        "Ljava/lang/ArithmeticException;",
                        "divide by zero",
                    ));
                }
                b.wrapping_div(lit)
            }
            0xd4 | 0xdc => {
                if lit == 0 {
                    return Err(self.throw_builtin(
                        "Ljava/lang/ArithmeticException;",
                        "rem by zero",
                    ));
                }
                b.wrapping_rem(lit)
            }
            0xd5 | 0xdd => b & lit,
            0xd6 | 0xde => b | lit,
            0xd7 | 0xdf => b ^ lit,
            0xe0 => b.wrapping_shl(lit as u32),
            0xe1 => b.wrapping_shr(lit as u32),
            0xe2 => (b as u32).wrapping_shr(lit as u32) as i32,
            _ => return vm("dexrt.op.unsupported"),
        })
    }

    /// Binary ops 0x90..0xaf (b op c → V). Shift counts mask to 5/6 bits.
    fn binop(&mut self, op: u8, b: V, c: V) -> Rt<V> {
        Ok(match op {
            // int
            0x90 => V::I32(Self::i32v(b)?.wrapping_add(Self::i32v(c)?)),
            0x91 => V::I32(Self::i32v(b)?.wrapping_sub(Self::i32v(c)?)),
            0x92 => V::I32(Self::i32v(b)?.wrapping_mul(Self::i32v(c)?)),
            0x93 => {
                let (x, y) = (Self::i32v(b)?, Self::i32v(c)?);
                if y == 0 {
                    return Err(self.throw_builtin(
                        "Ljava/lang/ArithmeticException;",
                        "divide by zero",
                    ));
                }
                V::I32(x.wrapping_div(y))
            }
            0x94 => {
                let (x, y) = (Self::i32v(b)?, Self::i32v(c)?);
                if y == 0 {
                    return Err(self.throw_builtin(
                        "Ljava/lang/ArithmeticException;",
                        "rem by zero",
                    ));
                }
                V::I32(x.wrapping_rem(y))
            }
            0x95 => V::I32(Self::i32v(b)? & Self::i32v(c)?),
            0x96 => V::I32(Self::i32v(b)? | Self::i32v(c)?),
            0x97 => V::I32(Self::i32v(b)? ^ Self::i32v(c)?),
            0x98 => V::I32(Self::i32v(b)?.wrapping_shl(Self::i32v(c)? as u32 & 0x1f)),
            0x99 => V::I32(Self::i32v(b)?.wrapping_shr(Self::i32v(c)? as u32 & 0x1f)),
            0x9a => V::I32(
                (Self::i32v(b)? as u32).wrapping_shr(Self::i32v(c)? as u32 & 0x1f) as i32,
            ),
            // long
            0x9b => V::I64(Self::i64v(b)?.wrapping_add(Self::i64v(c)?)),
            0x9c => V::I64(Self::i64v(b)?.wrapping_sub(Self::i64v(c)?)),
            0x9d => V::I64(Self::i64v(b)?.wrapping_mul(Self::i64v(c)?)),
            0x9e => {
                let (x, y) = (Self::i64v(b)?, Self::i64v(c)?);
                if y == 0 {
                    return Err(self.throw_builtin(
                        "Ljava/lang/ArithmeticException;",
                        "divide by zero",
                    ));
                }
                V::I64(x.wrapping_div(y))
            }
            0x9f => {
                let (x, y) = (Self::i64v(b)?, Self::i64v(c)?);
                if y == 0 {
                    return Err(self.throw_builtin(
                        "Ljava/lang/ArithmeticException;",
                        "rem by zero",
                    ));
                }
                V::I64(x.wrapping_rem(y))
            }
            0xa0 => V::I64(Self::i64v(b)? & Self::i64v(c)?),
            0xa1 => V::I64(Self::i64v(b)? | Self::i64v(c)?),
            0xa2 => V::I64(Self::i64v(b)? ^ Self::i64v(c)?),
            0xa3 => V::I64(Self::i64v(b)?.wrapping_shl(Self::i32v(c)? as u32 & 0x3f)),
            0xa4 => V::I64(Self::i64v(b)?.wrapping_shr(Self::i32v(c)? as u32 & 0x3f)),
            0xa5 => V::I64(
                (Self::i64v(b)? as u64).wrapping_shr(Self::i32v(c)? as u32 & 0x3f) as i64,
            ),
            // float / double
            0xa6 => V::F32(Self::f32v(b)? + Self::f32v(c)?),
            0xa7 => V::F32(Self::f32v(b)? - Self::f32v(c)?),
            0xa8 => V::F32(Self::f32v(b)? * Self::f32v(c)?),
            0xa9 => V::F32(Self::f32v(b)? / Self::f32v(c)?),
            0xaa => V::F32(Self::f32v(b)? % Self::f32v(c)?),
            0xab => V::F64(Self::f64v(b)? + Self::f64v(c)?),
            0xac => V::F64(Self::f64v(b)? - Self::f64v(c)?),
            0xad => V::F64(Self::f64v(b)? * Self::f64v(c)?),
            0xae => V::F64(Self::f64v(b)? / Self::f64v(c)?),
            0xaf => V::F64(Self::f64v(b)? % Self::f64v(c)?),
            _ => return vm("dexrt.op.unsupported"),
        })
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    use ovelo_dex::testdex::*;


    fn vm(bytes: Vec<u8>) -> Vm {
        Vm::new(DexFile::open(bytes).unwrap()).unwrap()
    }

    fn virt(name: &'static str, insns: Vec<u16>) -> M {
        M {
            name,
            ret: "I",
            params: &[],
            is_virtual: true,
            regs: 4,
            ins: 0,
            outs: 2,
            insns,
            tries: vec![],
            lits: &[],
        }
    }

    fn dir(name: &'static str, ins: u16, insns: Vec<u16>) -> M {
        M {
            name,
            ret: "I",
            params: &[],
            is_virtual: false,
            regs: 6,
            ins,
            outs: 2,
            insns,
            tries: vec![],
            lits: &[],
        }
    }

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

    // Instruction encodings (see DEX spec):
    //   const/4 vA,#lit4 : 0x12 | A<<8 | lit<<12
    //   return vA        : 0x0f | A<<8
    //   move-result vA   : 0x0a | A<<8
    //   move-exception vA: 0x0d | A<<8
    //   add-int vA,vB,vC : [0x90|A<<8, C|B<<8]
    //   div-int vA,vB,vC : [0x93|A<<8, C|B<<8]
    //   new-array vA,vB,T: [0x23|A<<8|B<<8, T]
    //   new-instance vA,T: [0x22|A<<8, T]
    //   aget vA,vB,vC    : [0x44|A<<8, C|B<<8]
    //   iget vA,vB,f     : [0x52|A<<8|B<<8, f]
    //   iput vA,vB,f     : [0x59|A<<8|B<<8, f]
    //   sget vA,f        : [0x60|A<<8, f]
    //   sput vA,f        : [0x69|A<<8, f]
    //   const-string vA,s: [0x1a|A<<8, s]
    //   invoke-virtual {regs},m : [0x6e|count<<12|first<<8, m, rest]

    #[test]
    fn add_two_args() {
        // regs=2, ins=2 -> args at v0,v1. add-int v0,v0,v1; return v0.
        let c = cls(
            "LCalc;",
            vec![M {
                name: "add",
                ret: "I",
                params: &["I", "I"],
                is_virtual: false,
                regs: 2,
                ins: 2,
                outs: 0,
                insns: vec![0x0090, 0x0100, 0x000f],
                tries: vec![],
                lits: &[],
            }],
        );
        let (b, _ix) = build(&[c]);
        let mut v = vm(b);
        assert_eq!(
            v.run("LCalc;", "add", &[V::I32(7), V::I32(5)]).unwrap(),
            Some(V::I32(12))
        );
    }

    #[test]
    fn div_by_zero_caught_by_handler() {
        // try units 1..3: div-int v0,v0,v0 -> handler @4 returns -1.
        let mut m = dir("f", 0, vec![
            0x0012, // const/4 v0,0
            0x0093, 0x0000, // div-int v0,v0,v0
            0x000f, // return v0
            0x020d, // move-exception v2
            0xf112, // const/4 v1,-1
            0x010f, // return v1
        ]);
        m.tries = vec![(1, 2, vec![], Some(4))];
        let (b, _ix) = build(&[cls("LT;", vec![m])]);
        let mut v = vm(b);
        assert_eq!(v.run("LT;", "f", &[]).unwrap(), Some(V::I32(-1)));
    }

    #[test]
    fn aget_out_of_bounds_uncaught() {
        let c = C {
            class: "LT;",
            // forces "I" into the type table
            sfields: &[("I", "unused")],
            methods: vec![dir("f", 0, vec![
                0x1112,             // const/4 v1,1
                0x1023, 0x0000,     // new-array v0,v1,type@I (patched)
                0x2212,             // const/4 v2,2
                0x0044, 0x0200,     // aget v0,v0,v2
                0x000f,
            ])],
            ..cls("LT;", vec![])
        };
        let (mut b, ix) = build(&[c]);
        let ti = ix.t("I");
        for i in (0..b.len() - 4).step_by(2) {
            if u16::from_le_bytes([b[i], b[i + 1]]) == 0x1023 {
                b[i + 2..i + 4].copy_from_slice(&(ti as u16).to_le_bytes());
            }
        }
        let mut v = vm(b);
        let e = v.run("LT;", "f", &[]).unwrap_err();
        assert_eq!(e.cause(), "dexrt.exception.uncaught");
    }

    #[test]
    fn virtual_dispatch_picks_override() {
        let base = C {
            class: "LB;",
            methods: vec![virt("who", vec![0x1112, 0x010f])],
            ..cls("LB;", vec![])
        };
        let child = C {
            class: "LC;",
            sup: Some("LB;"),
            methods: vec![virt("who", vec![0x2112, 0x010f])],
            ..cls("LC;", vec![])
        };
        let entry = cls(
            "LT;",
            vec![dir("main", 0, vec![
                0x0022, 0x0000,           // new-instance v0,LC (patched)
                0x106e, 0x0000, 0x0000,   // invoke-virtual {v0},LB.who (patched)
                0x010a,                   // move-result v1
                0x010f,                   // return v1
            ])],
        );
        let (mut b, ix) = build(&[base, child, entry]);
        let tc = ix.t("LC;");
        let mw = ix.m("LB;", "who");
        for i in (0..b.len() - 4).step_by(2) {
            let w = u16::from_le_bytes([b[i], b[i + 1]]);
            if w == 0x0022 {
                b[i + 2..i + 4].copy_from_slice(&(tc as u16).to_le_bytes());
            }
            if w == 0x106e {
                b[i + 2..i + 4].copy_from_slice(&(mw as u16).to_le_bytes());
            }
        }
        let mut v = vm(b);
        assert_eq!(v.run("LT;", "main", &[]).unwrap(), Some(V::I32(2)));
    }

    #[test]
    fn clinit_runs_once() {
        // <clinit>: LT.x = 1 ; main: LT.x + LT.x -> 2 (clinit ran once)
        let c = C {
            class: "LT;",
            sfields: &[("I", "x")],
            methods: vec![
                M {
                    name: "<clinit>",
                    ret: "V",
                    params: &[],
                    is_virtual: false,
                    regs: 2,
                    ins: 0,
                    outs: 0,
                    insns: vec![
                        0x1112,         // const/4 v1,1
                        0x0169, 0x0000, // sput v1,LT.x (patched)
                        0x000e,         // return-void
                    ],
                    tries: vec![],
                    lits: &[],
                },
                dir("main", 0, vec![
                    0x0060, 0x0000, // sget v0,LT.x
                    0x0160, 0x0000, // sget v1,LT.x
                    0x0090, 0x0100, // add-int v0,v0,v1
                    0x000f,
                ]),
            ],
            ..cls("LT;", vec![])
        };
        let (mut b, ix) = build(&[c]);
        let fx = ix.f("LT;", "x");
        for i in (0..b.len() - 4).step_by(2) {
            let w = u16::from_le_bytes([b[i], b[i + 1]]);
            if w == 0x0169 || w == 0x0060 || w == 0x0160 {
                b[i + 2..i + 4].copy_from_slice(&(fx as u16).to_le_bytes());
            }
        }
        let mut v = vm(b);
        assert_eq!(v.run("LT;", "main", &[]).unwrap(), Some(V::I32(2)));
    }



    #[test]
    fn const_string_and_builtin_length() {
        // Ljava/lang/String;.length declared so the invoke idx exists; the
        // builtin interpreter path handles it without running its insns.
        let strcls = C {
            ext: true,
            class: "Ljava/lang/String;",
            methods: vec![virt("length", vec![0x000e])],
            ..cls("Ljava/lang/String;", vec![])
        };
        let entry = cls(
            "LT;",
            vec![M {
                name: "len",
                ret: "I",
                params: &[],
                is_virtual: false,
                regs: 3,
                ins: 0,
                outs: 2,
                insns: vec![
                    0x001a, 0x0000, // const-string v0,"hello" (patched)
                    0x106e, 0x0000, 0x0000, // invoke-virtual {v0},String.length
                    0x010a, // move-result v1
                    0x010f, // return v1
                ],
                tries: vec![],
                lits: &["hello"],
            }],
        );
        let (mut b, ix) = build(&[strcls, entry]);
        let si = ix.s_idx("hello");
        let ml = ix.m("Ljava/lang/String;", "length");
        for i in (0..b.len() - 4).step_by(2) {
            let w = u16::from_le_bytes([b[i], b[i + 1]]);
            if w == 0x001a {
                b[i + 2..i + 4].copy_from_slice(&(si as u16).to_le_bytes());
            }
            if w == 0x106e {
                b[i + 2..i + 4].copy_from_slice(&(ml as u16).to_le_bytes());
            }
        }
        let mut v = vm(b);
        assert_eq!(v.run("LT;", "len", &[]).unwrap(), Some(V::I32(5)));
    }

    #[test]
    fn assignable_walks_missing_intermediate() {
        // LCustomBtn subclasses Button, but the DEX never names the
        // intermediate TextView — the framework chain must still reach
        // View, so Button→View stays assignable.
        let ext = |class: &'static str| C {
            ext: true,
            class,
            sup: None,
            ifaces: &[],
            sfields: &[],
            ifields: &[],
            methods: vec![],
        };
        let (b, ix) = build(&[
            ext("Landroid/view/View;"),
            ext("Landroid/widget/Button;"),
            C {
                sup: Some("Landroid/widget/Button;"),
                ..cls("LCustomBtn;", vec![])
            },
        ]);
        let v = vm(b);
        assert!(v.assignable(
            ix.t("LCustomBtn;") as u64,
            ix.t("Landroid/view/View;") as u64
        ));
        // …and an unrelated class is still rejected.
        assert!(!v.assignable(
            ix.t("Landroid/widget/Button;") as u64,
            ix.t("LCustomBtn;") as u64
        ));
    }

    #[test]
    fn packed_switch_jumps_to_case() {
        // v0=1; packed-switch v0 -> case key 1 returns 20.
        // Layout: [0] const/4 v0,1 ; [1..3] packed-switch +8 ;
        // [4] case0: const/4 v1,10 ; [5] return v1 ;
        // [6..8] case1: const/16 v1,20 ; return v1 ;
        // [9..] payload: ident, size=2, first_key=0, targets rel. to switch
        //       (case0 abs 4 -> +3, case1 abs 6 -> +5)
        let c = cls(
            "LT;",
            vec![dir("f", 0, vec![
                0x1012,                 // const/4 v0,1
                0x002b, 0x0008, 0x0000, // packed-switch v0,+8 -> @9
                0xa112,                 // const/4 v1,10
                0x010f,                 // return v1
                0x0113, 0x0014,         // const/16 v1,20
                0x010f,                 // return v1
                0x0100, 0x0002,         // payload: packed, size 2
                0x0000, 0x0000,         // first_key = 0
                0x0003, 0x0000,         // target[0] = +3 -> @4
                0x0005, 0x0000,         // target[1] = +5 -> @6
            ])],
        );
        let (b, _ix) = build(&[c]);
        let mut v = vm(b);
        assert_eq!(v.run("LT;", "f", &[]).unwrap(), Some(V::I32(20)));
    }

    // AOT: values and exception behavior must match the interpreter
    // (design: run Android ref → interpreter → AOT, results identical).

    #[test]
    fn aot_branch_and_loop_match_interp() {
        // select(a,b) = a>=b ? a : b — one conditional branch.
        //   [0..1] if-ge v0,v1,+3 ; [2] move v0,v1 ; [3] return v0
        let select = M {
            name: "select",
            ret: "I",
            params: &["I", "I"],
            is_virtual: false,
            regs: 2,
            ins: 2,
            outs: 0,
            insns: vec![
                0x1035, 0x0003, // if-ge v0,v1,+3
                0x1001, // move v0,v1
                0x000f, // return v0
            ],
            tries: vec![],
            lits: &[],
        };
        // sum(n) = 0+1+..+n-1 — backward-branch loop.
        //   [0] sum=0 [1] i=0 [2..3] if-ge i,n,+6
        //   [4] sum+=i [5..6] i+=1 [7] goto -5 [8] return sum
        let sum = M {
            name: "sum",
            ret: "I",
            params: &["I"],
            is_virtual: false,
            regs: 4,
            ins: 1,
            outs: 0,
            insns: vec![
                0x0012, // const/4 v0,0
                0x0112, // const/4 v1,0
                0x3135, 0x0006, // if-ge v1,v3,+6
                0x10b0, // add-int/2addr v0,v1
                0x01d8, 0x0101, // add-int/lit8 v1,v1,+1
                0xfb28, // goto -5
                0x000f, // return v0
            ],
            tries: vec![],
            lits: &[],
        };
        let (b, _ix) = build(&[cls("LT;", vec![select, sum])]);
        let mut v = vm(b);
        let interp_sel = v.run("LT;", "select", &[V::I32(3), V::I32(9)]).unwrap();
        let interp_sum = v.run("LT;", "sum", &[V::I32(5)]).unwrap();
        assert_eq!(interp_sel, Some(V::I32(9)));
        assert_eq!(interp_sum, Some(V::I32(10)));

        assert!(v.enable_aot() >= 2);
        assert_eq!(
            v.run("LT;", "select", &[V::I32(3), V::I32(9)]).unwrap(),
            interp_sel
        );
        assert_eq!(
            v.run("LT;", "select", &[V::I32(9), V::I32(3)]).unwrap(),
            Some(V::I32(9))
        );
        assert_eq!(v.run("LT;", "sum", &[V::I32(5)]).unwrap(), interp_sum);
        assert_eq!(v.run("LT;", "sum", &[V::I32(0)]).unwrap(), Some(V::I32(0)));
        assert_eq!(
            v.run("LT;", "sum", &[V::I32(100)]).unwrap(),
            Some(V::I32(4950))
        );
    }

    #[test]
    fn aot_div_by_zero_throws_like_interp() {
        // f() = 5/0 — uncaught div by zero in both engines.
        let m = M {
            name: "f",
            ret: "I",
            params: &[],
            is_virtual: false,
            regs: 2,
            ins: 0,
            outs: 0,
            insns: vec![
                0x5012, // const/4 v0,5
                0x0112, // const/4 v1,0
                0x0093, 0x0100, // div-int v0,v0,v1
                0x000f, // return v0
            ],
            tries: vec![],
            lits: &[],
        };
        let (b, _ix) = build(&[cls("LT;", vec![m])]);
        let mut v = vm(b);
        let interp_err = v.run("LT;", "f", &[]).unwrap_err().to_string();
        assert!(interp_err.contains("ArithmeticException"));

        assert_eq!(v.enable_aot(), 1);
        let aot_err = v.run("LT;", "f", &[]).unwrap_err().to_string();
        assert_eq!(aot_err, interp_err);
    }

    #[test]
    fn aot_int_min_div_neg_one_wraps() {
        // i32::MIN / -1 and i32::MIN % -1 must wrap like the interpreter
        // (MIN and 0), not trap the host process.
        let div = M {
            name: "d",
            ret: "I",
            params: &["I", "I"],
            is_virtual: false,
            regs: 2,
            ins: 2,
            outs: 0,
            insns: vec![0x0093, 0x0100, 0x000f], // div-int v0,v0,v1
            tries: vec![],
            lits: &[],
        };
        let rem = M {
            name: "r",
            ret: "I",
            params: &["I", "I"],
            is_virtual: false,
            regs: 2,
            ins: 2,
            outs: 0,
            insns: vec![0x0094, 0x0100, 0x000f], // rem-int v0,v0,v1
            tries: vec![],
            lits: &[],
        };
        let (b, _ix) = build(&[cls("LT;", vec![div, rem])]);
        let mut v = vm(b);
        let args = &[V::I32(i32::MIN), V::I32(-1)];
        let interp_d = v.run("LT;", "d", args).unwrap();
        let interp_r = v.run("LT;", "r", args).unwrap();
        assert_eq!(v.enable_aot(), 2);
        assert_eq!(v.run("LT;", "d", args).unwrap(), interp_d);
        assert_eq!(v.run("LT;", "r", args).unwrap(), interp_r);
        // rem-by-zero keeps the interpreter's distinct message.
        let interp_e = v
            .run("LT;", "r", &[V::I32(1), V::I32(0)])
            .unwrap_err()
            .to_string();
        assert!(interp_e.contains("rem by zero"));
    }

    #[test]
    fn aot_rejected_methods_stay_interpreted() {
        // try/catch (non-empty tries) is outside the compile whitelist:
        // enable_aot must leave it to the interpreter, which still runs
        // the handler.
        let mut m = dir("f", 0, vec![
            0x0012, // const/4 v0,0
            0x0093, 0x0000, // div-int v0,v0,v0
            0x000f, // return v0
            0x020d, // move-exception v2
            0xf112, // const/4 v1,-1
            0x010f, // return v1
        ]);
        m.tries = vec![(1, 2, vec![], Some(4))];
        let (b, _ix) = build(&[cls("LT;", vec![m])]);
        let mut v = vm(b);
        assert_eq!(v.enable_aot(), 0);
        assert_eq!(v.run("LT;", "f", &[]).unwrap(), Some(V::I32(-1)));
    }
}

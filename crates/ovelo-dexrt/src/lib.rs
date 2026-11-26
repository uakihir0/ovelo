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
mod jni;
mod native;
pub mod res;
mod res_attrs;
mod value;

pub use aot::Aot;
pub use jdk::view::{
    fire_global_layout, fire_pre_draw, mark_layout_requested, DexViewInfo, P_BGCOLOR, P_TCOLOR,
    P_TEXT, P_TSIZE,
};
pub use jdk::{pseudo_get, pseudo_set, P_ACTION, P_APPINFO, P_COMPONENT, P_DATA, P_META, P_TARGET};
pub use value::{CanvasOp, PaintState, PathCmd};
pub use value::{Heap, HostState, Obj, V};

use aot::{AotFn, TRAP_DIV_ZERO, TRAP_REM_ZERO};

use hashbrown::HashMap;
use ovelo_dex::{decode_all, ClassData, ClassDef, DexFile, EncodedMethod, Insn, TryItem};
use ovelo_error::{ErrorCategory, OveloError, Phase, Result};

fn err(cause: &str) -> OveloError {
    OveloError::new(ErrorCategory::DexUnsupported, Phase::DexExec, cause)
}

/// Cached `std::env::var_os(name).is_some()` — debug flags are fixed at
/// process start, but the interpreter consulted getenv once per
/// instruction (`aget`/`sget`/`iget` sites) and per dispatch.
pub(crate) fn dbg_flag(name: &'static str) -> bool {
    thread_local! {
        static CACHE: std::cell::RefCell<HashMap<&'static str, bool>> =
            std::cell::RefCell::new(HashMap::new());
    }
    CACHE.with(|c| {
        if let Some(v) = c.borrow().get(name) {
            return *v;
        }
        let v = std::env::var_os(name).is_some();
        c.borrow_mut().insert(name, v);
        v
    })
}
fn errd(cause: &str, detail: String) -> OveloError {
    OveloError::new(ErrorCategory::DexUnsupported, Phase::DexExec, cause).with_detail(detail)
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

/// A resolved method that returns void reports `None` — the same
/// value `invoke` callers read as "unresolved, try the builtin path".
/// Wrap it as a null placeholder so a handled call reads as handled.
fn void_resolved(o: Option<V>) -> Option<V> {
    o.or(Some(V::Ref(0)))
}

/// Register slots `args` occupy under dex's register packing —
/// long/double values take two. Name+arity resolution must match
/// this count or a wide-arg override is silently missed.
fn arg_reg_slots(args: &[V]) -> usize {
    args.iter()
        .map(|v| {
            if matches!(v, V::I64(_) | V::F64(_)) {
                2
            } else {
                1
            }
        })
        .sum()
}

/// `(w, h)` of the view owning a SurfaceHolder — its laid-out frame,
/// (0,0) when the view has no frame yet, the window default when the
/// holder has no owning view.
fn surface_holder_size(vm: &Vm, holder: u32) -> (i32, i32) {
    let Some(V::Ref(vh)) = jdk::pseudo_get(vm, holder, jdk::P_TARGET) else {
        return (720, 1280);
    };
    vm.dex_view(vh)
        .and_then(|i| i.frame.map(|(l, t, r, b)| (r - l, b - t)))
        .unwrap_or((0, 0))
}

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
type MethodSigRc = std::rc::Rc<(String, String, String)>;

/// `(field_name, type_desc)` → `(owner_desc, canon key)` for every
/// field across loaded dexes (owners differ under R8; the name+type is
/// what identifies the logical field).
pub(crate) type FieldsByName = HashMap<(String, String), Vec<(String, GIdx)>>;
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
#[derive(Clone, Copy)]
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
pub type HostCall = std::rc::Rc<std::cell::RefCell<dyn FnMut(&mut Vm, &[V]) -> Result<Option<V>>>>;
/// One `(name,sig)`-registered host impl: owner GIdx (or `GX_NONE`
/// for platform classes), its class descriptor, and the call itself.
type NamedHost = (GIdx, String, HostCall);

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
    /// `(GIdx owner or GX_NONE, registered class desc, impl)` — the
    /// desc survives platform-class registrations whose owner has no
    /// dex type index, and matches call sites by `assignable_desc`.
    hosts_named: HashMap<(String, String), Vec<NamedHost>>,
    /// AOT-compiled method bodies (`None` until `enable_aot`).
    aot: Option<Aot>,
    /// Package name of the guest app, set by the launcher so
    /// `Context.getPackageName` etc. answer correctly.
    pub guest_package: Option<String>,
    /// Manifest `application/@android:name` — ApplicationInfo.name
    /// must answer this class (null when undeclared), not the package.
    pub guest_app_class: Option<String>,
    /// Guest-visible filesystem root: `java.io.File` paths resolve
    /// under this directory, never onto the host fs directly.
    pub guest_root: std::path::PathBuf,
    /// Root `/storage/emulated/0` (and its `/sdcard` aliases) maps to —
    /// shared external storage, distinct from the app-private guest_root.
    pub external_root: std::path::PathBuf,
    /// `(package, launcher activity)` of every app installed in the
    /// store — `PackageManager.queryIntentActivities` enumerates them
    /// for launchers, `getLaunchIntentForPackage` resolves per package.
    pub installed_launchers: Vec<(String, String)>,
    /// On-disk path per installed package — `loadIcon` /
    /// `getApplicationIcon` read and decode the APK lazily, so a
    /// non-launcher app doesn't pin every installed APK in memory.
    pub installed_apk_paths: std::collections::BTreeMap<String, std::path::PathBuf>,
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
    pub(crate) main_dispatcher_h: u32,
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
    /// `OVELO_THROWFILT` — optional comma-separated throw trace filter.
    trace_throw_filter: Option<String>,
    /// `OVELO_TRACE_CALL` — log invoke sites whose resolved method name
    /// contains this substring (debug aid).
    trace_call: Option<String>,
    /// `OVELO_TRACE_FROM` — with `OVELO_TRACE_CALL`, additionally require
    /// the *caller* method name to contain this substring.
    trace_from: Option<String>,
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
    /// The frame `setContentView` attaches into, per window owner —
    /// the frame that last wore `android.R.id.content`. AppCompat's
    /// ensureSubDecor calls `frameworkFrame.setId(NO_ID)` before it
    /// moves its own ContentFrameLayout in, so an id-search can't be
    /// trusted to find the attachment point afterwards.
    pub(crate) content_frames: HashMap<u32, u32>,
    /// Last known host window size — decor views are created lazily
    /// inside guest calls and can't wait for the next layout pass to
    /// have bounds.
    pub(crate) window_size: (i32, i32),
    /// Down time of the currently active single-pointer touch gesture.
    pub(crate) touch_gesture_down_time: i64,
    /// Interned host enum constants, keyed (desc, ordinal) — so
    /// `Month.of(1) == Month.JANUARY` and `date.getMonth()` return the
    /// same handle the static constant did.
    pub(crate) enum_cache: HashMap<(String, i32), u32>,
    /// Canonical enum constant objects by (class desc, const name) —
    /// `values()`, `valueOf` and static-field mints share handles so
    /// `Paint.Cap.valueOf("ROUND") == Paint.Cap.ROUND` holds.
    pub(crate) enum_name_cache: HashMap<(String, String), u32>,
    /// Canonical boxed Boolean objects — Boolean.valueOf and the TRUE/FALSE
    /// constants must be identity-stable because bytecode uses reference
    /// comparisons for sentinel/control-flow checks.
    pub(crate) bool_cache: [u32; 2],
    /// Attached `TextClock` views that tick — refreshed once per
    /// `drain_runnables` pass (AOSP ticks them per postDelayed).
    pub clock_views: Vec<u32>,
    /// Installed-APK resource index for `LayoutInflater.inflate` — `None`
    /// degrades inflation to bare placeholder views.
    pub apk_res: Option<Box<res::ApkResources>>,
    /// Guest SQLite connections by database name
    /// (`android.database.sqlite` — see `jdk::db`).
    pub(crate) dbs: HashMap<String, std::rc::Rc<std::cell::RefCell<jdk::db::Db>>>,
    /// Database name -> its `SQLiteDatabase` object handle.
    pub(crate) db_handles: HashMap<String, u32>,
    /// Object handle (db or statement) -> database name it talks to.
    pub(crate) obj_db: HashMap<u32, String>,
    /// androidx.sqlite `BundledSQLite*` JNI handle spaces — Room3's
    /// `ConnectionPool` talks straight to these natives, bypassing the
    /// `android.database` layer. Connections share the `dbs` pool.
    pub(crate) bundled_conns: HashMap<u64, std::rc::Rc<std::cell::RefCell<jdk::db::Db>>>,
    pub(crate) bundled_stmts: HashMap<u64, jdk::db::BundledStmt>,
    /// Next bundled-driver handle (conns and stmts share one counter).
    pub(crate) bundled_h: u64,
    /// Running Service instances by class descriptor — `startService`
    /// reuses the live object so `onCreate` runs once per service.
    pub(crate) services: HashMap<String, u32>,
    /// In-flight Coil3 `RealImageLoader.enqueue` jobs the host fetch
    /// bridge owns: `(http_ticket, image_view)`. `drain_runnables`
    /// delivers decoded bitmaps as drawables — coil's own coroutine
    /// pipeline suspends on a dispatcher our loop never resumes.
    pub(crate) coil_jobs: Vec<(u64, u32)>,
    /// Last `startId` delivered to each running Service — `stopSelf(id)`
    /// only stops when the id matches the most recent request.
    pub(crate) service_start_id: HashMap<u32, i64>,
    /// Monotonic `startId` source for `onStartCommand` dispatches.
    pub(crate) service_seq: i64,
    /// Services that called `stopSelf` before `startService` finished
    /// registering them (self-stop inside `onCreate`) — the starter
    /// must not insert or start them.
    pub(crate) service_stop_pending: hashbrown::HashSet<u32>,
    /// Services whose `onDestroy` is owed on the next drain — deferred
    /// so `onStartCommand`/`startService` finish before teardown runs.
    pub(crate) pending_destroy: Vec<u32>,
    /// Accepted JobScheduler jobs `(JobInfo, ComponentName, due_ms)`
    /// waiting for a between-callback service dispatch.
    pub(crate) pending_jobs: Vec<(u32, u32, i64)>,
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
    /// class descriptor → global type_idx. `type_index_of` walks every
    /// dex's whole type table per call (each entry is a MUTF-8 decode);
    /// assignability checks and `canon` issue it per dispatch, so
    /// misses are cached too.
    type_idx_cache: std::cell::RefCell<HashMap<String, Option<GIdx>>>,
    /// `(field_name, field_type)` → canonical field GIdxs across all
    /// dexes, built lazily — lets host code write fields on dex
    /// instances under every owner a reader's field ref might name
    /// (R8 renames owners like `ViewGroup$LayoutParams` arbitrarily).
    fields_named_cache: std::cell::RefCell<Option<FieldsByName>>,
    /// `midx` → `(class, name, proto_sig)` — decoding the proto string
    /// re-reads half the dex tables per call; layout/measure passes
    /// issue it tens of thousands of times on the same methods.
    method_sig_cache: std::cell::RefCell<HashMap<GIdx, MethodSigRc>>,
    /// `midx` → parsed `code_item` — `exec_frame` re-parsed registers,
    /// tries and handlers on every single call of a method.
    code_cache: std::cell::RefCell<HashMap<GIdx, std::rc::Rc<ovelo_dex::CodeItem>>>,
    /// `midx` → decoded instruction table — `exec_frame` re-decoded
    /// every instruction of a method on every call into it.
    insn_cache: std::cell::RefCell<HashMap<GIdx, std::rc::Rc<Vec<Insn>>>>,
    /// `midx` → dense code-unit offset → instruction-index map. A
    /// binary search per executed opcode is the interpreter's hottest
    /// fixed cost in large parser methods.
    pcmap_cache: std::cell::RefCell<HashMap<GIdx, std::rc::Rc<Vec<u32>>>>,
    /// `midx` → canonical field for methods exactly shaped as a simple
    /// instance getter (`iget* this; return*`). Compose/data classes
    /// call these hundreds of thousands of times per frame.
    simple_getter_cache: std::cell::RefCell<HashMap<GIdx, Option<GIdx>>>,
    /// Reused register storage for completed interpreter frames —
    /// lexer/parser code calls tiny methods millions of times, and a
    /// fresh zeroed Vec per call dominates allocation traffic.
    reg_pool: Vec<Vec<V>>,
    /// `midx` → display name `Lcls;.name(sig)ret` — `last_call` was
    /// formatting one per invoke.
    method_desc_cache: std::cell::RefCell<HashMap<GIdx, std::rc::Rc<str>>>,
    /// `midx` → whether it is `Dispatchers.getMain` — decoded once instead
    /// of decoding class/name strings for every static invoke.
    main_getter_cache: std::cell::RefCell<HashMap<GIdx, bool>>,
    /// `(referenced midx, receiver class, defined-fallback?)` → resolved
    /// dispatch `Loc` — `resolve_virtual` re-walked the superclass chain
    /// decoding every candidate's name+proto on every invoke.
    resolve_cache: std::cell::RefCell<HashMap<(GIdx, GIdx, bool), Option<Loc>>>,
    /// `field GIdx` → canonical `field GIdx` — `canon_field` rebuilt the
    /// `(owner,name,type)` sig strings for every iget/iput.
    canon_field_cache: std::cell::RefCell<HashMap<GIdx, GIdx>>,
    /// `midx` → registered host impl — `host_named` decoded the ref's
    /// name+proto and re-ran assignability on every unresolved invoke.
    host_named_cache: std::cell::RefCell<HashMap<GIdx, Option<HostCall>>>,
    /// `(class GIdx, method name, arg slots)` → method GIdx — host→guest
    /// `call_virtual` (measure/layout/onViewAdded per node per pass)
    /// re-walked the superclass chain per call.
    named_chain_cache: std::cell::RefCell<HashMap<(GIdx, String, String), Option<GIdx>>>,
    /// `(sub GIdx, sup GIdx)` → is-assignable — the chain+interface walk
    /// ran per check-cast / host registration probe.
    assignable_cache: std::cell::RefCell<HashMap<(GIdx, GIdx), bool>>,
    /// `GIdx` → descriptor — `type_desc_of` re-decoded the MUTF-8 string
    /// table on every call; assignability/super walks issue it per hop.
    type_desc_cache: std::cell::RefCell<HashMap<GIdx, Option<String>>>,
    /// descriptor → superclass descriptor — `super_desc` resolved the
    /// dex class_def + its super string on every hop of every
    /// `assignable_desc` walk.
    super_desc_cache: std::cell::RefCell<HashMap<String, Option<String>>>,
    /// `sub` descriptor → `sup` descriptor → is-assignable — nested so
    /// lookups borrow `&str` keys without allocating a tuple.
    /// `assignable_desc` is hit by `isInstance`/`isAssignableFrom` in
    /// guest hot loops; each call re-walked the whole superclass chain.
    assignable_desc_cache: std::cell::RefCell<HashMap<String, HashMap<String, bool>>>,
    /// `midx` → `(params)ret` signature — the unresolved/builtin dispatch
    /// path re-decoded the proto's full param list on every invoke.
    proto_sig_cache: std::cell::RefCell<HashMap<GIdx, String>>,
    /// const-string handle by global dex-local string index.
    str_const_cache: std::cell::RefCell<HashMap<GIdx, u32>>,
    /// Bumped by every `pseudo_set` — lets the host render/layout
    /// passes skip rebuilding the dex-view tree when nothing changed.
    pub view_gen: u64,
    /// `view_gen` value the last driven layout walk ran against —
    /// `u64::MAX` until the first pass so it always runs once.
    pub layout_gen_done: std::cell::Cell<u64>,
    /// Heap size (slots) that triggers the next GC at a quiescent
    /// point; doubles after each collection.
    pub gc_mark: usize,
    /// Deferred async work — `(runnable, on_main)`. `Thread.start`,
    /// `Executor.execute`/`submit` and `Handler.post*` enqueue here
    /// instead of running inline (AOSP is async; inline execution
    /// deadlocks on self-rescheduling executors). Drained at the
    /// outermost `call_virtual` boundary.
    pub pending_runnables: std::collections::VecDeque<(u32, bool)>,
    /// Executor objects created by `Executors.newSingleThreadExecutor`.
    /// Their tasks must run one at a time; treating `execute` as an
    /// unbounded global queue lets rate limiters (kotlinx/Tasks.org
    /// `Throttle`) run every queued task concurrently.
    pub(crate) single_executors: hashbrown::HashSet<u32>,
    /// executor handle -> FIFO tasks waiting for its active task.
    pub(crate) executor_queues: HashMap<u32, std::collections::VecDeque<u32>>,
    /// executor handle -> currently running runnable.
    pub(crate) executor_active: HashMap<u32, u32>,
    /// runnable handle -> owning single-thread executor.
    pub(crate) runnable_executor: HashMap<u32, u32>,
    /// Views whose attach state changed while guest composition/layout
    /// work was active. `onAttachedToWindow` is delivered on a later
    /// drain boundary so nested Compose content cannot compose inside
    /// the composition that materialized its view.
    pub(crate) pending_attach_callbacks: std::collections::VecDeque<u32>,
    /// `Choreographer.postFrameCallback*` — `(callback handle, due_ms)`.
    /// Drained once per `drain_runnables` pass (= once per frame tick)
    /// as `doFrame(J)V`; the Compose Recomposer's frame clock and
    /// `View.postOnAnimation` both schedule through it.
    pub(crate) frame_callbacks: Vec<(u32, u64)>,
    /// Cooperative single-thread scheduling alternates main and worker
    /// queues when both have work. Real Android runs workers on parallel
    /// threads; strict FIFO lets continuously re-posted Compose main-queue
    /// work starve long-lived repository/parser workers forever.
    sched_prefer_worker: bool,
    /// Any `GLSurfaceView.setRenderer` has been bound. Non-GL apps
    /// must not scan every view every frame looking for renderers.
    pub(crate) gl_renderer_active: bool,
    /// The one `Choreographer.getInstance()` handle — identity matters
    /// (dex code compares instances).
    pub(crate) choreographer_h: u32,
    /// The shared `ByteOrder.nativeOrder()` object — buffer helpers
    /// (Korge's `Lb8`) request it per window write, 100k+/10s.
    pub(crate) byteorder_h: std::cell::Cell<u32>,
    /// `Application.ActivityLifecycleCallbacks` handles registered via
    /// `registerActivityLifecycleCallbacks` — application-global in
    /// AOSP. androidx ReportFragment relies on these to drive
    /// `ComponentActivity.mLifecycleRegistry` through its states.
    pub(crate) activity_callbacks: Vec<u32>,
    /// Time-gated posts (`Handler.postDelayed`/`sendMessageDelayed`) —
    /// `(handle, on_main, due_ms)`. Promoted into `pending_runnables` by
    /// `drain_runnables` once due; without this a self-rescheduling
    /// handler floods every drain tick regardless of its delay.
    pub(crate) delayed_runnables: Vec<(u32, bool, u64)>,
    /// `SurfaceHolder.addCallback` deliveries queued for the next
    /// drain tick: `(holder, callback, retries)`. Android only fires
    /// surfaceCreated/surfaceChanged once a real surface exists —
    /// calling them inline during `init()` hits views whose GL thread
    /// field is not assigned yet (Godot NPE'd exactly so). The size is
    /// resolved from the owning view's frame at delivery, not when
    /// `addCallback` runs (the view may not be laid out yet).
    pub(crate) pending_surface: Vec<(u32, u32, u32)>,
    /// Registered `(holder, callback)` pairs kept for redispatching
    /// `surfaceChanged` when the owning view's frame later changes
    /// (e.g. it was 0x0 when `addCallback` ran).
    pub(crate) surface_cbs: Vec<(u32, u32)>,
    /// Last `(w, h)` reported to each holder via `surfaceChanged`.
    pub(crate) surface_sizes: HashMap<u32, (i32, i32)>,
    /// `(midx, args)` of host calls that lost the borrow race: a guest
    /// call re-entered an already-running host callback (attach →
    /// onAttachedToWindow → addView). The dex-side bookkeeping ran via
    /// the builtin fallback, but the bridge's own side effects (GuestUi
    /// wiring, host-view materialization) did not — retry once the
    /// outer borrow is released, on the next drain tick.
    pub(crate) pending_host_retry: Vec<(GIdx, Vec<V>)>,
    /// Throwable a registered host callback wants the invoke dispatch
    /// to raise as a guest exception (host callbacks themselves cannot
    /// return `E::Throw`).
    pub(crate) host_throw: Option<u32>,
    /// Set while the drain loop replays `pending_host_retry` — lets a
    /// bridged method tell a retried invocation from a fresh one
    /// (the builtin fallback already ran the dex side).
    pub(crate) in_host_retry: bool,
    /// Runnables spawned by `Thread.start` — long-lived loops that
    /// legitimately park/reschedule forever (game loops like bouncy's
    /// FieldDriver), unlike executor workers which the repark budget
    /// exists to stop. Keyed by runnable handle.
    pub(crate) thread_runnables: hashbrown::HashSet<u32>,
    /// Runnable handle → the Thread that `start()`ed it, so isAlive
    /// distinguishes which Thread a shared Runnable runs on.
    pub(crate) runnable_thread: hashbrown::HashMap<u32, u32>,
    /// The runnable `run_on_worker` is executing right now — lets
    /// `Thread.sleep(ms)` record a wake deadline against it.
    pub(crate) cur_runnable: u32,
    /// Runnable handle -> wall-clock ms the parked thread asked to
    /// wake at (`Thread.sleep` in worker context writes; the drain
    /// reads when requeueing into `delayed_runnables`).
    pub(crate) sleep_until: HashMap<u32, u64>,
    /// Runnables whose `sleep_until` deadline came from
    /// `LockSupport.park*`/`Object.wait(ms)` — interruptible waits an
    /// `unpark`/`notify` may cut short, unlike `Thread.sleep`.
    pub(crate) park_waiters: hashbrown::HashSet<u32>,
    /// Handles currently sitting in `delayed_runnables` whose deadline
    /// is interruptible — `wake_parked` promotes these early.
    pub(crate) unparkable_delayed: hashbrown::HashSet<u32>,
    /// Runnables whose latest promotion out of `delayed_runnables` was
    /// their own deadline expiring — not an `unpark`. An interruptible
    /// parker that re-parks straight after a deadline wake ticks
    /// `deadline_park_cycles` (a kotlinx surplus worker re-parks here
    /// when its keep-alive check should have retired it).
    pub(crate) deadline_woke: hashbrown::HashSet<u32>,
    /// Consecutive deadline-expiry wake→re-park cycles per runnable —
    /// at the cap the runnable is retired into `parked_runnables`
    /// (still revivable by `unpark`/`enqueue_runnable`), approximating
    /// coroutine surplus-worker retirement.
    pub(crate) deadline_park_cycles: HashMap<u32, u32>,
    /// Runnables whose re-run after a long `Thread.sleep` park must
    /// skip the first sleep call: `run()` re-executes from the top,
    /// so without the flag a one-shot task that sleeps mid-body
    /// (`load(); sleep(200); post()`) re-parks at the same call on
    /// every drain and never finishes.
    pub(crate) slept_once: hashbrown::HashSet<u32>,
    /// ViewTreeObserver handle -> registered onGlobalLayout listeners.
    pub(crate) vto_listeners: HashMap<u32, Vec<u32>>,
    /// ViewTreeObserver handle -> `OnPreDrawListener` handles — Glide's
    /// ViewTarget waits on onPreDraw to learn the view's real size
    /// before kicking off the decode.
    pub(crate) vto_predraw: HashMap<u32, Vec<u32>>,
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
    reparked: hashbrown::HashSet<u32>,
    /// Array handles whose fill-loop peephole scan found a non-uniform
    /// range — re-scanning every loop iteration is O(n²), so a failed
    /// handle is remembered and the check skipped from then on. A handle
    /// that later becomes uniform just loses a skip (perf only).
    fill_bad: hashbrown::HashSet<u32>,
    /// Consecutive `park_drain` calls that drained only re-parked
    /// workers — bounded so mutual waiters yield to the outer drain
    /// (which re-runs them with a fresh budget) instead of spinning
    /// inside one `run()` until the budget kills the task.
    park_spins: u32,
    /// Consecutive park→requeue cycles per runnable handle — bounds
    /// the spin where a worker parks on a queue nothing can ever feed
    /// (e.g. `ReferenceQueue.remove` under our immediate-return stub):
    /// past the cap the requeue is dropped instead of re-running
    /// `run()` from the top each drain.
    repark_counts: HashMap<u32, u32>,
    /// Runnables that parked out of the live queue (dropped after the
    /// repark bound). Re-armed when fresh work is posted or `unpark`
    /// fires — either may feed a dropped worker's internal queue.
    parked_runnables: HashMap<u32, bool>,
    /// Activity a guest `startActivity` asked to launch — (class
    /// descriptor, the Intent the caller passed). The runner drains
    /// it into a real second-Activity dispatch.
    pub pending_activity: Option<(String, V)>,
    /// canonical class_idx → `java.lang.Class` mirror handle — Class
    /// objects are singletons (`Foo.class == x.getClass()`).
    class_mirrors: HashMap<GIdx, u32>,
    /// MediaStore `_id` per `_data` path — ids persist for the
    /// runtime's life so adding/removing other files never reshuffles
    /// a file's id (real MediaStore rowid semantics).
    pub media_ids: HashMap<String, i64>,
    /// next MediaStore `_id` to hand out (see `media_ids`).
    pub next_media_id: i64,
    /// Native `.so` images loaded through `System.loadLibrary` — the
    /// real JNI path for games (libGDX/Box2D) and other native libs.
    pub natives: Vec<native::NativeLib>,
    /// JNI jmethodID table — id = index+1 → dex method GIdx.
    pub jni_mids: Vec<GIdx>,
    /// JNI jfieldID table — id = index+1 → field GIdx.
    pub jni_fids: Vec<GIdx>,
    /// By-name `jmethodID`s: `GetMethodID` misses on host/JDK classes
    /// (reflection APIs the dex chain doesn't declare) get a tagged
    /// entry here — `(owner descriptor, name, signature)` — resolved
    /// at call time through the normal name dispatch. Indexed by
    /// `mid & !GX_NAMED`; see jni.rs.
    pub(crate) jni_named: Vec<(String, String, String)>,
    /// Exception a native call left pending (Throw/ThrowNew); 0 = none,
    /// `u32::MAX` = thrown without an object handle.
    pub jni_pending_exc: u32,
    /// Permission names currently granted to the guest package —
    /// seeded from the manifest's normal-level `<uses-permission>`
    /// entries and grown by `requestPermissions` (user grants all).
    pub granted_permissions: hashbrown::HashSet<String>,
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

/// Diagnostics: when the embedding binary runs all Rust allocations
/// through a private malloc zone (ovelo-cli's ZoneAlloc), it publishes
/// that zone's pointer here so OVELO_ZONECHECK can name it.
#[doc(hidden)]
pub static RUST_ZONE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Diagnostics: `malloc_zone_check` every process zone; aborts at the
/// first corrupt one naming `tag` — a milestone in OVELO_ZONECHECK runs.
#[doc(hidden)]
pub fn check_zones(tag: &str) {
    native::check_all_zones(tag)
}

/// Resolve a theme attribute to an ARGB color — `android.R.attr.windowBackground`
/// and friends. Chases `?attr` hops through the bound theme and
/// `@drawable` references to a representative flat color.
pub fn theme_color(vm: &Vm, attr_id: u32) -> Option<u32> {
    let res = vm.apk_res.as_ref()?;
    let mut cur = attr_id;
    for _ in 0..8 {
        let ev = res.style_item(vm.theme_id, cur).cloned()?;
        match ev.data_type {
            0x02 => cur = ev.data,
            0x1c..=0x1f | 0x10 | 0x11 => return Some(ev.data),
            0x01 => {
                return jdk::vector::resolve_ref_color(
                    jdk::vector::RCtx {
                        res,
                        theme: vm.theme_id,
                        depth: 0,
                    },
                    ev.data,
                )
                .or_else(|| {
                    jdk::vector::drawable_flat_color(
                        jdk::vector::RCtx {
                            res,
                            theme: vm.theme_id,
                            depth: 0,
                        },
                        ev.data,
                    )
                });
            }
            0x03 => return ev.string.as_deref().and_then(jdk::vector::parse_color),
            _ => return None,
        }
    }
    None
}

/// `theme_color` looked up by attr name (`colorSurface`, …) — used for
/// fallbacks when `windowBackground` points at a framework drawable the
/// APK doesn't carry.
pub fn theme_color_named(vm: &Vm, attr_name: &str) -> Option<u32> {
    let res = vm.apk_res.as_ref()?;
    theme_color(vm, res.attr_id_named(attr_name)?)
}

/// Rasterize an installed package's launcher icon from raw APK
/// bytes — vector XML, adaptive icon layers, or a raster image,
/// whichever `application/@android:icon` references.
pub fn package_icon_rgba(apk: &[u8]) -> Option<(u32, u32, std::rc::Rc<Vec<u8>>)> {
    let ai = ovelo_apk::inspect_apk(apk).ok()?;
    let icon = ai.manifest.icon?;
    let res = res::ApkResources::open(apk.to_vec())?;
    let (w, h, px) = jdk::vector::drawable_bitmap(
        jdk::vector::RCtx {
            res: &res,
            theme: ai.manifest.theme.unwrap_or(0),
            depth: 0,
        },
        icon,
    )?;
    // Callers per-pixel-process these (e.g. Palette/average-color
    // helpers interpreted in dex) — cap at a display-size decode.
    const MAX: u32 = 144;
    if w <= MAX && h <= MAX {
        return Some((w, h, px));
    }
    let scale = MAX as f64 / w.max(h) as f64;
    let (tw, th) = (
        ((w as f64 * scale).round() as u32).max(1),
        ((h as f64 * scale).round() as u32).max(1),
    );
    let mut out = vec![0u8; (tw * th * 4) as usize];
    for y in 0..th {
        for x in 0..tw {
            let sx = (x as f64 / scale) as u32;
            let sy = (y as f64 / scale) as u32;
            let (si, di) = (((sy * w + sx) * 4) as usize, ((y * tw + x) * 4) as usize);
            out[di..di + 4].copy_from_slice(&px[si..si + 4]);
        }
    }
    Some((tw, th, std::rc::Rc::new(out)))
}

/// `application/@android:label` of an installed APK — resource
/// reference resolved through its own arsc, or the literal string.
/// `PackageItemInfo.loadLabel` resolves to this, not the component's
/// class name.
pub fn package_label(apk: &[u8]) -> Option<String> {
    let ai = ovelo_apk::inspect_apk(apk).ok()?;
    match ai.manifest.label? {
        ovelo_apk::manifest::Value::Str(s) => Some(s),
        ovelo_apk::manifest::Value::Reference(r) => {
            let res = res::ApkResources::open(apk.to_vec())?;
            res.string_of(r).map(|s| s.to_string())
        }
        _ => None,
    }
}

/// Hold the VM's recursive mutex for a batch of driver-side work.
/// `invoke` self-locks, but the driver also touches `&mut Vm` between
/// invokes (runnable-drain bookkeeping, GC, dex-tree refresh) — wrap
/// those batches so guest-pthread JNI calls can't alias them.
pub fn lock_vm() -> impl Drop {
    jni::vm_lock()
}

impl Vm {
    /// Attach a guest ContentProvider to its application Context for
    /// runtime-driven provider startup. AOSP performs this inside
    /// `attachInfo`; dex overrides can validate authority metadata the
    /// runtime does not yet model, while the behavioral requirement
    /// here is that `getContext()` answers the application.
    pub fn set_provider_context(&mut self, provider: u32, context: V) {
        jdk::pseudo_set(self, provider, jdk::view::P_CONTEXT, context);
    }

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
            guest_app_class: None,
            guest_root: std::env::temp_dir().join("ovelo-guest"),
            external_root: std::env::temp_dir()
                .join("ovelo-guest")
                .join("storage")
                .join("emulated")
                .join("0"),
            installed_launchers: Vec::new(),
            installed_apk_paths: std::collections::BTreeMap::new(),
            depth: 0,
            stack: Vec::new(),
            throw_sites: HashMap::new(),
            stub_trace: Vec::new(),
            main_thread_h: 0,
            cur_thread: 0,
            worker_thread_h: 0,
            main_looper_h: 0,
            main_dispatcher_h: 0,
            app_h: 0,
            unsafe_offsets: HashMap::new(),
            unsafe_next_offset: 0,
            trace_field: std::env::var("OVELO_TRACE_FIELD").ok(),
            trace_throw: crate::dbg_flag("OVELO_TRACE_THROW"),
            trace_throw_filter: std::env::var("OVELO_THROWFILT").ok(),
            trace_call: std::env::var("OVELO_TRACE_CALL").ok(),
            trace_from: std::env::var("OVELO_TRACE_FROM").ok(),
            view_roots: Vec::new(),
            prefs_files: HashMap::new(),
            cur_activity: 0,
            content_views: HashMap::new(),
            decor_roots: HashMap::new(),
            content_frames: HashMap::new(),
            window_size: (720, 1280),
            touch_gesture_down_time: -1,
            enum_cache: HashMap::new(),
            enum_name_cache: HashMap::new(),
            bool_cache: [0, 0],
            clock_views: Vec::new(),
            apk_res: None,
            dbs: HashMap::new(),
            db_handles: HashMap::new(),
            obj_db: HashMap::new(),
            bundled_conns: HashMap::new(),
            bundled_stmts: HashMap::new(),
            bundled_h: 0,
            services: HashMap::new(),
            coil_jobs: Vec::new(),
            service_start_id: HashMap::new(),
            service_seq: 0,
            service_stop_pending: hashbrown::HashSet::new(),
            pending_destroy: Vec::new(),
            pending_jobs: Vec::new(),
            theme_id: 0,
            total_steps: 0,
            pending_activity: None,
            pending_runnables: std::collections::VecDeque::new(),
            single_executors: hashbrown::HashSet::new(),
            executor_queues: HashMap::new(),
            executor_active: HashMap::new(),
            runnable_executor: HashMap::new(),
            pending_attach_callbacks: std::collections::VecDeque::new(),
            frame_callbacks: Vec::new(),
            sched_prefer_worker: false,
            gl_renderer_active: false,
            choreographer_h: 0,
            byteorder_h: std::cell::Cell::new(0),
            activity_callbacks: Vec::new(),
            delayed_runnables: Vec::new(),
            pending_surface: Vec::new(),
            surface_cbs: Vec::new(),
            pending_host_retry: Vec::new(),
            host_throw: None,
            in_host_retry: false,
            surface_sizes: HashMap::new(),
            thread_runnables: hashbrown::HashSet::new(),
            runnable_thread: hashbrown::HashMap::new(),
            cur_runnable: 0,
            sleep_until: HashMap::new(),
            slept_once: hashbrown::HashSet::new(),
            vto_listeners: HashMap::new(),
            vto_predraw: HashMap::new(),
            draining: false,
            park_depth: 0,
            reparked: hashbrown::HashSet::new(),
            fill_bad: hashbrown::HashSet::new(),
            park_spins: 0,
            repark_counts: HashMap::new(),
            parked_runnables: HashMap::new(),
            park_waiters: hashbrown::HashSet::new(),
            unparkable_delayed: hashbrown::HashSet::new(),
            deadline_woke: hashbrown::HashSet::new(),
            deadline_park_cycles: HashMap::new(),
            media_ids: HashMap::new(),
            next_media_id: 0,
            natives: Vec::new(),
            jni_mids: Vec::new(),
            jni_named: Vec::new(),
            jni_fids: Vec::new(),
            jni_pending_exc: 0,
            field_idx_cache: std::cell::RefCell::new(HashMap::new()),
            type_idx_cache: std::cell::RefCell::new(HashMap::new()),
            fields_named_cache: std::cell::RefCell::new(None),
            method_sig_cache: std::cell::RefCell::new(HashMap::new()),
            code_cache: std::cell::RefCell::new(HashMap::new()),
            insn_cache: std::cell::RefCell::new(HashMap::new()),
            pcmap_cache: std::cell::RefCell::new(HashMap::new()),
            simple_getter_cache: std::cell::RefCell::new(HashMap::new()),
            reg_pool: Vec::new(),
            method_desc_cache: std::cell::RefCell::new(HashMap::new()),
            main_getter_cache: std::cell::RefCell::new(HashMap::new()),
            resolve_cache: std::cell::RefCell::new(HashMap::new()),
            canon_field_cache: std::cell::RefCell::new(HashMap::new()),
            host_named_cache: std::cell::RefCell::new(HashMap::new()),
            named_chain_cache: std::cell::RefCell::new(HashMap::new()),
            assignable_cache: std::cell::RefCell::new(HashMap::new()),
            type_desc_cache: std::cell::RefCell::new(HashMap::new()),
            super_desc_cache: std::cell::RefCell::new(HashMap::new()),
            assignable_desc_cache: std::cell::RefCell::new(HashMap::new()),
            proto_sig_cache: std::cell::RefCell::new(HashMap::new()),
            str_const_cache: std::cell::RefCell::new(HashMap::new()),
            view_gen: 0,
            layout_gen_done: std::cell::Cell::new(u64::MAX),
            gc_mark: 65_536,
            class_mirrors: HashMap::new(),
            granted_permissions: hashbrown::HashSet::new(),
        };
        v.add_dex(dex)?;
        Ok(v)
    }

    /// Seed install-time grants: every declared `<uses-permission>` is
    /// granted — the runtime has no permission UI, and apps that check
    /// `hasPermission` before requesting (fossify, …) would otherwise
    /// park on a grant screen forever.
    pub fn grant_manifest_permissions(&mut self, declared: &[String]) {
        for p in declared {
            self.granted_permissions.insert(p.clone());
        }
    }

    /// Merge another DEX (classes2.dex, …) into the shared index space.
    /// A class defined in a later dex resolves from invokes in earlier
    /// ones — same semantics as ART's unified class path.
    pub fn add_dex(&mut self, dex: DexFile) -> Result<()> {
        // A new dex can resolve names previous lookups missed.
        self.field_idx_cache.borrow_mut().clear();
        self.type_idx_cache.borrow_mut().clear();
        self.fields_named_cache.borrow_mut().take();
        self.method_sig_cache.borrow_mut().clear();
        self.code_cache.borrow_mut().clear();
        self.insn_cache.borrow_mut().clear();
        self.pcmap_cache.borrow_mut().clear();
        self.method_desc_cache.borrow_mut().clear();
        self.main_getter_cache.borrow_mut().clear();
        self.resolve_cache.borrow_mut().clear();
        self.canon_field_cache.borrow_mut().clear();
        self.host_named_cache.borrow_mut().clear();
        self.named_chain_cache.borrow_mut().clear();
        self.assignable_cache.borrow_mut().clear();
        self.type_desc_cache.borrow_mut().clear();
        self.super_desc_cache.borrow_mut().clear();
        self.assignable_desc_cache.borrow_mut().clear();
        self.proto_sig_cache.borrow_mut().clear();
        self.str_const_cache.borrow_mut().clear();
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
        jdk::http::install(self);
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
    /// Canonical field read by a method whose entire body is a simple
    /// instance getter. The shape check mirrors the interpreter's normal
    /// `iget`/`return` semantics, including receiver-register placement.
    fn simple_getter_field(&self, loc: Loc) -> Option<GIdx> {
        if let Some(hit) = self.simple_getter_cache.borrow().get(&loc.midx) {
            return *hit;
        }
        let out = (|| {
            let d = self.dex_of(loc.midx)?;
            let code = d.code_item(loc.em.code_off).ok()??;
            if code.ins_size != 1 || code.registers_size < code.ins_size {
                return None;
            }
            let insns = decode_all(&code.insns).ok()?;
            let [get, ret] = insns.as_slice() else {
                return None;
            };
            let expected_ret = match get.opcode {
                0x52 | 0x55..=0x58 => 0x0f,
                0x53 => 0x10,
                0x54 => 0x11,
                _ => return None,
            };
            if ret.opcode != expected_ret
                || get.regs.len() != 2
                || ret.regs.first() != get.regs.first()
                || usize::from(get.regs[1])
                    != usize::from(code.registers_size) - usize::from(code.ins_size)
            {
                return None;
            }
            Some(self.canon_field(gx(gd(loc.midx), get.idx?)))
        })();
        self.simple_getter_cache.borrow_mut().insert(loc.midx, out);
        out
    }

    pub(crate) fn canon_field(&self, g: GIdx) -> GIdx {
        if let Some(hit) = self.canon_field_cache.borrow().get(&g) {
            return *hit;
        }
        let out = (|| {
            let d = self.dex_of(g)?;
            let f = d.field(gl(g)).ok()?;
            let owner_desc = d.type_descriptor(f.class_idx as u32).ok()?;
            let name = d.string(f.name_idx).ok()?;
            // Resolve the ref through the class hierarchy to the class
            // that actually declares the field: `LPolygonShape;.addr`
            // and `LShape;.addr` then share one storage slot.
            if let Some(owner) = self.type_index_of(&owner_desc) {
                let fty = d.type_descriptor(f.type_idx as u32).ok();
                if let Some(decl) = self
                    .find_field_by_name(owner, &name, false, fty.as_deref())
                    .or_else(|| self.find_field_by_name(owner, &name, true, fty.as_deref()))
                {
                    return Some(decl);
                }
            }
            let sig = (
                owner_desc,
                name,
                d.type_descriptor(f.type_idx as u32).unwrap_or_default(),
            );
            Some(self.fields_by_sig.get(&sig).copied().unwrap_or(g))
        })()
        .unwrap_or(g);
        self.canon_field_cache.borrow_mut().insert(g, out);
        out
    }

    /// (canonical GIdx of the class itself, its &ClassDef) — `def`
    /// fields (`superclass_idx`, `interfaces_off`, …) are local to the
    /// DEFINING dex, whose index is `gd(canon_g)`.
    pub(crate) fn def_loc_of(&self, class_idx: GIdx) -> Option<(GIdx, &ClassDef)> {
        let desc = self.type_desc_of(class_idx).ok()?;
        // APKs ship D8-generated `android.*` stub defs whose every method
        // throws NoClassDefFoundError (minSdk gating for post-release
        // platform APIs). On a real device the platform class wins, so
        // shadow the dex def the same way: treat it as not defined.
        // `android.support.*` is bundled library code (legacy support
        // lib ships inside the app, not the platform) — never shadow it.
        if desc.starts_with("Landroid/") && !desc.starts_with("Landroid/support/") {
            return None;
        }
        let hit = self.defs.get(&desc).copied();
        if crate::dbg_flag("OVELO_ANNDBG")
            && (desc.contains("Navigator$Name") || desc.contains("ComposeNavigator"))
        {
            eprintln!("[anndbg] def_loc_of {desc} -> {hit:?}");
        }
        let (di, i) = hit?;
        let def = &self.dexes[di].class_defs()[i];
        Some((gx(di, def.class_idx), def))
    }

    /// The dex a global index belongs to.
    pub(crate) fn dex_of(&self, g: GIdx) -> Option<&DexFile> {
        self.dexes.get(gd(g))
    }

    /// Loaded dexes, in index order (dex 0 first — same order `add_dex`
    /// received them).
    pub(crate) fn all_dexes(&self) -> &[DexFile] {
        &self.dexes
    }

    /// `type_idx` lookup across every loaded dex (global result).
    /// Memoized — dex type tables don't change after `add_dex`.
    pub(crate) fn type_index_of(&self, desc: &str) -> Option<GIdx> {
        if let Some(hit) = self.type_idx_cache.borrow().get(desc) {
            return *hit;
        }
        let mut out = None;
        for (di, d) in self.dexes.iter().enumerate() {
            if let Ok(Some(i)) = d.type_index_of(desc) {
                out = Some(gx(di, i));
                break;
            }
        }
        self.type_idx_cache
            .borrow_mut()
            .insert(desc.to_string(), out);
        out
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

    /// Canonical field keys for `(name, type)` across every loaded dex,
    /// regardless of declaring owner — used when host code must write a
    /// field on a dex instance that readers reach through arbitrarily
    /// (R8-)named owner references. Each entry carries its declaring
    /// owner so callers can filter to a receiver's own hierarchy.
    pub(crate) fn field_keys_named(&self, name: &str, ty: &str) -> Vec<(String, GIdx)> {
        if let Some(m) = self.fields_named_cache.borrow().as_ref() {
            return m
                .get(&(name.to_string(), ty.to_string()))
                .cloned()
                .unwrap_or_default();
        }
        let mut m: FieldsByName = HashMap::new();
        for (di, d) in self.dexes.iter().enumerate() {
            for i in 0..d.field_ids_len() {
                let Ok(f) = d.field(i as u32) else { continue };
                let (Ok(n), Ok(t), Ok(owner)) = (
                    d.string(f.name_idx),
                    d.type_descriptor(f.type_idx as u32),
                    d.type_descriptor(f.class_idx as u32),
                ) else {
                    continue;
                };
                let g = self.canon_field(gx(di, i as u32));
                m.entry((n, t)).or_default().push((owner, g));
            }
        }
        let out = m
            .get(&(name.to_string(), ty.to_string()))
            .cloned()
            .unwrap_or_default();
        *self.fields_named_cache.borrow_mut() = Some(m);
        out
    }

    /// Descriptor for a global `type_idx`.
    /// Class index a `jclass` handle refers to: `Obj::Class` mirrors
    /// report their `type_idx`; ordinary objects report their own
    /// class via `class_of`.
    pub fn class_for_jclass(&self, h: u32) -> Option<u64> {
        match self.heap.get(h) {
            Some(Obj::Class { type_idx }) => Some(*type_idx),
            _ => self.heap.class_of(h),
        }
    }

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

    /// Debug: a view's recorded P_PARENT handle (0 when none).
    pub fn dbg_view_parent(&self, h: u32) -> u32 {
        jdk::view::view_parent(self, h)
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

    /// `is_guest_collection` for `java.util.Map` — dex subclasses of
    /// `HashMap` (nanojson `JsonObject`, …) need a mutation backing
    /// store the same way list subclasses do.
    pub(crate) fn is_guest_map(&self, h: u32) -> bool {
        let Some(Obj::Instance { class_idx, .. }) = self.heap.get(h) else {
            return false;
        };
        self.type_index_of("Ljava/util/Map;")
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
        if let Some(hit) = self.type_desc_cache.borrow().get(&g) {
            return hit.clone().ok_or_else(|| err("dexrt.dex.idx"));
        }
        let out = self.dex_of(g).and_then(|d| d.type_descriptor(gl(g)).ok());
        self.type_desc_cache.borrow_mut().insert(g, out.clone());
        out.ok_or_else(|| err("dexrt.dex.idx"))
    }

    /// `(params)ret` for a method ref, memoized — the unresolved /
    /// builtin dispatch path decodes the proto on every invoke.
    fn proto_sig_of(&self, midx: GIdx) -> Rt<String> {
        if let Some(hit) = self.proto_sig_cache.borrow().get(&midx) {
            return Ok(hit.clone());
        }
        let d = self
            .dex_of(midx)
            .ok_or_else(|| E::Vm(err("dexrt.dex.idx")))?;
        let mid = *d.method(gl(midx)).map_err(E::Vm)?;
        let out = proto_sig(d, mid.proto_idx as u32).map_err(E::Vm)?;
        self.proto_sig_cache.borrow_mut().insert(midx, out.clone());
        Ok(out)
    }

    /// `Lowner;.name(params)ret` for a global method ref, or None when
    /// the index doesn't resolve (sentinel).
    fn is_main_getter(&self, midx: GIdx) -> bool {
        if let Some(hit) = self.main_getter_cache.borrow().get(&midx) {
            return *hit;
        }
        let is = self
            .dex_of(midx)
            .and_then(|d| {
                let mid = d.method(gl(midx)).ok()?;
                let class = d.type_descriptor(mid.class_idx as u32).ok()?;
                let name = d.string(mid.name_idx).ok()?;
                Some(class == "Lkotlinx/coroutines/Dispatchers;" && name == "getMain")
            })
            .unwrap_or(false);
        self.main_getter_cache.borrow_mut().insert(midx, is);
        is
    }

    fn method_sig(&self, midx: GIdx) -> Result<MethodSigRc> {
        if let Some(hit) = self.method_sig_cache.borrow().get(&midx) {
            return Ok(hit.clone());
        }
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
        let out = std::rc::Rc::new((class, name, sig));
        self.method_sig_cache.borrow_mut().insert(midx, out.clone());
        Ok(out)
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
                let Ok(cd) = d.class_data_rc(def) else {
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
            // Misses are memoized — a subclass-named call resolved to
            // None before this registration must see the new impl.
            self.host_named_cache.borrow_mut().clear();
        }
        // A DEX may reference the same method through a subclass
        // descriptor; fall back to (name, sig) in call_builtin.
        // Registered even when the exact proto has no dex ref — apps
        // call `addView(View,LayoutParams)` while the bridge is wired
        // for canonical `addView(View)`, and callers below the invoke
        // dispatch consult this map directly.
        let owner = self.type_index_of(class_desc).unwrap_or(GX_NONE);
        self.hosts_named
            .entry((name.to_string(), proto_desc.to_string()))
            .or_default()
            .push((owner, class_desc.to_string(), f));
        Ok(found)
    }

    /// Register host fast paths for hot bundled Kotlin libraries.
    /// These preserve the dex-visible object model while moving
    /// byte-at-a-time JSON scanner loops out of the interpreter.
    pub fn register_library_fastpaths(&mut self) {
        jdk::kser::register(self);
    }

    /// Intern a string value — for host code and probes that need to
    /// hand a `String` to a dex call.
    pub fn intern_str(&mut self, s: &str) -> u32 {
        self.heap.intern_str(s)
    }

    /// Allocate a dex array — for host code and probes passing array
    /// args to dex calls.
    pub fn alloc_array(&mut self, elem: &str, data: Vec<V>) -> V {
        jdk::arr(self, elem, data)
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
        let mut midx =
            self.find_method_by_name(class_desc, name, arg_reg_slots(args), Some(args))?;
        let eff_args;
        if midx.is_none() && args.iter().any(|v| !matches!(v, V::Ref(_))) {
            eff_args = self.box_scalars(args);
            midx = self.find_method_by_name(class_desc, name, eff_args.len(), Some(&eff_args))?;
        } else {
            eff_args = args.to_vec();
        }
        let midx =
            midx.ok_or_else(|| errd("dexrt.method.missing", format!("{class_desc}.{name}")))?;
        // invoke reports a resolved void method as Some(null) — translate
        // back to the documented contract (None = void return).
        let ret_void = self
            .dex_of(midx)
            .and_then(|d| {
                d.method(gl(midx))
                    .ok()
                    .and_then(|m| d.proto(m.proto_idx as u32).ok().map(|p| p.return_type_idx))
                    .and_then(|t| d.type_descriptor(t).ok())
            })
            .is_some_and(|r| r == "V");
        match self.invoke(midx, eff_args, InvokeKind::Static) {
            Ok(v) => Ok(if ret_void { None } else { v }),
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
        self.mark_view_alloc(h, ty);
        // A real guest Application subclass IS the app object —
        // `getApplication[Context]` must hand it back so onCreate state
        // and custom methods are reachable. Overrides the interned
        // placeholder `app_object` may have minted earlier.
        if let Some(app_ty) = self.type_index_of("Landroid/app/Application;") {
            if self.assignable(ty, app_ty) {
                self.app_h = h;
            }
        }
        if let Some(midx) = self.find_method_by_name(class_desc, "<init>", 0, Some(&[]))? {
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
        self.mark_view_alloc(h, ty);
        let slots = arg_reg_slots(args);
        let mut midx = self.find_method_by_name(class_desc, "<init>", slots, Some(args))?;
        let eff_args;
        if midx.is_none() && args.iter().any(|v| !matches!(v, V::Ref(_))) {
            eff_args = self.box_scalars(args);
            midx =
                self.find_method_by_name(class_desc, "<init>", eff_args.len(), Some(&eff_args))?;
        } else {
            eff_args = args.to_vec();
        }
        if let Some(midx) = midx {
            let mut a = Vec::with_capacity(eff_args.len() + 1);
            a.push(V::Ref(h));
            a.extend_from_slice(&eff_args);
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

    /// Mark a freshly allocated object as a View/ViewGroup when its
    /// class is assignable — objects created by dex `new`/`instantiate*`
    /// never pass through `make_view_named`, so without this
    /// `is_viewish` drops their View builtins (`measure`/`layout` become
    /// silent no-ops and the subtree collapses to 0×0).
    fn mark_view_alloc(&mut self, h: u32, ty: GIdx) {
        let Some(view_ty) = self.type_index_of("Landroid/view/View;") else {
            return;
        };
        if !self.assignable(ty, view_ty) {
            return;
        }
        jdk::pseudo_set(self, h, jdk::view::P_IS_VIEW, V::I32(1));
        if let Some(group_ty) = self.type_index_of("Landroid/view/ViewGroup;") {
            if self.assignable(ty, group_ty) {
                jdk::pseudo_set(self, h, jdk::view::P_IS_GROUP, V::I32(1));
            }
        }
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
                let Ok(cd) = d.class_data_rc(def) else {
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
        self.mark_view_alloc(h, ty);
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
        if let Some(hit) = self.super_desc_cache.borrow().get(desc) {
            return hit.clone();
        }
        let out = self.super_desc_uncached(desc);
        self.super_desc_cache
            .borrow_mut()
            .insert(desc.to_string(), out.clone());
        out
    }

    fn super_desc_uncached(&self, desc: &str) -> Option<String> {
        if let Some(ty) = self.type_index_of(desc) {
            if let Some((cg, def)) = self.def_loc_of(ty) {
                if def.superclass_idx == u32::MAX {
                    return None;
                }
                return self.type_desc_of(gx(gd(cg), def.superclass_idx)).ok();
            }
        }
        jdk::host_supers(desc)
            .first()
            .map(|s| (*s).to_string())
            .or_else(|| Self::builtin_supers(desc).first().map(|s| (*s).to_string()))
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

    /// Deliver a host pointer event into the dex view tree
    /// (action: 0=down, 1=up, 2=move). Dex-materialized views —
    /// including games' input views — only see input through this
    /// bridge; the host view tree's own dispatch does not reach them.
    /// Ok(true) when the dex view tree claimed the point — callers
    /// then skip their own (host-widget) dispatch so a view mirrored
    /// in both trees never fires its listener twice.
    pub fn dispatch_touch(&mut self, x: i32, y: i32, action: i32) -> Result<bool> {
        jdk::view::dispatch_touch(self, x, y, action)
    }

    /// Dispatch a hardware-key event into the dex view hierarchy.
    pub fn dispatch_key(
        &mut self,
        action: i32,
        keycode: i32,
        unicode: i32,
        repeat: bool,
    ) -> Result<bool> {
        jdk::view::dispatch_key(self, action, keycode, unicode, repeat)
    }

    /// Invoke `name` on the object at `this` — resolves the method on
    /// its class chain by name and arg count. `Ok(None)` when the
    /// object's class doesn't declare it — the platform base class
    /// (not in this DEX) provides the default.
    pub fn call_virtual(&mut self, this: u32, name: &str, args: &[V]) -> Result<Option<V>> {
        if name == "onAttachedToWindow" {
            jdk::pseudo_set(self, this, jdk::view::P_ATTACHED, V::I32(1));
        } else if name == "onDetachedFromWindow" {
            jdk::pseudo_set(self, this, jdk::view::P_ATTACHED, V::I32(0));
        }
        let Some(ci) = self.heap.class_of(this) else {
            // `Obj::Class` mirrors carry no `class_idx` — as receivers their
            // class is `java.lang.Class` itself. JNI by-name method ids for
            // reflection APIs (`getDeclaredMethods`, `getSimpleName`, …)
            // reach here as CallObjectMethod on a `jclass`; without this the
            // call silently returned null.
            if matches!(self.heap.get(this), Some(Obj::Class { .. })) {
                let mut a = Vec::with_capacity(args.len() + 1);
                a.push(V::Ref(this));
                a.extend_from_slice(args);
                let r =
                    jdk::call(self, "Ljava/lang/Class;", name, "", &a).map_err(|e| match e {
                        E::Vm(v) => v,
                        E::Throw(t) => {
                            let d = self.describe_throw(t);
                            errd("dexrt.exception.uncaught", d)
                        }
                    })?;
                return Ok(r.flatten());
            }
            return Err(errd("dexrt.bad_this", name.to_string()));
        };
        // J/D params occupy two dex registers; arity must match
        // `find_method_by_name`'s slot count or a wide-arg dex override
        // (`Choreographer$FrameCallback.doFrame(J)V`) silently misses
        // and the builtin fallback no-ops.
        let (midx, eff_args) = self.resolve_call_args(ci, name, args)?;
        let Some(midx) = midx else {
            // No dex override — a framework method (`View.layout`,
            // `View.measure`, …). Route it through the builtin
            // dispatcher so host arms still apply when the caller is
            // the host itself rather than a dex invoke site.
            let desc = self.type_desc_of(ci).unwrap_or_default();
            // Views constructed by dex code (e.g. `TabLayout.TabView`)
            // never passed through `make_view_named`, so P_IS_VIEW is
            // unset and `is_viewish` drops every View builtin —
            // `measure`/`layout` become silent no-ops and the whole
            // subtree collapses to 0×0. Mark them structurally the
            // same way inflation does.
            if jdk::pseudo_get(self, this, jdk::view::P_IS_VIEW).is_none()
                && self.assignable_desc(&desc, "Landroid/view/View;")
            {
                jdk::pseudo_set(self, this, jdk::view::P_IS_VIEW, V::I32(1));
                if self.assignable_desc(&desc, "Landroid/view/ViewGroup;") {
                    jdk::pseudo_set(self, this, jdk::view::P_IS_GROUP, V::I32(1));
                }
            }
            let mut a = Vec::with_capacity(args.len() + 1);
            a.push(V::Ref(this));
            a.extend_from_slice(args);
            let r = jdk::call(self, &desc, name, "", &a).map_err(|e| match e {
                E::Vm(v) => v,
                E::Throw(t) => {
                    let d = self.describe_throw(t);
                    errd("dexrt.exception.uncaught", d)
                }
            })?;
            return Ok(r.flatten());
        };
        if crate::dbg_flag("OVELO_VIEWDBG") && (name == "layout" || name == "measure") {
            let d = self.describe_method(midx);
            eprintln!("[viewdbg] dex layout h{this} -> {d}");
        }
        let mut a = Vec::with_capacity(eff_args.len() + 1);
        a.push(V::Ref(this));
        a.extend_from_slice(&eff_args);
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
    pub(crate) fn call_virtual_rt(&mut self, this: u32, name: &str, args: &[V]) -> Rt<Option<V>> {
        let ci = self
            .heap
            .class_of(this)
            .ok_or_else(|| E::Vm(errd("dexrt.bad_this", name.to_string())))?;
        let (midx, eff_args) = self.resolve_call_args(ci, name, args).map_err(E::Vm)?;
        let Some(midx) = midx else {
            if crate::dbg_flag("OVELO_VIEWDBG") && matches!(name, "onMeasure" | "onLayout") {
                let d = self.type_desc_of(ci).unwrap_or_default();
                eprintln!("[viewdbg] onChain miss {name} ci_desc={d}");
            }
            return Ok(None);
        };
        let mut a = Vec::with_capacity(eff_args.len() + 1);
        a.push(V::Ref(this));
        a.extend_from_slice(&eff_args);
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
        self.park_drain_within(50)
    }

    /// `park_drain` with a caller-bounded internal sleep slice — joins
    /// with a millisecond deadline pass their remaining budget so a
    /// `join(1)` can't overshoot by the full 50ms slice.
    pub(crate) fn park_drain_within(&mut self, max_slice_ms: u64) -> std::result::Result<(), E> {
        // Promote delayed tasks that came due — a parked scheduler
        // worker requeued with `due=now` sits in `delayed_runnables`,
        // and an unbounded park (runBlocking on the main thread) must
        // run it rather than unwinding as if nothing were queued.
        let now = jdk::now_ms() as u64;
        let mut i = 0;
        while i < self.delayed_runnables.len() {
            if self.delayed_runnables[i].2 <= now {
                let (r, on_main, _) = self.delayed_runnables.swap_remove(i);
                self.unparkable_delayed.remove(&r);
                self.deadline_woke.insert(r);
                self.pending_runnables.push_back((r, on_main));
            } else {
                i += 1;
            }
        }
        // Nothing queued right now: a delayed task may still complete
        // the wait — a `runBlocking` parked inside a dex callback
        // (Hilt `SwitchingProvider.get` building the theme on the main
        // thread) unwinds fatally if we give up while its completion
        // sits in `delayed_runnables`. Wait out a short slice to the
        // soonest deadline, promote, then only unwind when still dry.
        if self.pending_runnables.is_empty() && self.park_depth < 8 {
            if let Some(due) = self.delayed_runnables.iter().map(|(_, _, t)| *t).min() {
                let now2 = jdk::now_ms() as u64;
                if due > now2 {
                    std::thread::sleep(std::time::Duration::from_millis(
                        (due - now2).min(max_slice_ms),
                    ));
                }
                let now3 = jdk::now_ms() as u64;
                let mut i = 0;
                while i < self.delayed_runnables.len() {
                    if self.delayed_runnables[i].2 <= now3 {
                        let (r, on_main, _) = self.delayed_runnables.swap_remove(i);
                        self.unparkable_delayed.remove(&r);
                        self.deadline_woke.insert(r);
                        self.pending_runnables.push_back((r, on_main));
                    } else {
                        i += 1;
                    }
                }
            }
        }
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
        if std::env::var("OVELO_DRAINDBG").is_ok() {
            eprintln!("[drain] enqueue h{} main={}", r, on_main);
        }
        self.reparked.clear();
        // The newly posted work may feed a worker that parked out of
        // the live queue — re-arm dropped parkers so they rescan.
        self.parked_runnables.remove(&r);
        self.wake_parked();
        self.pending_runnables.push_back((r, on_main));
    }

    /// True while any runnable is queued or waiting on its wake time —
    /// the frame loop drains continuously then (game threads sleep in
    /// `delayed_runnables` between ticks) instead of on the idle cadence.
    pub fn has_queued_runnables(&self) -> bool {
        !self.pending_runnables.is_empty()
            || !self.delayed_runnables.is_empty()
            || !self.frame_callbacks.is_empty()
    }

    /// Registered `Application.ActivityLifecycleCallbacks` handles — the
    /// runner dispatches `onActivity{Pre,Post}*` to them around each
    /// activity lifecycle stage (ReportFragment needs the Post/Pre
    /// variants to advance `ComponentActivity.mLifecycleRegistry`).
    pub fn activity_callbacks(&self) -> Vec<u32> {
        self.activity_callbacks.clone()
    }

    /// Whether this view already received `onAttachedToWindow` — the
    /// flag is set by `call_virtual` dispatch and by addView into an
    /// already-attached parent, so the host's first-materialize attach
    /// queue must skip flagged views or the callback fires twice.
    pub fn view_is_attached(&self, h: u32) -> bool {
        matches!(
            jdk::pseudo_get(self, h, jdk::view::P_ATTACHED),
            Some(V::I32(1))
        )
    }

    /// Dispatch the first window attach over `h`'s whole decor tree —
    /// the host frame loop uses it once the real window exists.
    pub fn view_window_attached(&mut self, h: u32) {
        jdk::view::window_attached(self, h);
    }

    /// `View.removeCallbacks`/`Handler.removeCallbacks`/`Choreographer.remove*` —
    /// drop a queued runnable without running it.
    pub(crate) fn remove_runnable(&mut self, r: u32) {
        self.pending_runnables.retain(|(h, _)| *h != r);
        self.delayed_runnables.retain(|(h, _, _)| *h != r);
        self.unparkable_delayed.remove(&r);
        self.deadline_woke.remove(&r);
        self.deadline_park_cycles.remove(&r);
        self.frame_callbacks.retain(|(h, _)| *h != r);
    }

    /// `enqueue_runnable` with a millisecond delay — the runnable sits
    /// in `delayed_runnables` until `drain_runnables` sees it due.
    pub(crate) fn enqueue_runnable_delayed(&mut self, r: u32, on_main: bool, delay_ms: i64) {
        self.reparked.clear();
        self.parked_runnables.remove(&r);
        self.wake_parked();
        let due = jdk::now_ms().saturating_add(delay_ms.max(0)) as u64;
        self.delayed_runnables.push((r, on_main, due));
    }

    /// Re-arm runnables that parked out of the live queue — called
    /// from `enqueue_runnable` (fresh work may feed a dropped worker's
    /// internal queue) and `LockSupport.unpark`. Delayed sleepers
    /// whose deadline came from `LockSupport.park*` (not `Thread.sleep`)
    /// are also promoted early — a permit cuts the wait short.
    pub(crate) fn wake_parked(&mut self) {
        for (r, on_main) in self.parked_runnables.drain() {
            self.repark_counts.remove(&r);
            self.reparked.remove(&r);
            self.deadline_woke.remove(&r);
            self.deadline_park_cycles.remove(&r);
            self.pending_runnables.push_back((r, on_main));
        }
        let mut i = 0;
        while i < self.delayed_runnables.len() {
            let (r, on_main, _) = self.delayed_runnables[i];
            if self.unparkable_delayed.contains(&r) {
                self.delayed_runnables.swap_remove(i);
                self.unparkable_delayed.remove(&r);
                // An unpark-driven promotion — not a deadline expiry.
                self.deadline_woke.remove(&r);
                self.deadline_park_cycles.remove(&r);
                self.repark_counts.remove(&r);
                self.reparked.remove(&r);
                self.pending_runnables.push_back((r, on_main));
            } else {
                i += 1;
            }
        }
    }

    /// Run enqueued async work FIFO — `Thread.start`/`Executor.execute`
    /// jobs on a worker thread, `Handler.post*` on the main one.
    /// Reentrant-safe; `max` bounds total runnables per drain so a
    /// self-rescheduling queue can't wedge the caller forever. Returns
    /// how many runnables completed — re-parked workers don't count.
    /// Stop-the-world mark-sweep GC. Only safe between dex activations —
    /// live interpreter frames hold their refs in Rust locals, so this
    /// no-ops while dex code is in flight (`depth`/`stack`/`draining`).
    /// `extra_roots` are guest handles held by callers (e.g. the UI's
    /// bound-view tables) that the VM cannot see itself.
    pub fn collect_garbage(&mut self, extra_roots: &mut dyn Iterator<Item = u32>) -> usize {
        if self.depth != 0 || !self.stack.is_empty() || self.draining {
            return 0;
        }
        let mut roots: Vec<u32> = Vec::new();
        for c in self.classes.values() {
            roots.extend(c.statics.values().filter_map(|v| match v {
                V::Ref(h) if *h != 0 => Some(*h),
                _ => None,
            }));
        }
        roots.extend(self.pending_runnables.iter().map(|(h, _)| *h));
        roots.extend(self.single_executors.iter().copied());
        roots.extend(self.executor_active.keys().copied());
        roots.extend(self.executor_active.values().copied());
        roots.extend(
            self.executor_queues
                .values()
                .flat_map(|q| q.iter().copied()),
        );
        roots.extend(self.pending_attach_callbacks.iter().copied());
        roots.extend(self.delayed_runnables.iter().map(|(h, _, _)| *h));
        // Callback registries are the only owners of their objects —
        // ReportFragment's ALC, Choreographer frame callbacks and the
        // cached Choreographer itself must outlive the guest refs that
        // registered them (those die with their registering frame).
        roots.extend(self.activity_callbacks.iter().copied());
        roots.extend(self.frame_callbacks.iter().map(|(h, _)| *h));
        roots.push(self.choreographer_h);
        roots.push(self.byteorder_h.get());
        roots.extend(self.parked_runnables.keys().copied());
        roots.extend(self.thread_runnables.iter().copied());
        roots.extend(self.runnable_thread.values().copied());
        roots.extend(jdk::gl::live_handles());
        roots.extend(self.sleep_until.keys().copied());
        roots.extend(self.reparked.iter().copied());
        roots.extend(self.repark_counts.keys().copied());
        roots.extend(self.park_waiters.iter().copied());
        roots.extend(self.unparkable_delayed.iter().copied());
        roots.extend(self.deadline_woke.iter().copied());
        roots.extend(self.deadline_park_cycles.keys().copied());
        roots.extend(self.vto_listeners.keys().copied());
        roots.extend(self.vto_listeners.values().flatten().copied());
        roots.extend(self.vto_predraw.keys().copied());
        roots.extend(self.vto_predraw.values().flatten().copied());
        roots.extend(self.class_mirrors.values().copied());
        roots.push(self.jni_pending_exc);
        // Runtime-held singletons and registries — Application, thread
        // and looper objects, the current activity, view/db/prefs
        // tables, thrown-object diagnostics.
        roots.extend([
            self.main_thread_h,
            self.cur_thread,
            self.worker_thread_h,
            self.main_looper_h,
            self.main_dispatcher_h,
            self.app_h,
            self.cur_activity,
        ]);
        roots.extend(self.view_roots.iter().copied());
        roots.extend(self.prefs_files.values().copied());
        // Interned enum constants are VM-lifetime singletons (like JVM
        // enum consts) — leaving them unrooted lets the sweep reclaim
        // the handle and `alloc` recycle it for an unrelated object,
        // so a later values()/valueOf/sget lookup returns a wrong
        // object entirely.
        roots.extend(self.enum_cache.values().copied());
        roots.extend(self.enum_name_cache.values().copied());
        roots.extend(self.bool_cache.into_iter().filter(|h| *h != 0));
        roots.extend(self.content_views.keys().copied());
        roots.extend(self.content_views.values().copied());
        roots.extend(self.decor_roots.keys().copied());
        roots.extend(self.decor_roots.values().copied());
        // clock_views is deliberately NOT rooted: a clock is kept alive
        // by its view-tree parent; once detached and unreferenced it is
        // collected and drain_runnables prunes the stale handle.
        roots.extend(self.db_handles.values().copied());
        // Stopped services and coil delivery targets must survive until
        // the drain runs their onDestroy / attaches the drawable —
        // collecting either first silently drops the delivery.
        roots.extend(self.pending_destroy.iter().copied());
        roots.extend(self.pending_jobs.iter().flat_map(|(a, b, _)| [*a, *b]));
        roots.extend(self.coil_jobs.iter().map(|(_, v)| *v));
        // obj_db/throw_sites/unsafe_offsets are NOT rooted: entries whose
        // owner object is dead are pruned after the sweep instead, so the
        // registries can't grow forever or keep stale values alive.
        // unsafe_offsets values stay rooted for entries whose owner is
        // still live (a dead owner's entry is dropped below, releasing
        // its value on the next collect).
        roots.extend(self.unsafe_offsets.values().filter_map(|v| match v {
            V::Ref(h) if *h != 0 => Some(*h),
            _ => None,
        }));
        if let Some((_, V::Ref(h))) = &self.pending_activity {
            if *h != 0 {
                roots.push(*h);
            }
        }
        roots.extend(extra_roots);
        let freed = self.heap.collect(roots);
        // Evict bookkeeping for objects this collect just reclaimed —
        // keeps the maps bounded and stops dead entries pointing at
        // recycled handles.
        self.unsafe_offsets
            .retain(|(h, _), _| self.heap.get(*h).is_some());
        self.obj_db.retain(|h, _| self.heap.get(*h).is_some());
        self.throw_sites.retain(|h, _| self.heap.get(*h).is_some());
        // Next collection when the heap grows another ~50%.
        self.gc_mark = (self.heap.len() * 3 / 2).max(65_536);
        if crate::dbg_flag("OVELO_MEMDBG") {
            eprintln!(
                "[gc] freed {freed} slots={} mark={}",
                self.heap.len(),
                self.gc_mark
            );
        }
        freed
    }

    /// Run up to `max` queued runnables (main + worker queues).
    /// Returns how many completed — re-parked workers don't count.
    pub fn drain_runnables(&mut self, max: usize) -> usize {
        if self.draining {
            return 0;
        }
        native::check_all_zones("drain");
        let now = jdk::now_ms() as u64;
        let due: Vec<_> = self
            .delayed_runnables
            .extract_if(.., |(_, _, t)| *t <= now)
            .collect();
        for (r, on_main, _) in due {
            self.unparkable_delayed.remove(&r);
            self.deadline_woke.insert(r);
            self.pending_runnables.push_back((r, on_main));
        }
        self.draining = true;
        // Host-side async deliveries before guest work: finished coil
        // image fetches become drawables, and services that stopped
        // since the last pass get their onDestroy here — not inline
        // inside their own callback. Runs AFTER the draining flag is
        // set so a nested drain from a delivered callback early-returns
        // instead of running an unbounded inner pass.
        jdk::http::drain_coil(self);
        jdk::android::drain_jobs(self);
        // Retry host calls that lost the borrow race during the last
        // pass — the outer callback has released by now, so the bridge
        // runs its host-side effects (GuestUi wiring etc.). attach_at
        // dedupes the dex side it already did via the builtin fallback.
        self.in_host_retry = true;
        for (midx, args) in std::mem::take(&mut self.pending_host_retry) {
            let host = self
                .hosts
                .get(&midx)
                .cloned()
                .or_else(|| self.host_named(midx).ok().flatten());
            if let Some(f) = host {
                if let Ok(mut g) = f.try_borrow_mut() {
                    let _ = g(self, &args);
                }
            }
        }
        self.in_host_retry = false;
        for (holder, cb, tries) in std::mem::take(&mut self.pending_surface) {
            let (mut sw, mut sh) = surface_holder_size(self, holder);
            if sw <= 0 || sh <= 0 {
                if tries < 240 {
                    // Not laid out yet — retry next tick so the guest
                    // never builds a projection over a 0x0 surface.
                    self.pending_surface.push((holder, cb, tries + 1));
                    continue;
                }
                // View never laid out (hidden SurfaceView etc.) —
                // report the window default rather than 0x0.
                (sw, sh) = (720, 1280);
            }
            let wh = V::Ref(holder);
            if crate::dbg_flag("OVELO_DRAWDBG") {
                eprintln!("[gldbg] surfaceCreated+Changed h{holder} cb{cb} {sw}x{sh}");
            }
            let _ = self.call_virtual(cb, "surfaceCreated", &[wh]);
            let _ = self.call_virtual(
                cb,
                "surfaceChanged",
                &[wh, V::I32(0), V::I32(sw), V::I32(sh)],
            );
            self.surface_sizes.insert(holder, (sw, sh));
        }
        // Redispatch `surfaceChanged` on registered holders whose owning
        // view's size changed (first layout, rotation, ...).
        for (holder, cb) in std::mem::take(&mut self.surface_cbs) {
            let (sw, sh) = surface_holder_size(self, holder);
            if sw > 0 && sh > 0 && self.surface_sizes.get(&holder) != Some(&(sw, sh)) {
                self.surface_sizes.insert(holder, (sw, sh));
                let _ = self.call_virtual(
                    cb,
                    "surfaceChanged",
                    &[V::Ref(holder), V::I32(0), V::I32(sw), V::I32(sh)],
                );
            }
            self.surface_cbs.push((holder, cb));
        }
        for h in std::mem::take(&mut self.pending_destroy) {
            let _ = self.call_virtual(h, "onDestroy", &[]);
        }
        jdk::view::deliver_attach_callbacks(self);
        // Choreographer frame callbacks — `drain_runnables` runs once
        // per frame tick, so a due callback fires on the next pass; one
        // that reposts itself lands in `frame_callbacks` again (the
        // Recomposer's loop works exactly that way).
        let frame_nanos = V::I64(jdk::now_nanos());
        let due_cbs: Vec<_> = self
            .frame_callbacks
            .extract_if(.., |(_, t)| *t <= now)
            .collect();
        let dbg = crate::dbg_flag("OVELO_DRAINDBG");
        if dbg && !due_cbs.is_empty() {
            eprintln!(
                "[drain] framecb due={} pending={}",
                due_cbs.len(),
                self.frame_callbacks.len()
            );
        }
        for (cb, _) in due_cbs {
            let before_steps = self.total_steps;
            let out = self.call_virtual(cb, "doFrame", &[frame_nanos]);
            if dbg {
                eprintln!(
                    "[drain] doFrame h{cb} steps={} out={:?}",
                    self.total_steps - before_steps,
                    out
                );
            }
        }
        let mut ran = 0usize;
        for _ in 0..max {
            // Android normally gives workers their own threads. On this
            // cooperative VM strict FIFO is not fair: Compose re-posts
            // enough main-queue work that a queued IO worker can remain
            // behind it indefinitely. Alternate sides when both are
            // present; fall back to whichever side has work.
            let preferred = self.pending_runnables.iter().position(|(_, on_main)| {
                if self.sched_prefer_worker {
                    !on_main
                } else {
                    *on_main
                }
            });
            let idx = preferred.or_else(|| {
                self.pending_runnables.iter().position(|(_, on_main)| {
                    if self.sched_prefer_worker {
                        *on_main
                    } else {
                        !on_main
                    }
                })
            });
            let Some((r, on_main)) = idx.and_then(|i| self.pending_runnables.remove(i)) else {
                break;
            };
            self.sched_prefer_worker = on_main;
            let out = if on_main {
                jdk::run_on_main(self, r)
            } else {
                jdk::run_on_worker(self, r)
            };
            if dbg {
                let cls = self
                    .heap
                    .class_of(r)
                    .and_then(|ci| self.type_desc_of(ci).ok())
                    .unwrap_or_default();
                match &out {
                    Ok(_) => eprintln!("[drain] ran h={r} {cls} main={on_main}"),
                    Err(e) => {
                        eprintln!("[drain] h={r} {cls} main={on_main} err={:?}", e)
                    }
                }
            }
            match &out {
                Err(e)
                    if e.cause() == "ovelo.park"
                        && self.thread_runnables.contains(&r)
                        && self.sleep_until.contains_key(&r) =>
                {
                    if dbg {
                        eprintln!(
                            "[drain] h={r} park -> delayed(thread) wait={}",
                            self.sleep_until
                                .get(&r)
                                .copied()
                                .unwrap_or(now)
                                .saturating_sub(now)
                        );
                    }
                    // A `Thread` runnable that parked on Thread.sleep
                    // waits out its requested deadline, then re-runs —
                    // no repark budget: a game loop legitimately parks
                    // once per frame forever. A thread parked WITHOUT a
                    // recorded deadline (LockSupport.park*/unpark pairs,
                    // e.g. a kotlinx scheduler worker idling) falls
                    // through to the counted repark path instead —
                    // `unwrap_or(now)` here requeued it instantly and
                    // spun the drain.
                    let due = self.sleep_until.remove(&r).unwrap_or(now);
                    let unparkable = self.park_waiters.remove(&r);
                    if unparkable && self.deadline_woke.remove(&r) {
                        if due.saturating_sub(now) >= 1_000 {
                            // The wake was this runnable's OWN deadline,
                            // not an unpark — a kotlinx surplus worker
                            // past its keep-alive would retire here, but
                            // our re-run-from-top model can't reach its
                            // termination check. After a couple of
                            // deadline-only cycles, retire it out of the
                            // delayed set: `unpark`/fresh
                            // `enqueue_runnable` still revive it via
                            // `parked_runnables`.
                            let n = self.deadline_park_cycles.entry(r).or_insert(0);
                            *n += 1;
                            if *n >= 2 {
                                if dbg {
                                    eprintln!("[drain] h={r} park -> retired(deadline)");
                                }
                                self.deadline_park_cycles.remove(&r);
                                self.reparked.remove(&r);
                                self.parked_runnables.insert(r, on_main);
                                continue;
                            }
                        } else {
                            // A short re-park is an active timer loop,
                            // not an idle keep-alive — don't count it.
                            self.deadline_park_cycles.remove(&r);
                        }
                    }
                    if unparkable {
                        self.unparkable_delayed.insert(r);
                    }
                    self.delayed_runnables.push((r, on_main, due));
                }
                Err(e) if e.cause() == "ovelo.park" && self.sleep_until.contains_key(&r) => {
                    if dbg {
                        eprintln!(
                            "[drain] h={r} park -> delayed(sleep) wait={}",
                            self.sleep_until
                                .get(&r)
                                .copied()
                                .unwrap_or(now)
                                .saturating_sub(now)
                        );
                    }
                    // A non-thread runnable parked with a recorded
                    // deadline (e.g. RealCall.execute polling a host
                    // fetch) waits it out rather than burning reparks.
                    let due = self.sleep_until.remove(&r).unwrap_or(now);
                    let unparkable = self.park_waiters.remove(&r);
                    if unparkable && self.deadline_woke.remove(&r) {
                        if due.saturating_sub(now) >= 1_000 {
                            let n = self.deadline_park_cycles.entry(r).or_insert(0);
                            *n += 1;
                            if *n >= 2 {
                                if dbg {
                                    eprintln!("[drain] h={r} park -> retired(deadline)");
                                }
                                self.deadline_park_cycles.remove(&r);
                                self.reparked.remove(&r);
                                self.parked_runnables.insert(r, on_main);
                                continue;
                            }
                        } else {
                            self.deadline_park_cycles.remove(&r);
                        }
                    }
                    if unparkable {
                        self.unparkable_delayed.insert(r);
                    }
                    self.delayed_runnables.push((r, on_main, due));
                }
                Err(e) if e.cause() == "ovelo.park" => {
                    // A parked worker yields its slot and reschedules —
                    // the runnable re-enters `run()` on a later drain
                    // and re-scans its own work queue. A worker that
                    // can never be woken (nothing feeds its queue)
                    // spins repark→re-run forever, so after enough
                    // consecutive parks stop requeueing it: it is
                    // dropped, not killed — a later `enqueue_runnable`
                    // of the same handle still runs it afresh.
                    let n = self.repark_counts.entry(r).or_insert(0);
                    *n += 1;
                    if *n <= 64 {
                        if dbg {
                            eprintln!("[drain] h={r} park -> requeue #{n}");
                        }
                        self.reparked.insert(r);
                        self.pending_runnables.push_back((r, on_main));
                    } else {
                        if dbg {
                            eprintln!("[drain] h={r} park -> parked out");
                        }
                        self.reparked.remove(&r);
                        // Parked out of the live queue — not gone: a
                        // later `enqueue_runnable`/`unpark` re-arms it
                        // so work posted to its queue still resumes it.
                        self.parked_runnables.insert(r, on_main);
                    }
                }
                _ => {
                    ran += 1;
                    self.reparked.remove(&r);
                    self.repark_counts.remove(&r);
                    self.park_spins = 0;
                    // A `Thread` runnable that returned (or threw) is
                    // done — drop it so GC can reclaim it and its
                    // captured references.
                    self.thread_runnables.remove(&r);
                    self.runnable_thread.remove(&r);
                    self.sleep_until.remove(&r);
                    self.park_waiters.remove(&r);
                    self.unparkable_delayed.remove(&r);
                    self.deadline_woke.remove(&r);
                    self.deadline_park_cycles.remove(&r);
                    self.slept_once.remove(&r);
                    if let Some(ex) = self.runnable_executor.remove(&r) {
                        if self.executor_active.get(&ex) == Some(&r) {
                            let next = self
                                .executor_queues
                                .get_mut(&ex)
                                .and_then(|q| q.pop_front());
                            if let Some(next) = next {
                                self.executor_active.insert(ex, next);
                                self.runnable_executor.insert(next, ex);
                                self.pending_runnables.push_back((next, false));
                            } else {
                                self.executor_active.remove(&ex);
                                self.executor_queues.remove(&ex);
                            }
                        }
                    }
                }
            }
        }
        // TextClock tick: AOSP reschedules the ticker runnable every
        // second; the drain cadence is our clock signal. Handles are
        // re-validated against the class chain — a GC'd slot recycled
        // for a non-clock object must be dropped, not overwritten.
        if !self.clock_views.is_empty() {
            let mut live: Vec<u32> = Vec::with_capacity(self.clock_views.len());
            for i in 0..self.clock_views.len() {
                let h = self.clock_views[i];
                // Same classification the registration site uses —
                // `jdk::class_of` covers host-backed clocks whose
                // class_idx is GX_NONE, which `heap.class_of` misses.
                let is_clock = self.heap.get(h).is_some()
                    && self.assignable_desc(
                        &jdk::class_of(self, V::Ref(h)),
                        "Landroid/widget/TextClock;",
                    );
                if is_clock {
                    live.push(h);
                    jdk::view::textclock_update(self, h);
                }
            }
            self.clock_views = live;
        }
        self.draining = false;
        ran
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

    /// `attach_at` for the guest bridge: registered `addView` overloads
    /// intercept the invoke, so the dex-side bookkeeping (parent, attach
    /// mark, `onViewAdded`) must run from the bridge itself.
    pub fn view_tree_attach_at(
        &mut self,
        parent: u32,
        child: u32,
        idx: Option<usize>,
        lp: Option<V>,
    ) {
        if parent != 0 && child != 0 {
            jdk::view::attach_at(self, parent, V::Ref(child), idx, true, lp);
        }
    }

    /// Index of `child` in the dex-side child list of `parent` — the
    /// authoritative order after the dex bookkeeping ran (used to mirror
    /// host-side placement on retried addView calls).
    pub fn view_child_index(&self, parent: u32, child: u32) -> Option<usize> {
        jdk::view::children(self, parent)
            .iter()
            .position(|&c| jdk::value_eq(self, c, V::Ref(child)))
    }

    /// Parent handle recorded by `attach_at` — the bridge checks this
    /// before re-running dex attach bookkeeping: a host call retried
    /// after the borrow race already attached dex-side via the builtin
    /// fallback, and a second `attach_at` would re-fire `onViewAdded`
    /// and re-append an indexless child (moving it past siblings
    /// added meanwhile).
    pub fn view_parent_of(&self, h: u32) -> Option<u32> {
        match jdk::pseudo_get(self, h, jdk::view::P_PARENT) {
            Some(V::Ref(p)) if p != 0 => Some(p),
            _ => None,
        }
    }

    /// True while the drain loop replays `pending_host_retry` — the
    /// bridge uses it to skip dex-side bookkeeping the builtin
    /// fallback already did for this exact invocation.
    pub fn host_retrying(&self) -> bool {
        self.in_host_retry
    }

    /// `detach` for the guest bridge: registered `removeView`/`detach`
    /// overloads intercept the invoke, so the dex-side bookkeeping must
    /// run from the bridge itself (`notify` mirrors AOSP's
    /// removeView-vs-detachViewFromParent split).
    pub fn view_tree_detach(&mut self, parent: u32, child: u32, notify: bool) {
        if parent != 0 && child != 0 {
            jdk::view::detach(self, parent, V::Ref(child), notify);
        }
    }

    /// The handle `activity` last set as its content view, if any.
    pub fn activity_content(&self, activity: u32) -> Option<u32> {
        jdk::view::activity_content(self, activity)
    }

    /// Push a host-computed frame onto a dex view object — the host
    /// layout engine positions bridged views without running the dex
    /// `layout()` call, so `getWidth()`/`getHeight()` would read 0
    /// (Korge's GL view NaN'd its ortho matrix exactly so).
    pub fn view_set_frame(&mut self, h: u32, l: i32, t: i32, r: i32, b: i32) {
        if h != 0 {
            jdk::view::frame_set(self, h, l, t, r, b);
            jdk::view::clear_layout_requested(self, h);
        }
    }

    /// Whether dex `layout()` owns this view's frame — host layout
    /// size pushes must not stomp it.
    pub fn view_layout_done(&self, h: u32) -> bool {
        jdk::view::layout_done(self, h)
    }

    /// Record the host window size — decor views created lazily take
    /// their initial bounds from it.
    pub fn set_window_size(&mut self, w: i32, h: i32) {
        if w > 0 && h > 0 {
            self.window_size = (w, h);
        }
    }

    /// Give every window decor root the window's bounds — they're
    /// host-created view objects that never pass through layout, so
    /// `getDecorView().getWidth()`/`getHeight()` would read 0 (Korge
    /// sizes its main framebuffer from exactly those calls). Content
    /// frames are left to the guest's own layout — AppCompat places
    /// them below the action bar, and syncing them would stomp that.
    /// The exception is a synthetic (host-allocated) content frame:
    /// it sits at the decor's origin filling the window and no dex
    /// layout will ever re-size it, so it follows the decor.
    pub fn sync_decor_frames(&mut self, w: i32, h: i32) {
        let mut views: Vec<u32> = self.decor_roots.values().copied().collect();
        views.extend(
            self.content_frames
                .values()
                .copied()
                .filter(|cf| !self.is_dex_view(*cf)),
        );
        for v in views {
            self.view_set_frame(v, 0, 0, w, h);
        }
    }

    /// `(width, height)` a dex LayoutParams object requests —
    /// AOSP constants (`-1` match_parent, `-2` wrap_content) pass
    /// through; resource ids resolve to px. Used by the host view
    /// bridge to size bridged children.
    pub fn layout_params_dims(&self, lp: u32) -> (Option<i32>, Option<i32>) {
        jdk::view::lp_dims(self, lp)
    }

    /// The dex parent of a view object, 0 when unattached.
    pub fn dex_parent(&self, h: u32) -> u32 {
        match jdk::pseudo_get(self, h, jdk::view::P_PARENT) {
            Some(V::Ref(p)) => p,
            _ => 0,
        }
    }

    /// Snapshot one node of the dex view tree for host rendering —
    /// text, colors, layout params, visibility, listener, children.
    pub fn dex_view(&self, h: u32) -> Option<jdk::view::DexViewInfo> {
        jdk::view::dex_view_info(self, h)
    }

    /// Run the view's guest `onDraw(Canvas)` over a recording canvas
    /// sized `(w, h)` and return the ops for host replay. Empty when
    /// no class in the chain overrides `onDraw` or the call fails.
    pub fn guest_draw_ops(&mut self, view: u32, w: i32, h: i32) -> Vec<crate::value::CanvasOp> {
        jdk::gfx::guest_draw_ops(self, view, w, h)
    }

    /// Whether the view's Compose measure/layout pass is currently active.
    pub fn compose_measure_active(&mut self, view: u32) -> bool {
        jdk::gfx::compose_measure_active(self, view)
    }

    /// Log the dex LayoutNode tree under `view` (OVELO_NODEDUMP).
    pub fn dump_node_tree(&mut self, view: u32) {
        jdk::gfx::dump_node_tree(self, view)
    }

    /// Whether `view` has a bound `GLSurfaceView.Renderer` — such views
    /// draw every frame (continuous render mode) independent of
    /// view-tree generation changes.
    pub fn has_gl_renderer(&self, view: u32) -> bool {
        jdk::pseudo_get(self, view, jdk::gl::P_GLES_RENDERER).is_some()
    }

    /// Whether any view ever bound a GL renderer.
    pub fn any_gl_renderer(&self) -> bool {
        self.gl_renderer_active
    }

    /// Cheap P_IS_VIEW probe — `dex_view` builds full node info
    /// (attrs, children, strings) and costs far more than callers
    /// that only need to know whether a handle is a view.
    pub fn is_dex_view(&self, h: u32) -> bool {
        h != 0 && jdk::pseudo_get(self, h, jdk::view::P_IS_VIEW).is_some()
    }

    /// Does `h` have a dex-defined override of `name(params)` on its
    /// class chain? Framework containers (RelativeLayout,
    /// FrameLayout, LinearLayout) arrive as skeleton classes with no
    /// methods, so a virtual call misses — builtins use this to pick
    /// between the dex method and a generic framework fallback.
    pub(crate) fn has_dex_virtual(&self, h: u32, name: &str, params: &[&str]) -> bool {
        let Some(ci) = self.heap.class_of(h) else {
            return false;
        };
        if crate::dbg_flag("OVELO_VIEWDBG") {
            let d = self.type_desc_of(ci).unwrap_or_else(|_| "?".into());
            eprintln!(
                "[viewdbg] hdv h{h} ci={ci} desc={d} {name} -> {:?}",
                self.find_method_on_chain_sig(ci, name, params)
            );
        }
        matches!(self.find_method_on_chain_sig(ci, name, params), Ok(Some(_)))
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

    /// `getDrawable(resId)` for host-side shims: a `BitmapDrawable`
    /// carrying the rasterized bitmap when the resource is an image
    /// (vector AXML or PNG/JPEG), else a plain `Drawable` placeholder.
    pub fn drawable_for_res(&mut self, res_id: u32) -> V {
        jdk::vector::drawable_for_res(self, res_id)
    }

    /// Drawable at styleable `index` of a `TypedArray` handle — host
    /// `TintTypedArray` shims resolve through the wrapped array.
    /// `None` when the index resolves to no drawable resource.
    pub fn typed_array_drawable(&mut self, ta_h: u32, index: usize) -> Option<V> {
        match jdk::view::ta_lookup(self, ta_h, index) {
            Some(V::I32(res)) if res > 0 => Some(self.drawable_for_res(res as u32)),
            _ => None,
        }
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
        self.describe_throw_inner(h, &mut hashbrown::HashSet::new())
    }

    fn describe_throw_inner(&self, h: u32, seen: &mut hashbrown::HashSet<u32>) -> String {
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
                                extra.push_str(&format!(" caused by {descriptor}: {detail}"));
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
    fn describe_method(&self, midx: GIdx) -> std::rc::Rc<str> {
        if let Some(hit) = self.method_desc_cache.borrow().get(&midx) {
            return hit.clone();
        }
        let s: std::rc::Rc<str> = self
            .method_sig(midx)
            .map(|ms| format!("{}.{}{}", ms.0, ms.1, ms.2))
            .unwrap_or_else(|_| format!("method@{midx}"))
            .into();
        self.method_desc_cache.borrow_mut().insert(midx, s.clone());
        s
    }

    /// Innermost-first chain of live guest frames for throw-site
    /// diagnostics — `Luv0;.t <- DateTimeFormatter;.parseDateTime <-
    /// Lgt1;.g0`. Consecutive repeats collapse (recursion) and the
    /// string is length-capped so recursive call paths stay readable.
    fn throw_site_desc(&self) -> String {
        let mut out = String::new();
        let mut prev = None;
        for &midx in self.stack.iter().rev() {
            if Some(midx) == prev {
                continue;
            }
            prev = Some(midx);
            if !out.is_empty() {
                out.push_str(" <- ");
            }
            out.push_str(&self.describe_method(midx));
            if out.len() > 700 {
                out.push_str(" <- ...");
                break;
            }
        }
        out
    }

    /// Live call stack as `Lcls;.name(sig)ret` strings, newest frame
    /// last — jdk shims read it to build guest stack traces.
    pub(crate) fn describe_stack(&self) -> Vec<String> {
        self.stack
            .iter()
            .map(|&m| self.describe_method(m).to_string())
            .collect()
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

    /// Each scalar in `args` boxed into its `Number` mirror
    /// (`HostState::Slot`); refs pass through unchanged. A raw
    /// `V::I64` out of a JSON map fed to `Map.merge` targets
    /// `apply(Object,Object)` — dex expects a boxed `Long`, not a
    /// 2-register primitive image.
    fn box_scalars(&mut self, args: &[V]) -> Vec<V> {
        args.iter()
            .map(|v| {
                let desc = match v {
                    V::I64(_) => "Ljava/lang/Long;",
                    V::I32(_) => "Ljava/lang/Integer;",
                    V::F64(_) => "Ljava/lang/Double;",
                    V::F32(_) => "Ljava/lang/Float;",
                    _ => return *v,
                };
                jdk::host(self, desc, HostState::Slot(*v))
            })
            .collect()
    }

    /// Resolve `name` on `ci`'s chain for `args`, retrying with each
    /// scalar boxed into its `Number` mirror (`HostState::Slot`) when
    /// the plain image misses. A raw `V::I64` out of a JSON map fed
    /// to `Map.merge` targets `apply(Object,Object)` — an unboxed
    /// 2-register read misses that method entirely and the caller
    /// then deletes the entry as "absent". Returns the matching
    /// method plus the arg image to invoke with (boxed when used).
    pub(crate) fn resolve_call_args(
        &mut self,
        ci: GIdx,
        name: &str,
        args: &[V],
    ) -> Result<(Option<GIdx>, Vec<V>)> {
        let nslots = arg_reg_slots(args);
        if let Some(m) = self.find_method_on_chain(ci, name, nslots, Some(args))? {
            return Ok((Some(m), args.to_vec()));
        }
        if args.iter().any(|v| !matches!(v, V::Ref(_))) {
            let boxed = self.box_scalars(args);
            if let Some(m) = self.find_method_on_chain(ci, name, boxed.len(), Some(&boxed))? {
                return Ok((Some(m), boxed));
            }
        }
        Ok((None, args.to_vec()))
    }

    /// `find_method_by_name` over the class's superclass chain — an
    /// inherited callback (e.g. onCreate on a DEX parent) still runs.
    fn find_method_on_chain(
        &self,
        mut class_idx: GIdx,
        name: &str,
        nslots: usize,
        args: Option<&[V]>,
    ) -> Result<Option<GIdx>> {
        // The arg profile selects among same-slot-count overloads
        // (`doFrame(J)` vs `doFrame(int,int)` — both 2 registers), so
        // the cache key carries it, not just `nslots`.
        let pat = args
            .map(|av| {
                av.iter()
                    .map(|v| match v {
                        V::I64(_) => 'J',
                        V::F64(_) => 'D',
                        _ => '.',
                    })
                    .collect::<String>()
            })
            .unwrap_or_else(|| nslots.to_string());
        let key = (class_idx, name.to_string(), pat);
        if let Some(hit) = self.named_chain_cache.borrow().get(&key) {
            return Ok(*hit);
        }
        let mut out = None;
        for _ in 0..64 {
            let Some((cg, def)) = self.def_loc_of(class_idx) else {
                break;
            };
            let sup = def.superclass_idx;
            let desc = self.type_desc_of(cg)?;
            if let Some(m) = self.find_method_by_name(&desc, name, nslots, args)? {
                out = Some(m);
                break;
            }
            if sup == u32::MAX {
                break;
            }
            class_idx = gx(gd(cg), sup);
        }
        self.named_chain_cache.borrow_mut().insert(key, out);
        Ok(out)
    }

    /// First method `name` on `class_desc` whose params occupy exactly
    /// `nslots` registers (J/D params take two) — args pack as a register
    /// image, so a long/double param counts double. When `args` is given
    /// the wide-arg positions must also line up (`doFrame(J)` beats
    /// `doFrame(int,int)` for an `I64` call — same 2 slots, wrong
    /// method otherwise).
    fn find_method_by_name(
        &self,
        class_desc: &str,
        name: &str,
        nslots: usize,
        args: Option<&[V]>,
    ) -> Result<Option<GIdx>> {
        let Some(&(di, def_i)) = self.defs.get(class_desc) else {
            return Ok(None);
        };
        let d = &self.dexes[di];
        let def = &d.class_defs()[def_i];
        let cd = d.class_data_rc(def)?;
        for m in cd.direct_methods.iter().chain(cd.virtual_methods.iter()) {
            let mid = d.method(m.method_idx)?;
            if d.string(mid.name_idx)? != name {
                continue;
            }
            let p = d.proto(mid.proto_idx as u32)?;
            let mut slots = 0usize;
            let mut ptypes = Vec::new();
            for t in d.proto_params(p)? {
                let ty = d.type_descriptor(t as u32)?;
                slots += if ty.starts_with('J') || ty.starts_with('D') {
                    2
                } else {
                    1
                };
                if args.is_some() {
                    ptypes.push(ty);
                }
            }
            if slots != nslots {
                continue;
            }
            if let Some(av) = args {
                let fits = ptypes.len() == av.len()
                    && ptypes.iter().zip(av.iter()).all(|(ty, a)| match a {
                        V::I64(_) => ty == "J",
                        V::F64(_) => ty == "D",
                        _ => !(ty.starts_with('J') || ty.starts_with('D')),
                    });
                if !fits {
                    continue;
                }
            }
            return Ok(Some(gx(di, m.method_idx)));
        }
        Ok(None)
    }

    /// Whether `class_desc` declares a method named `name` at all —
    /// the existence probe `Class.getDeclaredMethod` needs (it throws
    /// NoSuchMethodException on a miss, unlike a name+arity resolve).
    pub(crate) fn has_method_named(&self, class_desc: &str, name: &str) -> bool {
        let Some(&(di, def_i)) = self.defs.get(class_desc) else {
            return false;
        };
        let d = &self.dexes[di];
        let Some(def) = d.class_defs().get(def_i) else {
            return false;
        };
        let Ok(cd) = d.class_data_rc(def) else {
            return false;
        };
        cd.direct_methods
            .iter()
            .chain(cd.virtual_methods.iter())
            .any(|m| {
                d.method(m.method_idx)
                    .ok()
                    .and_then(|mid| d.string(mid.name_idx).ok())
                    .map(|n| n == name)
                    .unwrap_or(false)
            })
    }

    /// Whether `desc` names a class defined by a loaded dex — host
    /// classes (`Ljava/lang/String;`, `Landroid/view/View;`, …) have
    /// no def, so signature-based lookups can't apply to them.
    pub(crate) fn is_dex_class(&self, class_desc: &str) -> bool {
        self.defs.contains_key(class_desc)
    }

    /// `<init>` on `class_desc` whose declared parameter descriptors
    /// equal `params` exactly — what `Class.getConstructor(sig)`
    /// resolves by signature rather than slot count.
    pub(crate) fn find_ctor_sig(
        &self,
        class_desc: &str,
        params: &[String],
    ) -> Result<Option<GIdx>> {
        let Some(&(di, def_i)) = self.defs.get(class_desc) else {
            return Ok(None);
        };
        let d = &self.dexes[di];
        let def = &d.class_defs()[def_i];
        let cd = d.class_data_rc(def)?;
        for m in cd.direct_methods.iter().chain(cd.virtual_methods.iter()) {
            let mid = d.method(m.method_idx)?;
            if d.string(mid.name_idx)? != "<init>" {
                continue;
            }
            let p = d.proto(mid.proto_idx as u32)?;
            let got: Vec<String> = d
                .proto_params(p)?
                .iter()
                .filter_map(|t| d.type_descriptor(*t as u32).ok())
                .collect();
            if got == params {
                return Ok(Some(gx(di, m.method_idx)));
            }
        }
        Ok(None)
    }

    /// Method `name` on `class_desc` whose declared params equal
    /// `params` exactly — overloads sharing a slot count (like
    /// `generateLayoutParams(AttributeSet)` vs `(LayoutParams)`)
    /// need the signature, not just arity.
    pub(crate) fn find_method_by_sig(
        &self,
        class_desc: &str,
        name: &str,
        params: &[&str],
    ) -> Result<Option<GIdx>> {
        let Some(&(di, def_i)) = self.defs.get(class_desc) else {
            return Ok(None);
        };
        let d = &self.dexes[di];
        let def = &d.class_defs()[def_i];
        let cd = d.class_data_rc(def)?;
        for m in cd.direct_methods.iter().chain(cd.virtual_methods.iter()) {
            let mid = d.method(m.method_idx)?;
            if d.string(mid.name_idx)? != name {
                continue;
            }
            let p = d.proto(mid.proto_idx as u32)?;
            let got: Vec<String> = d
                .proto_params(p)?
                .iter()
                .filter_map(|t| d.type_descriptor(*t as u32).ok())
                .collect();
            if got.iter().map(String::as_str).eq(params.iter().copied()) {
                return Ok(Some(gx(di, m.method_idx)));
            }
        }
        Ok(None)
    }

    /// Walk `class_idx`'s superclass chain for a signature-exact
    /// `name(params)` — like [`find_method_on_chain`] but resolved by
    /// signature rather than slot count.
    pub(crate) fn find_method_on_chain_sig(
        &self,
        mut class_idx: GIdx,
        name: &str,
        params: &[&str],
    ) -> Result<Option<GIdx>> {
        for _ in 0..64 {
            let Some((cg, def)) = self.def_loc_of(class_idx) else {
                break;
            };
            let sup = def.superclass_idx;
            let desc = self.type_desc_of(cg)?;
            if let Some(m) = self.find_method_by_sig(&desc, name, params)? {
                return Ok(Some(m));
            }
            if sup == u32::MAX {
                return Ok(None);
            }
            class_idx = gx(gd(cg), sup);
        }
        Ok(None)
    }

    /// [`call_virtual`] resolved by exact parameter descriptors —
    /// host callers that know the overload signature (framework
    /// contract methods like `generateLayoutParams(AttributeSet)`).
    /// Falls back to the builtin dispatcher when no dex override exists.
    pub fn call_virtual_sig(
        &mut self,
        this: u32,
        name: &str,
        params: &[&str],
        args: &[V],
    ) -> Result<Option<V>> {
        let ci = self
            .heap
            .class_of(this)
            .ok_or_else(|| errd("dexrt.bad_this", name.to_string()))?;
        let Some(midx) = self.find_method_on_chain_sig(ci, name, params)? else {
            let desc = self.type_desc_of(ci).unwrap_or_default();
            let mut a = Vec::with_capacity(args.len() + 1);
            a.push(V::Ref(this));
            a.extend_from_slice(args);
            let r = jdk::call(self, &desc, name, "", &a).map_err(|e| match e {
                E::Vm(v) => v,
                E::Throw(t) => {
                    let d = self.describe_throw(t);
                    errd("dexrt.exception.uncaught", d)
                }
            })?;
            return Ok(r.flatten());
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

    /// Declared parameter descriptors of the method `midx` names.
    pub(crate) fn method_params(&self, midx: GIdx) -> Vec<String> {
        let Some(d) = self.dex_of(midx) else {
            return Vec::new();
        };
        let Ok(mid) = d.method(gl(midx)) else {
            return Vec::new();
        };
        let Ok(p) = d.proto(mid.proto_idx as u32) else {
            return Vec::new();
        };
        d.proto_params(p)
            .map(|list| {
                list.iter()
                    .filter_map(|t| d.type_descriptor(*t as u32).ok())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn def_of(&self, class_idx: GIdx) -> Option<&ClassDef> {
        self.def_loc_of(class_idx).map(|(_, d)| d)
    }

    fn class_data_of(&self, class_idx: GIdx) -> Result<Option<std::rc::Rc<ClassData>>> {
        match self.def_loc_of(class_idx) {
            Some((cg, d)) => Ok(Some(
                self.dex_of(cg)
                    .ok_or_else(|| err("dexrt.dex.idx"))?
                    .class_data_rc(d)?,
            )),
            None => Ok(None),
        }
    }

    /// `sub` assignable to `sup` through the superclass chain, with
    /// interfaces walked transitively (a class may implement a
    /// sub-interface of `sup` or reach `sup` through another interface).
    pub(crate) fn assignable(&self, sub: GIdx, sup: GIdx) -> bool {
        if let Some(hit) = self.assignable_cache.borrow().get(&(sub, sup)) {
            return *hit;
        }
        let out = self.assignable_uncached(sub, sup);
        self.assignable_cache.borrow_mut().insert((sub, sup), out);
        out
    }

    fn assignable_uncached(&self, sub: GIdx, sup: GIdx) -> bool {
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
                        if d == sup_desc || jdk::host_supers(&d).iter().any(|s| *s == sup_desc) {
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
        jdk::host_supers(d)
            .first()
            .copied()
            .or_else(|| Self::builtin_supers(d).first().copied())
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
            | "Ljava/util/NoSuchElementException;"
            | "Ljava/util/ConcurrentModificationException;"
            | "Ljava/util/EmptyStackException;"
            | "Ljava/util/MissingResourceException;"
            | "Ljava/util/InputMismatchException;"
            | "Ljava/util/IllegalFormatException;"
            | "Ljava/util/MissingFormatArgumentException;"
            | "Ljava/util/CompletionException;"
            | "Ljava/lang/IllegalStateException;"
            | "Ljava/lang/IllegalArgumentException;"
            | "Ljava/lang/IllegalMonitorStateException;"
            | "Ljava/lang/UnsupportedOperationException;"
            | "Ljava/lang/NumberFormatException;"
            | "Ljava/lang/IllegalCallerException;"
            | "Ljava/lang/SecurityException;"
            | "Ljava/lang/StringIndexOutOfBoundsException;"
            | "Ljava/lang/TypeNotPresentException;"
            | "Ljava/lang/EnumConstantNotPresentException;"
            | "Ljava/lang/reflect/UndeclaredThrowableException;"
            | "Ljava/lang/reflect/MalformedParameterizedTypeException;"
            | "Ljava/lang/annotation/AnnotationTypeMismatchException;"
            | "Ljava/lang/invoke/WrongMethodTypeException;"
            | "Ljava/nio/BufferOverflowException;"
            | "Ljava/nio/BufferUnderflowException;"
            | "Ljava/nio/ReadOnlyBufferException;"
            | "Ljava/nio/file/ProviderMismatchException;"
            | "Ljava/security/ProviderException;"
            | "Ljava/time/DateTimeException;"
            | "Ljava/io/UncheckedIOException;"
            | "Ljava/util/regex/PatternSyntaxException;"
            | "Ljava/util/concurrent/RejectedExecutionException;"
            | "Ljava/util/concurrent/CompletionException;"
            | "Ljava/util/concurrent/CancellationException;"
            | "Ljava/lang/reflect/MalformedParametersException;" => &[
                "Ljava/lang/RuntimeException;",
                "Ljava/lang/Exception;",
                "Ljava/lang/Throwable;",
                "Ljava/lang/Object;",
            ],
            "Ljava/lang/IllegalThreadStateException;" => &[
                "Ljava/lang/IllegalArgumentException;",
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
            // java.io exception tree — `catch (IOException)` sites (Glide's
            // header parse, stream wrappers) must see the subtype.
            "Ljava/nio/file/FileSystemException;" => &[
                "Ljava/io/IOException;",
                "Ljava/lang/Exception;",
                "Ljava/lang/Throwable;",
                "Ljava/lang/Object;",
            ],
            "Ljava/nio/file/FileAlreadyExistsException;"
            | "Ljava/nio/file/NoSuchFileException;"
            | "Ljava/nio/file/DirectoryNotEmptyException;" => &[
                "Ljava/nio/file/FileSystemException;",
                "Ljava/io/IOException;",
                "Ljava/lang/Exception;",
                "Ljava/lang/Throwable;",
                "Ljava/lang/Object;",
            ],
            "Ljava/io/IOException;" => &[
                "Ljava/lang/Exception;",
                "Ljava/lang/Throwable;",
                "Ljava/lang/Object;",
            ],
            "Ljava/io/EOFException;"
            | "Ljava/io/FileNotFoundException;"
            | "Ljava/io/InterruptedIOException;"
            | "Ljava/io/MalformedURLException;"
            | "Ljava/io/ObjectStreamException;"
            | "Ljava/io/OptionalDataException;"
            | "Ljava/io/ProtocolException;"
            | "Ljava/io/SyncFailedException;"
            | "Ljava/io/UnknownServiceException;"
            | "Ljava/io/UnsupportedEncodingException;"
            | "Ljava/io/UTFDataFormatException;"
            | "Ljava/io/WriteAbortedException;"
            | "Ljava/util/zip/ZipException;"
            | "Ljava/net/UnknownHostException;"
            | "Ljava/net/HttpRetryException;"
            | "Ljava/net/SocketException;"
            | "Ljava/net/ConnectException;"
            | "Ljava/net/BindException;"
            | "Ljava/net/NoRouteToHostException;"
            | "Ljava/net/PortUnreachableException;"
            | "Ljava/net/SocketTimeoutException;"
            | "Ljava/net/ProtocolException;"
            | "Ljava/net/MalformedURLException;"
            | "Ljavax/net/ssl/SSLException;"
            | "Ljavax/net/ssl/SSLHandshakeException;"
            | "Ljavax/net/ssl/SSLPeerUnverifiedException;" => &[
                "Ljava/io/IOException;",
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
            // Checked exceptions that map straight to `Exception`
            // (skipping `RuntimeException` — `catch (RuntimeException)`
            // must not catch these).
            "Ljava/security/GeneralSecurityException;"
            | "Ljava/security/NoSuchAlgorithmException;"
            | "Ljava/security/NoSuchProviderException;"
            | "Ljava/security/InvalidKeyException;"
            | "Ljava/security/SignatureException;"
            | "Ljava/security/KeyException;"
            | "Ljava/security/cert/CertificateException;"
            | "Ljavax/crypto/BadPaddingException;"
            | "Ljavax/crypto/IllegalBlockSizeException;"
            | "Ljavax/crypto/NoSuchPaddingException;"
            | "Ljavax/crypto/ShortBufferException;"
            | "Ljava/text/ParseException;"
            | "Ljava/lang/instrument/IllegalClassFormatException;"
            | "Ljava/lang/InterruptedException;"
            | "Ljava/lang/CloneNotSupportedException;"
            | "Ljava/lang/ReflectiveOperationException;"
            | "Ljava/lang/ClassNotFoundException;"
            | "Ljava/lang/InstantiationException;"
            | "Ljava/lang/IllegalAccessException;"
            | "Ljava/lang/NoSuchMethodException;"
            | "Ljava/lang/NoSuchFieldException;"
            | "Ljava/lang/reflect/InvocationTargetException;"
            | "Ljava/util/concurrent/ExecutionException;"
            | "Ljava/util/concurrent/TimeoutException;"
            | "Ljava/util/zip/DataFormatException;"
            | "Ljava/net/URISyntaxException;"
            | "Landroid/os/RemoteException;" => &[
                "Ljava/lang/Exception;",
                "Ljava/lang/Throwable;",
                "Ljava/lang/Object;",
            ],
            "Ljava/lang/Exception;" | "Ljava/lang/Error;" => {
                &["Ljava/lang/Throwable;", "Ljava/lang/Object;"]
            }
            "Ljava/lang/Throwable;" => &["Ljava/lang/Object;"],
            // Unknown throwable descriptors absent from the dex: a
            // framework `*Error` is an Error subclass, `*Exception` an
            // Exception subclass (not RuntimeException — catching it
            // must not claim unchecked-throwable semantics).
            _ if d.ends_with("Error;") => &[
                "Ljava/lang/Error;",
                "Ljava/lang/Throwable;",
                "Ljava/lang/Object;",
            ],
            _ if d.ends_with("Exception;") => &[
                "Ljava/lang/Exception;",
                "Ljava/lang/Throwable;",
                "Ljava/lang/Object;",
            ],
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
            // Writers that accumulate characters in memory — writes
            // append to the buffer and toString() yields it back.
            "Ljava/io/StringWriter;" | "Ljava/io/CharArrayWriter;" => {
                HostState::StrBuf(String::new())
            }
            "Ljava/util/ArrayList;"
            | "Ljava/util/LinkedList;"
            | "Ljava/util/Vector;"
            | "Ljava/util/Stack;"
            | "Ljava/util/HashSet;"
            | "Ljava/util/EnumSet;"
            | "Ljava/util/LinkedHashSet;"
            | "Ljava/util/TreeSet;"
            | "Ljava/util/ArrayDeque;"
            | "Ljava/util/BitSet;"
            | "Ljava/util/PriorityQueue;"
            | "Ljava/util/AbstractCollection;"
            | "Ljava/util/AbstractList;"
            | "Ljava/util/AbstractSet;"
            | "Ljava/util/AbstractQueue;"
            | "Ljava/util/AbstractSequentialList;"
            | "Ljava/util/Arrays$ArrayList;"
            | "Ljava/util/Collections$EmptyList;"
            | "Ljava/util/Collections$EmptySet;"
            | "Ljava/util/Collections$SingletonList;"
            | "Ljava/util/Collections$SingletonSet;"
            | "Ljava/util/concurrent/CopyOnWriteArrayList;"
            | "Ljava/util/concurrent/CopyOnWriteArraySet;"
            | "Ljava/util/concurrent/ConcurrentLinkedQueue;"
            | "Ljava/util/concurrent/ConcurrentLinkedDeque;"
            | "Ljava/util/concurrent/LinkedBlockingQueue;"
            | "Ljava/util/concurrent/LinkedBlockingDeque;"
            | "Ljava/util/concurrent/ArrayBlockingQueue;"
            | "Ljava/util/concurrent/PriorityBlockingQueue;"
            | "Landroid/util/ArraySet;" => HostState::List(Vec::new()),
            "Ljava/util/HashMap;"
            | "Ljava/util/LinkedHashMap;"
            | "Ljava/util/Hashtable;"
            | "Ljava/util/TreeMap;"
            | "Ljava/util/WeakHashMap;"
            | "Ljava/util/IdentityHashMap;"
            | "Ljava/util/EnumMap;"
            | "Ljava/util/Properties;"
            | "Ljava/util/AbstractMap;"
            | "Ljava/util/Collections$EmptyMap;"
            | "Ljava/util/Collections$SingletonMap;"
            | "Ljava/util/concurrent/ConcurrentHashMap;"
            | "Ljava/util/concurrent/ConcurrentSkipListMap;"
            | "Landroid/util/ArrayMap;"
            | "Landroid/util/SparseArray;"
            | "Landroid/util/SparseIntArray;"
            | "Landroid/util/SparseBooleanArray;"
            | "Landroid/util/SparseLongArray;"
            | "Landroid/util/LongSparseArray;"
            | "Landroid/util/LruCache;"
            | "Landroid/os/Bundle;"
            | "Landroid/os/BaseBundle;"
            | "Landroid/os/PersistableBundle;"
            | "Landroid/app/job/JobInfo;"
            | "Landroid/app/job/JobInfo$Builder;"
            | "Landroid/app/job/JobParameters;"
            | "Landroid/content/Intent;" => HostState::Map(Vec::new()),
            "Ljava/util/Calendar;"
            | "Ljava/util/GregorianCalendar;"
            | "Ljava/util/Date;"
            | "Ljava/time/Instant;"
            | "Ljava/sql/Date;"
            | "Ljava/sql/Timestamp;"
            | "Ljava/sql/Time;" => HostState::Millis(now_ms),
            "Ljava/util/Random;"
            | "Ljava/security/SecureRandom;"
            | "Ljava/util/concurrent/ThreadLocalRandom;"
            | "Ljava/util/SplittableRandom;" => {
                HostState::Slot(V::I64(0x9E3779B97F4A7C15_u64 as i64))
            }
            "Ljava/lang/Integer;"
            | "Ljava/lang/Long;"
            | "Ljava/lang/Short;"
            | "Ljava/lang/Byte;"
            | "Ljava/lang/Float;"
            | "Ljava/lang/Double;"
            | "Ljava/lang/Boolean;"
            | "Ljava/lang/Character;"
            | "Ljava/lang/Void;"
            | "Ljava/lang/Thread;"
            | "Ljava/lang/ThreadLocal;"
            | "Ljava/lang/InheritableThreadLocal;"
            | "Ljava/lang/ref/WeakReference;"
            | "Ljava/lang/ref/SoftReference;"
            | "Ljava/lang/ref/PhantomReference;"
            | "Ljava/util/Optional;"
            | "Ljava/util/OptionalInt;"
            | "Ljava/util/OptionalLong;"
            | "Ljava/util/OptionalDouble;"
            | "Landroid/util/Pair;"
            | "Landroid/util/MutableInt;"
            | "Ljava/util/concurrent/atomic/AtomicReference;"
            | "Ljava/util/concurrent/atomic/AtomicInteger;"
            | "Ljava/util/concurrent/atomic/AtomicLong;"
            | "Ljava/util/concurrent/atomic/AtomicBoolean;"
            | "Ljava/util/concurrent/atomic/AtomicMarkableReference;"
            | "Ljava/util/concurrent/atomic/AtomicStampedReference;"
            | "Ljava/util/concurrent/atomic/AtomicReferenceFieldUpdater;"
            | "Ljava/util/concurrent/atomic/AtomicIntegerFieldUpdater;"
            | "Ljava/util/concurrent/atomic/AtomicLongFieldUpdater;"
            | "Ljava/util/concurrent/CountDownLatch;"
            | "Ljava/util/concurrent/Semaphore;"
            | "Landroid/os/Message;" => HostState::Slot(V::I32(0)),
            "Ljava/io/File;" => HostState::Path(String::new()),
            "Landroid/graphics/Path;" => HostState::GraphicsPath(Vec::new()),
            "Lorg/json/JSONObject;" => HostState::Map(Vec::new()),
            "Lorg/json/JSONArray;" => HostState::List(Vec::new()),
            "Ljava/util/regex/Pattern;"
            | "Ljava/util/regex/Matcher;"
            | "Ljava/text/SimpleDateFormat;"
            | "Ljava/text/DateFormat;"
            | "Ljava/text/DecimalFormat;"
            | "Ljava/text/NumberFormat;"
            | "Ljava/text/MessageFormat;"
            | "Ljava/time/format/DateTimeFormatter;" => HostState::Regex(String::new()),
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
        let fty = d.type_descriptor(fd.type_idx as u32).unwrap_or_default();
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
    pub(crate) fn assignable_desc(&self, sub: &str, sup: &str) -> bool {
        if sub == sup || sup == "Ljava/lang/Object;" {
            return true;
        }
        if let Some(hit) = self
            .assignable_desc_cache
            .borrow()
            .get(sub)
            .and_then(|m| m.get(sup))
        {
            return *hit;
        }
        let out = self.assignable_desc_uncached(sub, sup);
        self.assignable_desc_cache
            .borrow_mut()
            .entry(sub.to_string())
            .or_default()
            .insert(sup.to_string(), out);
        out
    }

    fn assignable_desc_uncached(&self, sub: &str, sup: &str) -> bool {
        if Self::builtin_supers(sub).contains(&sup) || jdk::host_supers(sub).contains(&sup) {
            return true;
        }
        // Prefer the dex-aware walk when both types are referenced: it
        // traverses interfaces as well as superclasses. The descriptor-only
        // superclass fallback below cannot see `implements` edges (Ktor's
        // ByteChannel -> ByteReadChannel being one), so reflection-driven
        // type checks otherwise reject valid implementations.
        if let (Some(sub_g), Some(sup_g)) = (self.type_index_of(sub), self.type_index_of(sup)) {
            return self.assignable(sub_g, sup_g);
        }
        // `super_desc` resolves dex class_defs first, then host_supers —
        // a dex ViewGroup subclass (custom RecyclerView etc.) walks
        // through androidx up to the framework class.
        let mut d = sub.to_string();
        for _ in 0..32 {
            let Some(next) = self.super_desc(&d) else {
                return false;
            };
            if next == sup {
                return true;
            }
            if next == d {
                return false;
            }
            d = next;
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
            // An array element is itself a reference type, so `T[][]`
            // casts to `Object[]`/`Cloneable[]`/`Serializable[]`.
            (Some(_), None) => {
                matches!(
                    sup,
                    "Ljava/lang/Object;" | "Ljava/lang/Cloneable;" | "Ljava/io/Serializable;"
                )
            }
            (None, Some(_)) => false,
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
                if let (Some(a), Some(b)) = (self.type_index_of(sub), self.type_index_of(sup)) {
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
                // Framework exception classes absent from the dex can't
                // be traced at all; their names still imply the family.
                Self::builtin_supers(descriptor).contains(&sup.as_str())
                    || self.assignable_desc(descriptor, &sup)
                    || (sup == "Ljava/lang/Exception;" && descriptor.ends_with("Exception;"))
                    || (sup == "Ljava/lang/Throwable;" && descriptor.starts_with('L'))
            }
            Some(Obj::Str(_)) => {
                sup == "Ljava/lang/String;"
                    || sup == "Ljava/lang/Object;"
                    || sup == "Ljava/lang/CharSequence;"
                    || sup == "Ljava/lang/Comparable;"
                    || sup == "Ljava/io/Serializable;"
            }
            Some(Obj::Class { .. }) => {
                // java.lang.Class implements Type, GenericDeclaration,
                // AnnotatedElement, Serializable.
                sup == "Ljava/lang/Class;"
                    || sup == "Ljava/lang/Object;"
                    || sup == "Ljava/lang/reflect/Type;"
                    || sup == "Ljava/lang/reflect/GenericDeclaration;"
                    || sup == "Ljava/lang/reflect/AnnotatedElement;"
                    || sup == "Ljava/io/Serializable;"
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
        if !self.stack.is_empty() {
            let site = self.throw_site_desc();
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
            if crate::dbg_flag("OVELO_CLINITDBG") {
                eprintln!(
                    "[clinit] {} FAILED: {}",
                    this.type_desc_of(class_idx)
                        .unwrap_or_else(|_| format!("class#{class_idx}")),
                    match &e {
                        E::Throw(h) => format!("guest throwable {}", this.describe_throw(*h)),
                        E::Vm(v) => format!("{v}"),
                    }
                );
            }
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
        if let Ok(ms) = self.method_sig(midx) {
            let (_, name, sig) = &*ms;
            // Static/direct refs may name a subclass of the actual
            // declarer — ART resolves them walking the superclass
            // chain (`V3/g.b` declared on `V3/i` etc.), so do the
            // same on a direct-table miss.
            let mut cur = Some(ms.0.clone());
            for _ in 0..64 {
                let Some(desc) = cur else { break };
                if let Some(&g) =
                    self.methods_by_sig
                        .get(&(desc.clone(), name.clone(), sig.clone()))
                {
                    if let Some(&(c, em)) = self.defined.get(&g) {
                        return Ok(Loc {
                            class_idx: c,
                            midx: g,
                            em,
                        });
                    }
                }
                let Some(&(di, def_i)) = self.defs.get(&desc) else {
                    break;
                };
                let d = &self.dexes[di];
                let def = &d.class_defs()[def_i];
                if def.superclass_idx == u32::MAX {
                    break;
                }
                cur = d
                    .type_descriptor(def.superclass_idx)
                    .ok()
                    .map(|s| s.to_string());
            }
        }
        Err(E::Vm(err("dexrt.invoke.unresolved")))
    }

    /// Virtual/interface dispatch: find the override in `start_class`'s
    /// virtual table matching the target's (name, proto). Name and proto
    /// indexes are dex-local — compare descriptors, never raw indexes.
    fn resolve_virtual(&self, midx: GIdx, start_class: GIdx, fallback: bool) -> Rt<Loc> {
        let key = (midx, start_class, fallback);
        if let Some(hit) = self.resolve_cache.borrow().get(&key) {
            return hit.ok_or_else(|| E::Vm(err("dexrt.invoke.unresolved")));
        }
        let out = self
            .resolve_virtual_uncached(midx, start_class, fallback)
            .ok();
        self.resolve_cache.borrow_mut().insert(key, out);
        out.ok_or_else(|| E::Vm(err("dexrt.invoke.unresolved")))
    }

    fn resolve_virtual_uncached(&self, midx: GIdx, start_class: GIdx, fallback: bool) -> Rt<Loc> {
        let Ok(ms) = self.method_sig(midx) else {
            return Err(E::Vm(err("dexrt.invoke.unresolved")));
        };
        let (_, name, sig) = &*ms;
        let mut cur = Some(start_class);
        for _ in 0..64 {
            let Some(ci) = cur else { break };
            let Some((cg, def)) = self.def_loc_of(ci) else {
                break;
            };
            let Some(d) = self.dex_of(cg) else { break };
            if let Ok(cd) = d.class_data_rc(def) {
                for m in &cd.virtual_methods {
                    let Ok(om) = d.method(m.method_idx) else {
                        continue;
                    };
                    let oname = d.string(om.name_idx).unwrap_or_default();
                    if oname.as_str() == name.as_str()
                        && proto_sig(d, om.proto_idx as u32).unwrap_or_default() == sig.as_str()
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
        // Interface default methods: D8 desugars `Modifier.foldIn`-style
        // defaults onto the interface def with a real body, and a class
        // implements the interface without declaring the method — the
        // superclass-only walk above then misses it and the call stubs.
        if let Some(hit) = self.find_interface_method(start_class, name, sig) {
            return Ok(hit);
        }
        // Fall back to the declared method itself (e.g. dispatch target is the
        // declared class — a class that doesn't override).
        if fallback {
            self.resolve_defined(midx)
        } else {
            Err(E::Vm(err("dexrt.invoke.unresolved")))
        }
    }

    /// First interface method carrying a code body reachable from `start`
    /// through the whole class/interface hierarchy — where JVM dispatch
    /// lands after the superclass chain misses (default methods).
    fn find_interface_method(&self, start: GIdx, name: &str, sig: &str) -> Option<Loc> {
        let mut seen = hashbrown::HashSet::new();
        let mut stack = vec![start];
        while let Some(ci) = stack.pop() {
            if !seen.insert(ci) {
                continue;
            }
            let Some((cg, def)) = self.def_loc_of(ci) else {
                continue;
            };
            let Some(d) = self.dex_of(cg) else {
                continue;
            };
            if let Ok(types) = d.type_list(def.interfaces_off) {
                for &t in &types {
                    stack.push(gx(gd(cg), t as u32));
                }
            }
            if def.superclass_idx != u32::MAX {
                stack.push(gx(gd(cg), def.superclass_idx));
            }
            let Ok(cd) = d.class_data_rc(def) else {
                continue;
            };
            for m in &cd.virtual_methods {
                let Ok(om) = d.method(m.method_idx) else {
                    continue;
                };
                if d.string(om.name_idx).unwrap_or_default() == name
                    && proto_sig(d, om.proto_idx as u32).unwrap_or_default() == sig
                    && m.code_off != 0
                {
                    return Some(Loc {
                        class_idx: cg,
                        midx: gx(gd(cg), m.method_idx),
                        em: *m,
                    });
                }
            }
        }
        None
    }

    /// Builtin method table: (class descriptor, name) — never silent success,
    /// only listed signatures are served. Registered host methods win.
    /// Host stub lookup by `(name, proto)` — the DEX may reference a
    /// framework method through a subclass descriptor the exact-midx
    /// registration doesn't cover (e.g. `LSubAct;.setContentView`).
    fn host_named(&self, midx: GIdx) -> Rt<Option<HostCall>> {
        if let Some(hit) = self.host_named_cache.borrow().get(&midx) {
            return Ok(hit.clone());
        }
        let out = self.host_named_uncached(midx)?;
        self.host_named_cache.borrow_mut().insert(midx, out.clone());
        Ok(out)
    }

    fn host_named_uncached(&self, midx: GIdx) -> Rt<Option<HostCall>> {
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
        for (owner, desc, f) in self.hosts_named.get(&key).into_iter().flatten() {
            if *owner != GX_NONE && (*owner == owner_ref || self.assignable(owner_ref, *owner)) {
                return Ok(Some(f.clone()));
            }
            // Descriptor fallback covers every GX_NONE mix (platform
            // classes have no dex type on one or both sides).
            // `<init>` is non-virtual: only the exact declared class
            // may match — `new EditText` reaching the inherited `View`
            // ctor bridge builds the wrong GuestView kind (the dex
            // EditText's setText then hits a type error).
            if let Ok(site) = self.type_desc_of(owner_ref) {
                if if name == "<init>" {
                    site == *desc
                } else {
                    self.assignable_desc(&site, desc)
                } {
                    return Ok(Some(f.clone()));
                }
            }
        }
        Ok(None)
    }

    /// The `(name,sig)`-registered host impl for a reference class —
    /// the same rule `host_named` applies (owner or subclass), for
    /// builtin arms that run below the normal invoke dispatch and
    /// would otherwise swallow a bridged call (`ViewGroup.addView`).
    pub(crate) fn named_host(&self, class: &str, name: &str, sig: &str) -> Option<HostCall> {
        let owner_ref = self.type_index_of(class).unwrap_or(GX_NONE);
        if crate::dbg_flag("OVELO_VIEWDBG") {
            let key = (name.to_string(), sig.to_string());
            let es: Vec<String> = self
                .hosts_named
                .get(&key)
                .into_iter()
                .flatten()
                .map(|(o, d, _)| format!("o{o}={d}"))
                .collect();
            eprintln!(
                "[viewdbg] named_host {class}.{name}{sig} owner_ref={owner_ref} entries={es:?}"
            );
        }
        for (owner, desc, f) in self
            .hosts_named
            .get(&(name.to_string(), sig.to_string()))
            .into_iter()
            .flatten()
        {
            if *owner != GX_NONE
                && owner_ref != GX_NONE
                && (*owner == owner_ref || self.assignable(owner_ref, *owner))
            {
                return Some(f.clone());
            }
            // Descriptor ancestry covers platform classes on either
            // side — `ViewGroup.addView` invoked via a `ViewGroup`-typed
            // ref must not miss the bridge (fossify documents white
            // frame was exactly this: 4-arg addView host-miss).
            // `<init>` is non-virtual: exact declared class only.
            if if name == "<init>" {
                class == desc
            } else {
                self.assignable_desc(class, desc)
            } {
                return Some(f.clone());
            }
        }
        None
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
        if crate::dbg_flag("OVELO_VIEWDBG") && self.describe_method(midx).contains("setText") {
            eprintln!(
                "[viewdbg] call_builtin {} args={args:?}",
                self.describe_method(midx)
            );
        }
        let mut args = self.compact_args(midx, args, has_receiver);
        // `put(boolean)` into a JSON container must arrive boxed: a
        // bare I32 reads back as 0/1 and canonicalizes to null on a
        // later `aput-object` into an Object[] (org.json's real put
        // autoboxes to java.lang.Boolean).
        let z_params: Vec<usize> = self
            .dex_of(midx)
            .and_then(|d| {
                let mid = d.method(gl(midx)).ok()?;
                let class = d.type_descriptor(mid.class_idx as u32).ok()?;
                let name = d.string(mid.name_idx).ok()?;
                if !matches!(
                    class.as_str(),
                    "Lorg/json/JSONObject;" | "Lorg/json/JSONArray;"
                ) || name != "put"
                {
                    return None;
                }
                let p = d.proto(mid.proto_idx as u32).ok()?;
                let ps = d.proto_params(p).ok()?;
                let mut out = vec![];
                for (i, ty) in ps.iter().enumerate() {
                    if d.type_descriptor(*ty as u32)
                        .map(|s| s == "Z")
                        .unwrap_or(false)
                    {
                        out.push(i);
                    }
                }
                Some(out)
            })
            .unwrap_or_default();
        for i in z_params {
            let ai = i + has_receiver as usize;
            if let Some(V::I32(b)) = args.get(ai).copied() {
                args[ai] = jdk::boxed_boolean(self, b != 0);
            }
        }
        let args = args.as_slice();
        let host = self.hosts.get(&midx).cloned().or(self.host_named(midx)?);
        if let Some(f) = host {
            match f.try_borrow_mut() {
                Ok(mut g) => {
                    let r = g(self, args).map_err(E::Vm);
                    if let Some(h) = self.host_throw.take() {
                        return Err(E::Throw(h));
                    }
                    // A registered host ctor bypasses view::call's Context
                    // capture — record the ctx arg here too so `getContext()`
                    // resolves to the real Activity for dex-instantiated views
                    // (Compose's AndroidComposeView ctor hits this path).
                    if r.is_ok() {
                        let is_init = self
                            .dex_of(midx)
                            .and_then(|d| {
                                let mid = d.method(gl(midx)).ok()?;
                                d.string(mid.name_idx).ok()
                            })
                            .map(|n| n == "<init>")
                            .unwrap_or(false);
                        if is_init {
                            if let (Some(V::Ref(this)), Some(V::Ref(c))) =
                                (args.first(), args.get(1))
                            {
                                if *c != 0
                                    && self.assignable_desc(
                                        &jdk::class_of(self, V::Ref(*this)),
                                        "Landroid/view/View;",
                                    )
                                {
                                    jdk::pseudo_set(self, *this, jdk::view::P_CONTEXT, V::Ref(*c));
                                }
                            }
                        }
                    }
                    return r;
                }
                // Guest code re-entering the same registered callback
                // (attach → onAttachedToWindow → setContent → addView)
                // must not abort: fall through to the builtin stubs,
                // which still do the dex-side bookkeeping (attach_at),
                // and queue the bridge for the next drain tick so its
                // host-side effects (GuestUi wiring) still happen.
                Err(_) => {
                    self.pending_host_retry.push((midx, args.to_vec()));
                }
            }
        }
        let Some(d) = self.dex_of(midx) else {
            return vm("dexrt.invoke.unresolved");
        };
        let mid = *d.method(gl(midx)).map_err(E::Vm)?;
        let class = d.type_descriptor(mid.class_idx as u32).map_err(E::Vm)?;
        let name = d.string(mid.name_idx).map_err(E::Vm)?;
        match (class.as_str(), name.as_str()) {
            // ServiceLoader cannot discover kotlinx-coroutines-android's
            // factory here; the dex getter otherwise degrades to a typed
            // placeholder. Materialize the bundled HandlerContext instead.
            ("Lkotlinx/coroutines/Dispatchers;", "getMain") => Ok(Some(jdk::main_dispatcher(self))),
            ("Ljava/lang/Object;", "<init>") => Ok(None),
            ("Ljava/lang/Object;", "getClass") => {
                let this = args.first().copied().unwrap_or(V::Ref(0));
                let ci = match this {
                    V::Ref(h) if h != 0 => match self.heap.class_of(h) {
                        Some(c) if c != GX_NONE => c,
                        // Str/Class/Array/Throwable have no stored
                        // class_idx — derive the descriptor instead so
                        // e.g. "x".getClass() mirrors String, not the
                        // class that happens to sit at type index 0.
                        _ => self
                            .type_index_of(&jdk::class_of(self, V::Ref(h)))
                            .unwrap_or(GX_NONE),
                    },
                    _ => GX_NONE,
                };
                Ok(Some(V::Ref(self.class_mirror(ci))))
            }
            ("Ljava/lang/String;", "length") => {
                if let V::Ref(h) = args.first().copied().unwrap_or(V::Ref(0)) {
                    if let Some(s) = self.heap.str_val(h).map(|x| x.to_string()).or_else(|| {
                        jdk::pseudo_get(self, h, jdk::P_DATA).and_then(|v| jdk::sv(self, v))
                    }) {
                        return Ok(Some(V::I32(s.chars().count() as i32)));
                    }
                }
                Err(self.throw_builtin("Ljava/lang/NullPointerException;", "String.length"))
            }
            // `new String(x)` on a non-dex class: capture the source
            // text into P_DATA so the Instance behaves like the
            // interned Obj::Str everywhere s()/str_val read strings.
            ("Ljava/lang/String;", "<init>") => {
                if let V::Ref(this) = args.first().copied().unwrap_or(V::Ref(0)) {
                    if this != 0 {
                        // `new String(byte[], charset)` — decode the
                        // byte array; `new String(x)` mirrors the text.
                        let t = match args.get(1).copied() {
                            Some(V::Ref(ah)) => match self.heap.get(ah) {
                                Some(Obj::Array { elem, data }) => {
                                    // `String(x[], off, len, ...)` — the
                                    // (off,len) pair bounds the decode;
                                    // ignoring it leaks segment padding
                                    // (okio reads share 8192-buf tails).
                                    let (lo, hi) = if args.len() >= 4 {
                                        let o = jdk::i64v(args[2]).max(0) as usize;
                                        let n = jdk::i64v(args[3]).max(0) as usize;
                                        (o.min(data.len()), (o + n).min(data.len()))
                                    } else {
                                        (0, data.len())
                                    };
                                    if elem == "C" {
                                        let units: Vec<u16> = data[lo..hi]
                                            .iter()
                                            .map(|v| jdk::i64v(*v) as u16)
                                            .collect();
                                        Some(String::from_utf16_lossy(&units))
                                    } else {
                                        let bytes: Vec<u8> = data[lo..hi]
                                            .iter()
                                            .map(|v| jdk::i64v(*v) as u8)
                                            .collect();
                                        Some(String::from_utf8_lossy(&bytes).into_owned())
                                    }
                                }
                                _ => jdk::sv(self, V::Ref(ah)),
                            },
                            _ => None,
                        }
                        .unwrap_or_default();
                        let r = self.heap.intern_str(&t);
                        jdk::pseudo_set(self, this, jdk::P_DATA, V::Ref(r));
                    }
                }
                Ok(None)
            }
            _ => {
                let ret_desc;
                {
                    let p = d.proto(mid.proto_idx as u32).map_err(E::Vm)?;
                    ret_desc = d.type_descriptor(p.return_type_idx).map_err(E::Vm)?;
                }
                let sig = self.proto_sig_of(midx)?;
                // JDK/framework builtins cover real behavior for the
                // common java.* / android.* surface.
                if let Some(r) = jdk::call(self, &class, &name, &sig, args)? {
                    return Ok(r);
                }
                // Interface dispatch names the interface ref
                // (Appendable.append) while the implementation is
                // registered under the receiver's concrete class
                // (StringBuilder.append). Retry `jdk::call` against the
                // receiver's runtime descriptor before stubbing.
                if has_receiver {
                    if let Some(&V::Ref(h)) = args.first() {
                        let rc = match self.heap.get(h) {
                            Some(Obj::Host { desc, .. }) => Some(desc.clone()),
                            Some(Obj::Instance { class_idx, .. }) => {
                                self.type_desc_of(*class_idx).ok()
                            }
                            Some(Obj::Str(_)) => Some("Ljava/lang/String;".to_string()),
                            _ => None,
                        };
                        if let Some(rc) = rc.filter(|c| *c != class) {
                            if let Some(r) = jdk::call(self, &rc, &name, &sig, args)? {
                                return Ok(r);
                            }
                        }
                    }
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
                } else if let Some(v) = self.annotation_method_value(midx, args) {
                    // Bodiless annotation-interface call
                    // (`subscribe.threadMode()`) answered from the
                    // mirror's stored elements / declared default.
                    Ok(Some(v))
                } else {
                    vm("dexrt.invoke.unresolved").map_err(|e| match e {
                        E::Vm(v) => E::Vm(v.with_detail(self.describe_method(midx).to_string())),
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
        if crate::dbg_flag("OVELO_STUBDBG") {
            eprintln!("[stub] {trace} -> {ret} args={}", args.len());
        }
        if self.stub_trace.len() >= 32 {
            self.stub_trace.remove(0);
        }
        self.stub_trace.push(trace.to_string());
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
            // CharSequence-typed returns must carry real text — a typed
            // placeholder stringifies as "java/lang/CharSequence@xxxx"
            // and leaks into TextViews.
            "Ljava/lang/CharSequence;" => Ok(Some(V::Ref(self.heap.intern_str("")))),
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
    /// `call_virtual` that bypasses registered host overrides on the
    /// ROOT method only — compat shims delegate to the real dex body
    /// through this once the genuine path is known to work (nested
    /// invokes inside the dex body still honor host overrides).
    pub fn call_virtual_direct(&mut self, this: u32, name: &str, args: &[V]) -> Result<Option<V>> {
        let ci = self
            .heap
            .class_of(this)
            .ok_or_else(|| errd("dexrt.bad_this", name.to_string()))?;
        let (midx, eff_args) = self.resolve_call_args(ci, name, args)?;
        let Some(midx) = midx else {
            let desc = self.type_desc_of(ci).unwrap_or_default();
            let mut a = Vec::with_capacity(eff_args.len() + 1);
            a.push(V::Ref(this));
            a.extend_from_slice(&eff_args);
            let r = jdk::call(self, &desc, name, "", &a).map_err(|e| match e {
                E::Vm(v) => v,
                E::Throw(t) => {
                    let d = self.describe_throw(t);
                    errd("dexrt.exception.uncaught", d)
                }
            })?;
            return Ok(r.flatten());
        };
        let mut a = Vec::with_capacity(eff_args.len() + 1);
        a.push(V::Ref(this));
        a.extend_from_slice(&eff_args);
        let out = match self.invoke_inner(midx, a, InvokeKind::Virtual, true) {
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

    /// Unboxed scalar (I64/F64/I32/F32) used as an Object — JSON values
    /// reach `((Number)x).doubleValue()` unboxed. Answer the boxing
    /// surface by coercion; anything else stays unhandled so the
    // caller falls back to the null-receiver NPE.
    fn scalar_virtual(&mut self, midx: GIdx, v: V, args: &[V]) -> Rt<Option<V>> {
        let d = match self.dex_of(midx) {
            Some(d) => d,
            None => return Ok(None),
        };
        let m = d.method(gl(midx)).map_err(E::Vm)?;
        let name = d.string(m.name_idx).map_err(E::Vm)?;
        // A zero scalar in a reference slot is `null`, not the number
        // zero — `null.toString()` must NPE, not yield "0". But when the
        // resolved method belongs to a numeric box (unboxed JSON numbers
        // reach `Number.intValue()`), zero is a legitimate value.
        if Self::scalar_is_zero(v) {
            let owner = d.type_descriptor(m.class_idx as u32).unwrap_or_default();
            if !matches!(
                owner.as_str(),
                "Ljava/lang/Number;"
                    | "Ljava/lang/Integer;"
                    | "Ljava/lang/Long;"
                    | "Ljava/lang/Short;"
                    | "Ljava/lang/Byte;"
                    | "Ljava/lang/Double;"
                    | "Ljava/lang/Float;"
            ) {
                return Ok(None);
            }
        }
        let as_f64 = match v {
            V::I32(i) => i as f64,
            V::I64(i) => i as f64,
            V::F32(f) => f as f64,
            V::F64(f) => f,
            _ => return Ok(None),
        };
        let as_i64 = as_f64 as i64;
        Ok(Some(match name.as_str() {
            "intValue" | "shortValue" | "byteValue" | "charValue" => V::I32(as_i64 as i32),
            "longValue" => V::I64(as_i64),
            "floatValue" => V::F32(as_f64 as f32),
            "doubleValue" => V::F64(as_f64),
            "booleanValue" => V::I32((as_i64 != 0) as i32),
            "toString" => {
                let t = match v {
                    V::I64(i) => i.to_string(),
                    V::I32(i) => i.to_string(),
                    _ => {
                        if as_f64.fract() == 0.0 {
                            format!("{}", as_f64 as i64)
                        } else {
                            as_f64.to_string()
                        }
                    }
                };
                jdk::jstr(self, &t)
            }
            "hashCode" => V::I32(as_i64 as i32 ^ (as_i64 >> 32) as i32),
            "equals" => {
                let eq = match args.get(1).copied() {
                    Some(o) => match (v, o) {
                        (V::I32(a), V::I32(b)) => a == b,
                        (V::I64(a), V::I64(b)) => a == b,
                        (V::F32(a), V::F32(b)) => a == b,
                        (V::F64(a), V::F64(b)) => a == b,
                        _ => false,
                    },
                    _ => false,
                };
                V::I32(eq as i32)
            }
            _ => return Ok(None),
        }))
    }

    /// Zero-valued scalar in a reference slot — `const/4 v, 0x0` is
    /// how dex writes `null`, so 0 is the null literal there.
    fn scalar_is_zero(v: V) -> bool {
        match v {
            V::I32(0) | V::I64(0) => true,
            V::F32(f) => f == 0.0,
            V::F64(f) => f == 0.0,
            _ => false,
        }
    }

    /// `instanceof`/check-cast target test for an unboxed scalar —
    /// numbers satisfy Number/Object and the matching box class.
    fn scalar_is_instance(&mut self, v: V, ty: GIdx) -> bool {
        let want = self.type_desc_of(ty).unwrap_or_default();
        let num =
            matches!(v, V::I32(_) | V::I64(_) | V::F32(_) | V::F64(_)) && !Self::scalar_is_zero(v);
        let int_kind = matches!(v, V::I32(_) | V::I64(_));
        let hit =
            num && matches!(
                want.as_str(),
                "Ljava/lang/Object;"
                    | "Ljava/lang/Number;"
                    | "Ljava/lang/Integer;"
                    | "Ljava/lang/Long;"
                    | "Ljava/lang/Short;"
                    | "Ljava/lang/Byte;"
                    | "Ljava/lang/Double;"
                    | "Ljava/lang/Float;"
                    | "Ljava/io/Serializable;"
                    | "Ljava/lang/Comparable;"
            ) && (want != "Ljava/lang/Double;" && want != "Ljava/lang/Float;" || !int_kind);
        if crate::dbg_flag("OVELO_TRACE_INSTOF") {
            eprintln!("[instof] scalar {:?} vs {} -> {}", v, want, hit);
        }
        hit
    }

    fn invoke(&mut self, midx: GIdx, args: Vec<V>, kind: InvokeKind) -> Rt<Option<V>> {
        self.invoke_inner(midx, args, kind, false)
    }

    /// Re-enter the dex implementation beneath a host fast path when
    /// the fast path detects its uncommon slow case.
    pub(crate) fn invoke_no_host(
        &mut self,
        midx: GIdx,
        args: Vec<V>,
        kind: InvokeKind,
    ) -> Rt<Option<V>> {
        self.invoke_inner(midx, args, kind, true)
    }

    fn invoke_inner(
        &mut self,
        midx: GIdx,
        args: Vec<V>,
        kind: InvokeKind,
        skip_host: bool,
    ) -> Rt<Option<V>> {
        // Serialize with JNI entry points: guest `.so`s spawn real
        // pthreads whose env calls must not alias the interpreter's
        // heap access (recursive mutex — nested invoke/JNI re-enters
        // fine, call_native drops all levels while guest code runs).
        let _vmlock = jni::vm_lock();
        if matches!(kind, InvokeKind::Static) && self.is_main_getter(midx) {
            return Ok(Some(jdk::main_dispatcher(self)));
        }
        if crate::dbg_flag("OVELO_HOTCALL") {
            thread_local! {
                static HOT: std::cell::RefCell<HashMap<GIdx, u64>> =
                    std::cell::RefCell::new(HashMap::new());
                static LAST: std::cell::Cell<std::time::Instant> =
                    std::cell::Cell::new(std::time::Instant::now());
            }
            let dump = LAST.with(|l| l.get().elapsed().as_secs() >= 10);
            HOT.with(|h| {
                *h.borrow_mut().entry(midx).or_default() += 1;
                if dump {
                    let mut top: Vec<(GIdx, u64)> =
                        h.borrow().iter().map(|(g, c)| (*g, *c)).collect();
                    top.sort_by_key(|(_, c)| std::cmp::Reverse(*c));
                    for (g, c) in top.into_iter().take(12) {
                        eprintln!("[hotcall] {c:>9}x {}", self.describe_method(g));
                    }
                    eprintln!("[hotcall] ----");
                    h.borrow_mut().clear();
                }
            });
            if dump {
                LAST.with(|l| l.set(std::time::Instant::now()));
            }
        }
        if crate::dbg_flag("OVELO_VIEWDBG") {
            if let Some(d) = self.dex_of(midx) {
                if let Ok(m) = d.method(gl(midx)) {
                    if let Ok(n) = d.string(m.name_idx) {
                        if matches!(
                            n.as_str(),
                            "measure"
                                | "measureChild"
                                | "measureChildWithMargins"
                                | "onMeasureChild"
                        ) {
                            eprintln!(
                                "[viewdbg] invoke {} recv={:?}",
                                self.describe_method(midx),
                                args.first()
                            );
                        }
                    }
                }
            }
        }
        let loc = match kind {
            InvokeKind::Static | InvokeKind::Direct => match self.resolve_defined(midx) {
                Ok(l) => l,
                Err(_) => {
                    return self.call_builtin(midx, &args, matches!(kind, InvokeKind::Direct));
                }
            },
            InvokeKind::Virtual | InvokeKind::Interface => {
                let recv = match args.first() {
                    Some(&V::Ref(h)) if h != 0 => h,
                    Some(v) => {
                        // JSON/scalar values arrive unboxed (I64/F64):
                        // `Number.*Value`/`Object` methods on them
                        // coerce the scalar instead of faulting.
                        if let Some(r) = self.scalar_virtual(midx, *v, &args)? {
                            return Ok(Some(r));
                        }
                        return Err(self.throw_builtin(
                            "Ljava/lang/NullPointerException;",
                            &format!(
                                "null receiver calling {} arg={:?}",
                                self.describe_method(midx),
                                v
                            ),
                        ));
                    }
                    None => {
                        return Err(self.throw_builtin(
                            "Ljava/lang/NullPointerException;",
                            &format!("null receiver calling {}", self.describe_method(midx)),
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
                    Err(_) => {
                        // invoke-super on an interface default method:
                        // the caller implements the interface, but its
                        // superclass chain (Object) doesn't — JVM picks
                        // the *named* interface's body. The ref's owner
                        // is that interface, so resolve the method on the
                        // ref itself first (guarded against the caller's
                        // own def, which would recurse); only fall back
                        // to scanning the caller's direct interfaces when
                        // the owner has no body.
                        let loc = self
                            .resolve_defined(midx)
                            .ok()
                            .filter(|l| l.class_idx != caller_class && l.em.code_off != 0)
                            .or_else(|| {
                                self.def_loc_of(caller_class).and_then(|(cg, d)| {
                                    let dex = self.dex_of(cg)?;
                                    let types = dex.type_list(d.interfaces_off).ok()?;
                                    let ms = self.method_sig(midx).ok()?;
                                    types.iter().find_map(|&t| {
                                        self.find_interface_method(
                                            gx(gd(cg), t as u32),
                                            &ms.1,
                                            &ms.2,
                                        )
                                    })
                                })
                            });
                        match loc {
                            Some(l) => l,
                            None => return self.call_builtin(midx, &args, true),
                        }
                    }
                }
            }
        };
        // A registered host override short-circuits even a dex-defined
        // method — used for compat shims on library code that can't run
        // here (e.g. appcompat's vector-drawable path).
        let host = if skip_host {
            None
        } else {
            self.hosts.get(&loc.midx).cloned()
        };
        if let Some(f) = host {
            // A re-entrant call into an already-borrowed host callback
            // falls through to dex/builtin execution — the outer call
            // dispatching guest code (attach → onAttachedToWindow →
            // addView) is a legitimate AOSP flow, not an error.
            match f.try_borrow_mut() {
                Ok(mut g) => {
                    let has_recv = !matches!(kind, InvokeKind::Static);
                    let args = self.compact_args(loc.midx, &args, has_recv);
                    let r = g(self, &args).map_err(E::Vm);
                    if let Some(h) = self.host_throw.take() {
                        return Err(E::Throw(h));
                    }
                    return r;
                }
                Err(_) => {
                    // Queue the bridge for the next drain tick so its
                    // host-side wiring still runs once the borrow frees.
                    let has_recv = !matches!(kind, InvokeKind::Static);
                    let args = self.compact_args(loc.midx, &args, has_recv);
                    self.pending_host_retry.push((loc.midx, args));
                }
            }
        }
        if !matches!(kind, InvokeKind::Static) {
            if let Some(f) = self.simple_getter_field(loc) {
                let h = match args.first() {
                    Some(&V::Ref(h)) => h,
                    _ => 0,
                };
                if h == 0 {
                    return Err(self.throw_builtin(
                        "Ljava/lang/NullPointerException;",
                        &format!("null receiver calling {}", self.describe_method(midx)),
                    ));
                }
                let stored = match self.heap.get(h) {
                    Some(Obj::Instance { fields, .. }) | Some(Obj::Host { fields, .. }) => {
                        fields.get(&f).copied()
                    }
                    other => {
                        return Err(E::Vm(errd(
                            "dexrt.type",
                            format!(
                                "fast iget h{h} field={} kind={:?}",
                                self.describe_field(f),
                                other.map(std::mem::discriminant),
                            ),
                        )));
                    }
                };
                let v = match stored {
                    Some(v) => v,
                    None => match self.builtin_ifield(h, f) {
                        Some(b) => b,
                        None => match self.ref_field_stub(h, f) {
                            Some(v) => v,
                            None => self.field_default(f).unwrap_or(V::I32(0)),
                        },
                    },
                };
                return Ok(Some(v));
            }
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
                    self.proto_sig_of(loc.midx)?,
                    d.proto(mid.proto_idx as u32)
                        .and_then(|p| d.type_descriptor(p.return_type_idx))
                        .map_err(E::Vm)?,
                )
            };
            let args = self.compact_args(loc.midx, &args, !matches!(kind, InvokeKind::Static));
            if let Some(r) = jdk::call(self, &class, &name, &sig, &args)? {
                return Ok(r);
            }
            // Annotation-interface calls (`Navigator.Name.value()`,
            // `Subscribe.threadMode()`) are bodiless decls too — answer
            // from the receiver mirror's elements (or declared
            // AnnotationDefault) before falling to the stub path.
            if let Some(v) = self.annotation_method_value(loc.midx, &args) {
                return Ok(Some(v));
            }
            // Debug bisection: OVELO_STUB_NATIVE=<substring> skips the
            // native call entirely, returning a zero value for its
            // return type — used to isolate which guest `.so` entry
            // point's execution corrupts state.
            static STUB_PAT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
            let stub = STUB_PAT
                .get_or_init(|| std::env::var("OVELO_STUB_NATIVE").ok())
                .as_deref();
            if stub.is_some_and(|p| name.contains(p)) {
                eprintln!("[stubnative] {class}.{name}{sig} skipped");
                return Ok(void_resolved(match ret_desc.as_str() {
                    "V" => None,
                    "Z" | "B" | "C" | "S" | "I" => Some(V::I32(0)),
                    "J" => Some(V::I64(0)),
                    "F" => Some(V::F32(0.0)),
                    "D" => Some(V::F64(0.0)),
                    _ => Some(V::Ref(0)),
                }));
            }
            // A dex `native` method resolves to a loaded `.so`: the
            // `Java_<cls>_<name>` symbol or a RegisterNatives binding.
            if let Some(addr) = jni::native_addr(self, &class, &name, &sig) {
                return jni::dispatch_native(
                    self,
                    addr,
                    &class,
                    &ret_desc,
                    matches!(kind, InvokeKind::Static),
                    &args,
                )
                .map(void_resolved);
            }
            return self
                .stub_placeholder(loc.midx, &ret_desc, &name, &class, &args)
                .map(void_resolved);
        }
        if let Some(f) = self.aot.as_ref().and_then(|a| a.get(loc.midx)) {
            return self
                .run_native(f, loc.midx, loc.em.code_off, args)
                .map(void_resolved);
        }
        self.exec(loc, args).map(void_resolved)
    }

    /// JNI-facing wrappers — the `jni` module sits outside `Vm` but
    /// needs the same interpreter surface the opcodes use.
    pub(crate) fn invoke_pub(
        &mut self,
        midx: GIdx,
        args: Vec<V>,
        kind: InvokeKind,
    ) -> Rt<Option<V>> {
        self.invoke(midx, args, kind)
    }

    /// Invoke a STATIC method resolved by declaring-class desc + name
    /// — `Method.invoke` with a null receiver lands here (e.g.
    /// `Handler.createAsync` via HandlerCompat).
    /// `Method.invoke` — dispatch the exact method the mirror stored in
    /// `P_METH`, with virtual resolution on the target when present.
    /// Mirrors from `getDeclaredMethods` carry the declaring method id;
    /// re-resolving by name+arity picks the wrong overload.
    pub(crate) fn invoke_method_mirror(
        &mut self,
        midx: GIdx,
        target: u32,
        args: Vec<V>,
    ) -> Result<Option<V>> {
        let res = if target != 0 {
            let mut a = Vec::with_capacity(args.len() + 1);
            a.push(V::Ref(target));
            a.extend_from_slice(&args);
            self.invoke(midx, a, InvokeKind::Virtual)
        } else {
            self.invoke(midx, args, InvokeKind::Static)
        };
        match res {
            Ok(v) => Ok(v),
            Err(E::Vm(e)) => Err(e),
            Err(E::Throw(r)) => {
                let d = self.describe_throw(r);
                Err(errd("dexrt.exception.uncaught", d))
            }
        }
    }

    pub(crate) fn call_static_named(
        &mut self,
        owner: &str,
        name: &str,
        args: Vec<V>,
    ) -> Result<Option<V>> {
        let ci = self.type_index_of(owner).unwrap_or(GX_NONE);
        let (midx, eff_args) = self.resolve_call_args(ci, name, &args)?;
        match midx {
            Some(midx) => match self.invoke(midx, eff_args, InvokeKind::Static) {
                Ok(v) => Ok(v),
                Err(E::Vm(e)) => Err(e),
                Err(E::Throw(r)) => {
                    let d = self.describe_throw(r);
                    Err(errd("dexrt.exception.uncaught", d))
                }
            },
            _ => {
                // Non-dex class (Looper, System, …): the declaring
                // descriptor can't resolve on a dex chain, but the
                // builtin surface can still answer it — same fallback
                // `call_virtual_sig` uses.
                if let Some(r) = jdk::call(self, owner, name, "", &args).map_err(|e| match e {
                    E::Vm(v) => v,
                    E::Throw(h) => {
                        let d = self.describe_throw(h);
                        errd("dexrt.exception.uncaught", d)
                    }
                })? {
                    return Ok(r);
                }
                Err(errd("dexrt.invoke.unresolved", format!("{owner}.{name}")))
            }
        }
    }

    /// `EncodedValue` → guest value, resolving the dex-local indices
    /// (string/type/field) against the dex that owns class `cg`.
    fn encoded_to_v(&mut self, cg: GIdx, ev: &ovelo_dex::EncodedValue) -> Option<V> {
        use ovelo_dex::EncodedValue as EV;
        match ev {
            EV::Byte(x) => Some(V::I32(*x as i32)),
            EV::Short(x) => Some(V::I32(*x as i32)),
            EV::Char(x) => Some(V::I32(*x as i32)),
            EV::Int(x) => Some(V::I32(*x)),
            EV::Bool(b) => Some(V::I32(*b as i32)),
            EV::Long(x) => Some(V::I64(*x)),
            EV::Float(x) => Some(V::F32(*x)),
            EV::Double(x) => Some(V::F64(*x)),
            EV::String(i) => {
                let s = self.dex_of(cg)?.string(*i).ok()?;
                Some(jdk::jstr(self, &s))
            }
            EV::Type(i) => Some(V::Ref(self.class_mirror(gx(gd(cg), *i)))),
            EV::Enum(f) | EV::Field(f) => {
                // The enum constant / field the element names — resolve
                // to the same object an `sget` would return so guest
                // `==` comparisons hold (EventBus's threadMode check).
                let d = self.dex_of(cg)?;
                let fi = d.field(*f).ok()?;
                let odesc = d.type_descriptor(fi.class_idx as u32).ok()?;
                let fname = d.string(fi.name_idx).ok()?;
                self.get_static_named(&odesc, &fname)
            }
            EV::Array(items) => {
                let vals: Vec<V> = items
                    .iter()
                    .map(|it| self.encoded_to_v(cg, it).unwrap_or(V::Ref(0)))
                    .collect();
                let h = self.heap.alloc(Obj::Array {
                    elem: "Ljava/lang/Object;".to_string(),
                    data: vals,
                });
                Some(V::Ref(h))
            }
            EV::Annotation { type_idx, elements } => {
                self.annotation_mirror(cg, *type_idx, elements)
            }
            _ => None,
        }
    }

    /// Mint an annotation mirror: a host obj typed by the annotation
    /// interface whose Map state carries the element name→value pairs
    /// (`Subscribe{threadMode:POSTING,sticky:false,priority:0}`).
    fn annotation_mirror(
        &mut self,
        cg: GIdx,
        type_idx: u32,
        elements: &[(u32, ovelo_dex::EncodedValue)],
    ) -> Option<V> {
        let tdesc = self.dex_of(cg)?.type_descriptor(type_idx).ok()?;
        let mut pairs = Vec::with_capacity(elements.len());
        for (ni, ev) in elements {
            let n = self.dex_of(cg)?.string(*ni).ok()?;
            let v = self.encoded_to_v(cg, ev).unwrap_or(V::Ref(0));
            pairs.push((jdk::jstr(self, &n), v));
        }
        Some(jdk::host(self, &tdesc, HostState::Map(pairs)))
    }

    /// The annotation set declared on a method — items are local to
    /// the defining dex (`cg` pairs them back to their dex context).
    pub(crate) fn method_annotation_items(
        &self,
        midx: GIdx,
    ) -> (GIdx, Vec<ovelo_dex::AnnotationItem>) {
        let fallback = (0, Vec::new());
        let Some(d) = self.dex_of(midx) else {
            return fallback;
        };
        let Ok(mid) = d.method(gl(midx)) else {
            return fallback;
        };
        let cg = gx(gd(midx), mid.class_idx as u32);
        let Some((_, def)) = self.def_loc_of(cg) else {
            return fallback;
        };
        let Ok(dir) = d.annotations_directory(def) else {
            return (cg, Vec::new());
        };
        let items = dir
            .method_annotations
            .iter()
            .find(|(mi, _)| *mi == gl(midx))
            .and_then(|(_, off)| d.annotation_set(*off).ok())
            .unwrap_or_default();
        (cg, items)
    }

    /// `Method.getAnnotation(Class)`: the declared annotation whose
    /// type descriptor matches, minted on demand.
    pub(crate) fn method_annotation(&mut self, midx: GIdx, annot_desc: &str) -> V {
        let (cg, items) = self.method_annotation_items(midx);
        for it in &items {
            if self
                .dex_of(cg)
                .and_then(|d| d.type_descriptor(it.type_idx).ok())
                .as_deref()
                == Some(annot_desc)
            {
                return self
                    .annotation_mirror(cg, it.type_idx, &it.elements)
                    .unwrap_or(V::Ref(0));
            }
        }
        V::Ref(0)
    }

    /// All declared annotations on a method as minted mirrors.
    pub(crate) fn method_annotations(&mut self, midx: GIdx) -> Vec<V> {
        let (cg, items) = self.method_annotation_items(midx);
        items
            .iter()
            .filter_map(|it| self.annotation_mirror(cg, it.type_idx, &it.elements))
            .collect()
    }

    /// The annotation set declared on a class — items are local to
    /// the class's defining dex (`cg` pairs them back).
    pub(crate) fn class_annotation_items(
        &self,
        cg: GIdx,
    ) -> (GIdx, Vec<ovelo_dex::AnnotationItem>) {
        let fallback = (cg, Vec::new());
        let Some((_, def)) = self.def_loc_of(cg) else {
            return fallback;
        };
        let Some(d) = self.dex_of(cg) else {
            return fallback;
        };
        let Ok(dir) = d.annotations_directory(def) else {
            return (cg, Vec::new());
        };
        if dir.class_annotations_off == 0 {
            return (cg, Vec::new());
        }
        let items = d
            .annotation_set(dir.class_annotations_off)
            .unwrap_or_default();
        (cg, items)
    }

    /// `Class.getAnnotation(Class)`: the declared class-level
    /// annotation whose type descriptor matches, minted on demand.
    pub(crate) fn class_annotation(&mut self, cg: GIdx, annot_desc: &str) -> V {
        let (cg, items) = self.class_annotation_items(cg);
        for it in &items {
            if self
                .dex_of(cg)
                .and_then(|d| d.type_descriptor(it.type_idx).ok())
                .as_deref()
                == Some(annot_desc)
            {
                return self
                    .annotation_mirror(cg, it.type_idx, &it.elements)
                    .unwrap_or(V::Ref(0));
            }
        }
        V::Ref(0)
    }

    /// All declared class-level annotations as minted mirrors.
    pub(crate) fn class_annotations(&mut self, cg: GIdx) -> Vec<V> {
        let (cg, items) = self.class_annotation_items(cg);
        items
            .iter()
            .filter_map(|it| self.annotation_mirror(cg, it.type_idx, &it.elements))
            .collect()
    }

    /// Canonical class GIdx for a type descriptor, when the dex
    /// defines it — class-annotation lookups key off this.
    pub(crate) fn class_gidx_of(&self, desc: &str) -> Option<GIdx> {
        let &(di, i) = self.defs.get(desc)?;
        let def = &self.dexes[di].class_defs()[i];
        Some(gx(di, def.class_idx))
    }

    /// `Method.getDefaultValue` — the `AnnotationDefault` element for an
    /// annotation-interface method. DX stores it on the interface's
    /// *class* annotation set with elements keyed by method name.
    pub(crate) fn method_default_value(&mut self, midx: GIdx) -> Option<V> {
        let (cg, name) = {
            let d = self.dex_of(midx)?;
            let mid = d.method(gl(midx)).ok()?;
            let cg = gx(gd(midx), mid.class_idx as u32);
            (cg, d.string(mid.name_idx).ok()?)
        };
        self.annotation_default_element(cg, &name)
    }

    /// Element `name` of the `AnnotationDefault` item in the class
    /// annotation set of `cg`, converted to a value.
    fn annotation_default_element(&mut self, cg: GIdx, name: &str) -> Option<V> {
        let (_, def) = self.def_loc_of(cg)?;
        let dir = self.dex_of(cg)?.annotations_directory(def).ok()?;
        if dir.class_annotations_off == 0 {
            return None;
        }
        let items = self
            .dex_of(cg)?
            .annotation_set(dir.class_annotations_off)
            .ok()?;
        for it in &items {
            if self
                .dex_of(cg)
                .and_then(|d| d.type_descriptor(it.type_idx).ok())
                .as_deref()
                != Some("Ldalvik/annotation/AnnotationDefault;")
            {
                continue;
            }
            for (ni, ev) in &it.elements {
                if self.dex_of(cg).and_then(|d| d.string(*ni).ok()).as_deref() == Some(name) {
                    return self.encoded_to_v(cg, ev);
                }
            }
        }
        None
    }

    /// Answer a bodiless annotation-interface call (`subscribe.threadMode()`)
    /// from the receiver mirror's elements, then its declared
    /// `AnnotationDefault`. `None` when this isn't an annotation call.
    fn annotation_method_value(&mut self, midx: GIdx, args: &[V]) -> Option<V> {
        let dbg = crate::dbg_flag("OVELO_ANNDBG");
        if dbg {
            eprintln!("[anndbg] enter {}", self.describe_method(midx));
        }
        let (cg, name) = {
            let d = self.dex_of(midx)?;
            let mid = d.method(gl(midx)).ok()?;
            let cg = gx(gd(midx), mid.class_idx as u32);
            let (_, def) = match self.def_loc_of(cg) {
                Some(x) => x,
                None => {
                    if dbg {
                        eprintln!(
                            "[anndbg] no def for {}",
                            self.type_desc_of(cg).unwrap_or_default()
                        );
                    }
                    return None;
                }
            };
            // ACC_ANNOTATION — only interface-style dex classes qualify.
            if def.access_flags & 0x2000 == 0 {
                if dbg {
                    eprintln!(
                        "[anndbg] {} not annotation flags={:#x}",
                        self.type_desc_of(cg).unwrap_or_default(),
                        def.access_flags
                    );
                }
                return None;
            }
            (cg, d.string(mid.name_idx).ok()?)
        };
        let V::Ref(h) = args.first().copied().unwrap_or(V::Ref(0)) else {
            return None;
        };
        let owner_desc = self.type_desc_of(cg).ok()?;
        if dbg {
            let recv = self.heap.get(h).map(|o| match o {
                Obj::Host { desc, .. } => desc.clone(),
                Obj::Instance { class_idx, .. } => {
                    self.type_desc_of(*class_idx).unwrap_or_else(|_| "?".into())
                }
                _ => "?".to_string(),
            });
            eprintln!(
                "[anndbg] annotation_method_value owner={owner_desc} name={name} recv={recv:?}"
            );
        }
        if let Some(Obj::Host {
            desc,
            state: HostState::Map(m),
            ..
        }) = self.heap.get(h)
        {
            if *desc == owner_desc {
                for (k, v) in m {
                    let kn = match k {
                        V::Ref(kh) => jdk::s(self, *kh),
                        _ => None,
                    };
                    if kn.as_deref() == Some(name.as_str()) {
                        return Some(*v);
                    }
                }
            } else {
                return None;
            }
        } else {
            return None;
        }
        // Declared default: the interface's class annotation set holds
        // an AnnotationDefault item keyed by method name.
        self.annotation_default_element(cg, &name)
    }

    /// Mint `java.lang.reflect.Method` mirrors for every method a dex
    /// class declares — `getDeclaredMethods` backs reflection scanners
    /// like EventBus's `@Subscribe` discovery.
    pub(crate) fn declared_method_mirrors(&mut self, owner: &str) -> Vec<V> {
        let mut specs: Vec<(GIdx, String, u32)> = Vec::new();
        {
            let Some(ci) = self.type_index_of(owner) else {
                return Vec::new();
            };
            let Some((cg, def)) = self.def_loc_of(ci) else {
                return Vec::new();
            };
            let Some(d) = self.dex_of(cg) else {
                return Vec::new();
            };
            let Ok(cd) = d.class_data_rc(def) else {
                return Vec::new();
            };
            for em in cd.direct_methods.iter().chain(cd.virtual_methods.iter()) {
                let name = d
                    .method(em.method_idx)
                    .ok()
                    .and_then(|m| d.string(m.name_idx).ok())
                    .unwrap_or_default();
                specs.push((gx(gd(cg), em.method_idx), name, em.access_flags));
            }
        }
        let cv = jdk::jstr(self, owner);
        specs
            .into_iter()
            .map(|(midx, name, flags)| {
                let hv = jdk::host(self, "Ljava/lang/reflect/Method;", HostState::StrBuf(name));
                if let V::Ref(h) = hv {
                    jdk::pseudo_set(self, h, jdk::P_COMPONENT, cv);
                    jdk::pseudo_set(self, h, jdk::P_METH, V::I64(midx as i64));
                    jdk::pseudo_set(self, h, jdk::P_FLAGS, V::I32(flags as i32));
                }
                hv
            })
            .collect()
    }

    /// `Class.getDeclaredFields`/`getFields`: mint a `Field` mirror per
    /// declared dex field (statics first, like the dex class data). The
    /// mirror carries the exact field index in `P_DATA` so `Field.get`
    /// and `getType`/`getModifiers` answer for the real member.
    pub(crate) fn declared_field_mirrors(&mut self, owner: &str) -> Vec<V> {
        let mut specs: Vec<(GIdx, String, u32)> = Vec::new();
        {
            let Some(ci) = self.type_index_of(owner) else {
                return Vec::new();
            };
            let Some((cg, def)) = self.def_loc_of(ci) else {
                return Vec::new();
            };
            let Some(d) = self.dex_of(cg) else {
                return Vec::new();
            };
            let Ok(cd) = d.class_data_rc(def) else {
                return Vec::new();
            };
            for ef in cd.static_fields.iter().chain(cd.instance_fields.iter()) {
                let name = d
                    .field(ef.field_idx)
                    .ok()
                    .and_then(|f| d.string(f.name_idx).ok())
                    .unwrap_or_default();
                specs.push((gx(gd(cg), ef.field_idx), name, ef.access_flags));
            }
        }
        let cv = jdk::jstr(self, owner);
        specs
            .into_iter()
            .map(|(fidx, name, flags)| {
                let hv = jdk::host(self, "Ljava/lang/reflect/Field;", HostState::StrBuf(name));
                if let V::Ref(h) = hv {
                    jdk::pseudo_set(self, h, jdk::P_COMPONENT, cv);
                    jdk::pseudo_set(self, h, jdk::P_DATA, V::I64(fidx as i64));
                    jdk::pseudo_set(self, h, jdk::P_FLAGS, V::I32(flags as i32));
                }
                hv
            })
            .collect()
    }

    /// `Class.getDeclaredClasses`: `Class` objects for the nested classes
    /// defined in the dex (`Foo$Inner`, `Foo$$serializer`, ...).
    pub(crate) fn nested_class_mirrors(&mut self, owner: &str) -> Vec<V> {
        let prefix = format!("{}$", owner.trim_end_matches(';'));
        let names: Vec<String> = self
            .defs
            .keys()
            .filter(|k| k.starts_with(prefix.as_str()))
            .cloned()
            .collect();
        names.into_iter().map(|n| jdk::class_obj(self, n)).collect()
    }

    /// A field's declared type descriptor.
    pub(crate) fn field_type_desc(&self, fidx: GIdx) -> Option<String> {
        let fidx = self.canon_field(fidx);
        let d = self.dex_of(fidx)?;
        let f = d.field(gl(fidx)).ok()?;
        d.type_descriptor(f.type_idx as u32).ok()
    }

    /// A method's declared parameter type descriptors (proto order).
    pub(crate) fn method_param_descs(&self, midx: GIdx) -> Vec<String> {
        let Some(d) = self.dex_of(midx) else {
            return Vec::new();
        };
        let (Ok(mid),) = (d.method(gl(midx)),) else {
            return Vec::new();
        };
        let Ok(p) = d.proto(mid.proto_idx as u32) else {
            return Vec::new();
        };
        d.proto_params(p)
            .unwrap_or_default()
            .iter()
            .filter_map(|t| d.type_descriptor(*t as u32).ok())
            .collect()
    }

    /// A method's declared return type descriptor.
    pub(crate) fn method_return_desc(&self, midx: GIdx) -> Option<String> {
        let d = self.dex_of(midx)?;
        let mid = d.method(gl(midx)).ok()?;
        let p = d.proto(mid.proto_idx as u32).ok()?;
        d.type_descriptor(p.return_type_idx).ok()
    }

    /// Read a guest static field by owner + name — host-side access to
    /// singletons like `AppInitializer.sInstance` that dex code owns.
    pub fn get_static_named(&mut self, owner: &str, name: &str) -> Option<V> {
        let g = self.field_index_of(owner, name)?;
        self.sget_pub(g)
    }

    /// Reflective static read for `Field.get(null)`. Unlike `sget`, a
    /// `<clinit>` failure triggered by the probe is rolled back: a
    /// reflective miss (kotlinx.serialization's `INSTANCE`/`Companion`
    /// probes touch many classes) must not poison the class for the
    /// real `sget`s that run later.
    pub(crate) fn probe_static_named(&mut self, owner: &str, name: &str) -> Option<V> {
        let f = self
            .field_index_of(owner, name)
            .map(|g| self.canon_field(g))?;
        let owner_idx = self
            .dex_of(f)
            .and_then(|d| d.field(gl(f)).ok())
            .map(|fi| self.canon(gx(gd(f), fi.class_idx as u32)))
            .unwrap_or(f);
        match self.classes.get(&owner_idx).map(|c| c.clinit) {
            Some(2) => {}
            // In-progress or already-failed: probing more is wrong.
            _ if self.classes.contains_key(&owner_idx) => return None,
            _ => {
                let prev3: hashbrown::HashSet<GIdx> = self
                    .classes
                    .iter()
                    .filter(|(_, c)| c.clinit == 3)
                    .map(|(k, _)| *k)
                    .collect();
                if self.ensure_init(owner_idx).is_err() {
                    for (k, c) in self.classes.iter_mut() {
                        if c.clinit == 3 && !prev3.contains(k) {
                            c.clinit = 0;
                        }
                    }
                    return None;
                }
            }
        }
        self.classes
            .get(&owner_idx)
            .and_then(|c| c.statics.get(&f))
            .copied()
            .or_else(|| {
                if self.def_of(owner_idx).is_none() {
                    self.builtin_sfield(f).or_else(|| self.field_default(f))
                } else {
                    self.encoded_static(owner_idx, f)
                }
            })
    }

    /// Read a static like the `sget` opcode: guest-encoded value,
    /// builtin table for host classes, field-type default last.
    pub(crate) fn sget_pub(&mut self, f: GIdx) -> Option<V> {
        let f = self.canon_field(f);
        let owner = self
            .dex_of(f)
            .and_then(|d| d.field(gl(f)).ok())
            .map(|fi| self.canon(gx(gd(f), fi.class_idx as u32)))
            .unwrap_or(f);
        // kotlinx's MainDispatcherLoader depends on ServiceLoader
        // metadata that the runtime does not expose. Materialize the
        // bundled Android HandlerContext directly instead of letting a
        // failed `<clinit>` degrade the field to a type-only host stub.
        let dbg_fname = self.dbg_field_name(f);
        if dbg_fname == "Lkotlinx/coroutines/internal/MainDispatcherLoader;.dispatcher" {
            if let Some(v) = self.classes.get(&owner).and_then(|c| c.statics.get(&f)) {
                return Some(*v);
            }
            let v = jdk::main_dispatcher(self);
            self.classes.entry(owner).or_default().statics.insert(f, v);
            return Some(v);
        }
        self.ensure_init(owner).ok()?;
        if let Some(v) = self.classes.get(&owner).and_then(|c| c.statics.get(&f)) {
            return Some(*v);
        }
        let v = if self.def_of(owner).is_none() {
            self.builtin_sfield(f).or_else(|| self.field_default(f))
        } else {
            self.encoded_static(owner, f)
                .or_else(|| self.field_default(f))
        }?;
        if matches!(v, V::Ref(h) if h != 0) {
            self.classes.entry(owner).or_default().statics.insert(f, v);
        }
        Some(v)
    }

    /// Write a static like the `sput` opcode.
    pub(crate) fn sput_pub(&mut self, f: GIdx, v: V) {
        let f = self.canon_field(f);
        let owner = self
            .dex_of(f)
            .and_then(|d| d.field(gl(f)).ok())
            .map(|fi| self.canon(gx(gd(f), fi.class_idx as u32)))
            .unwrap_or(f);
        self.classes.entry(owner).or_default().statics.insert(f, v);
    }

    /// Walk `class_idx`'s chain for a field by name (static when
    /// `is_static`, instance otherwise) → global field idx. When
    /// `want_ty` is given and several fields share the name (legal in
    /// dex, common under obfuscation), the one declaring that type wins
    /// over an earlier same-name hit on a shallower class.
    pub(crate) fn find_field_by_name(
        &self,
        mut class_idx: GIdx,
        name: &str,
        is_static: bool,
        want_ty: Option<&str>,
    ) -> Option<GIdx> {
        let mut name_hit = None;
        for _ in 0..64 {
            let Some((cg, def)) = self.def_loc_of(class_idx) else {
                return name_hit;
            };
            let Some(d) = self.dex_of(cg) else {
                return name_hit;
            };
            let Ok(cd) = d.class_data_rc(def) else {
                return name_hit;
            };
            let it = if is_static {
                &cd.static_fields
            } else {
                &cd.instance_fields
            };
            for f in it {
                if let Ok(fi) = d.field(f.field_idx) {
                    let Ok(fname) = d.string(fi.name_idx) else {
                        continue;
                    };
                    if fname != name {
                        continue;
                    }
                    if let Some(want) = want_ty {
                        if d.type_descriptor(fi.type_idx as u32).ok().as_deref() == Some(want) {
                            return Some(gx(gd(cg), f.field_idx));
                        }
                    }
                    if name_hit.is_none() {
                        name_hit = Some(gx(gd(cg), f.field_idx));
                    }
                }
            }
            if def.superclass_idx == u32::MAX {
                return name_hit;
            }
            class_idx = gx(gd(cg), def.superclass_idx);
        }
        name_hit
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
        let ins = code.ins_size as usize;
        let start = nregs.saturating_sub(ins);
        let mut regs = vec![0i64; nregs];
        let write = |regs: &mut Vec<i64>, i: usize, a: &V| {
            if i < regs.len() {
                regs[i] = match a {
                    V::I32(v) => *v as i64,
                    V::I64(v) => *v,
                    V::F32(v) => v.to_bits() as i64,
                    V::F64(v) => v.to_bits() as i64,
                    V::Ref(h) => *h as i64,
                };
            }
        };
        // Same reg-vs-param convention split as exec_frame: dex sites
        // pass every register incl. wide high halves; host calls pass
        // one V per parameter and J/D consume two registers.
        if args.len() == ins {
            for (i, a) in args.iter().enumerate() {
                write(&mut regs, start + i, a);
            }
        } else {
            let mut ri = start;
            for a in &args {
                write(&mut regs, ri, a);
                ri += if matches!(a, V::I64(_) | V::F64(_)) {
                    2
                } else {
                    1
                };
            }
        }
        let mut trap: u8 = 0;
        self.stack.push(midx);
        #[allow(unsafe_code)]
        let ret = unsafe { f(regs.as_mut_ptr(), &mut trap) };
        self.stack.pop();
        let site = self.describe_method(midx).to_string();
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
        if self.depth >= 4096 {
            return Err(self.throw_builtin("Ljava/lang/StackOverflowError;", "call depth exceeded"));
        }
        self.depth += 1;
        self.stack.push(loc.midx);
        let r = self.exec_frame(loc, args);
        self.stack.pop();
        self.depth -= 1;
        r
    }

    fn exec_frame(&mut self, loc: Loc, args: Vec<V>) -> Rt<Option<V>> {
        let code_hit = self.code_cache.borrow().get(&loc.midx).cloned();
        let code = if let Some(c) = code_hit {
            c
        } else {
            let c = std::rc::Rc::new(
                self.dex_of(loc.midx)
                    .ok_or_else(|| E::Vm(err("dexrt.dex.idx")))?
                    .code_item(loc.em.code_off)
                    .map_err(E::Vm)?
                    .ok_or_else(|| E::Vm(err("dexrt.method.no_code")))?,
            );
            self.code_cache.borrow_mut().insert(loc.midx, c.clone());
            c
        };
        let table_hit = self.insn_cache.borrow().get(&loc.midx).cloned();
        let table = if let Some(t) = table_hit {
            t
        } else {
            let t = std::rc::Rc::new(decode_all(&code.insns).map_err(E::Vm)?);
            self.insn_cache.borrow_mut().insert(loc.midx, t.clone());
            t
        };
        let pcmap_hit = self.pcmap_cache.borrow().get(&loc.midx).cloned();
        let pcmap = if let Some(m) = pcmap_hit {
            m
        } else {
            let mut m = vec![0u32; code.insns.len().saturating_add(1)];
            for (pos, insn) in table.iter().enumerate() {
                if let Some(slot) = m.get_mut(insn.offset as usize) {
                    *slot = pos as u32 + 1;
                }
            }
            let m = std::rc::Rc::new(m);
            self.pcmap_cache.borrow_mut().insert(loc.midx, m.clone());
            m
        };
        let nregs = code.registers_size as usize;
        let mut regs = self.reg_pool.pop().unwrap_or_default();
        regs.clear();
        regs.resize(nregs, V::I32(0));
        let mut fr = Frame {
            regs,
            result: None,
            last_call: 0,
            exc: None,
            table,
            pcmap,
            loc,
            code,
        };
        // Copy args into the last `ins_size` registers. Dex invoke
        // sites pass one entry per *register* (a wide arg's high half
        // is listed too, so args.len()==ins_size); host-side callers
        // pass one V per *parameter* — a J/D arg then needs an extra
        // register slot after it or every later arg shifts one reg.
        let ins = fr.code.ins_size as usize;
        let start = nregs.saturating_sub(ins);
        if args.len() == ins {
            for (i, a) in args.iter().enumerate() {
                if start + i < nregs {
                    fr.regs[start + i] = *a;
                }
            }
        } else {
            let mut ri = start;
            for a in &args {
                if ri < nregs {
                    fr.regs[ri] = *a;
                }
                ri += if matches!(a, V::I64(_) | V::F64(_)) {
                    2
                } else {
                    1
                };
            }
        }
        let out = self.exec_frame_loop(&mut fr);
        let mut regs = std::mem::take(&mut fr.regs);
        if regs.capacity() <= 1024 && self.reg_pool.len() < 128 {
            regs.clear();
            self.reg_pool.push(regs);
        }
        out
    }

    fn exec_frame_loop(&mut self, fr: &mut Frame) -> Rt<Option<V>> {
        let mut pc: usize = 0;
        // Instruction budget — a guest (or stub-driven) loop that never
        // terminates must fail loudly, not hang the host process.
        // Compose's gapbuffer SlotWriter null-fills freshly grown slot
        // arrays one element per dex op — a big composition legitimately
        // clears the 20M default, so the cap is env-overrideable for
        // diagnosis/throughput trade-offs.
        let budget: u64 = std::env::var("OVELO_BUDGET")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(20_000_000);
        let stalldbg = crate::dbg_flag("OVELO_STALLDBG");
        let mut steps: u64 = 0;
        loop {
            steps += 1;
            if stalldbg && steps.is_multiple_of(5_000_000) {
                eprintln!(
                    "[stall] {} +{pc:04x} steps={steps}",
                    self.describe_method(fr.loc.midx)
                );
                for (i, m) in self.stack.iter().rev().take(8).enumerate() {
                    eprintln!("[stall-frm{i}] {}", self.describe_method(*m));
                }
            }
            self.total_steps += 1;
            if stalldbg && self.total_steps.is_multiple_of(1_000_000) {
                eprintln!(
                    "[stall-total] {} +{pc:04x} total={}",
                    self.describe_method(fr.loc.midx),
                    self.total_steps
                );
                if self.total_steps.is_multiple_of(10_000_000) {
                    eprintln!("[stall-depth] {}", self.stack.len());
                    for (i, m) in self.stack.iter().take(8).enumerate() {
                        eprintln!("[stall-root{i}] {}", self.describe_method(*m));
                    }
                }
            }
            if steps > budget {
                return Err(self.throw_builtin(
                    "Ljava/lang/InternalError;",
                    &format!("instruction budget exceeded at +{pc:04x}"),
                ));
            }
            let res = self.step(fr, pc);
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
                Err(E::Throw(r)) => match self.find_handler(fr, pc, r) {
                    _ if self.trace_throw => {
                        let d = self.describe_throw(r);
                        let method = self.describe_method(fr.loc.midx);
                        let hit = self
                            .trace_throw_filter
                            .as_deref()
                            .map(|p| {
                                p.split(',').any(|one| {
                                    !one.is_empty() && (method.contains(one) || d.contains(one))
                                })
                            })
                            .unwrap_or(true);
                        if hit {
                            eprintln!("[throw] {method} +{pc:04x} -> {d}");
                        }
                        match self.find_handler(fr, pc, r) {
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
    /// Method the last invoke targeted — named lazily when a
    /// `move-result` reads a void result.
    last_call: GIdx,
    /// Pending exception for `move-exception`.
    exc: Option<u32>,
    loc: Loc,
    code: std::rc::Rc<ovelo_dex::CodeItem>,
    /// Decoded instructions (payloads included), in offset order.
    table: std::rc::Rc<Vec<Insn>>,
    /// Dense code-unit offset → table position + 1 (0 = invalid).
    pcmap: std::rc::Rc<Vec<u32>>,
}

#[derive(Clone, Copy)]
enum InvokeKind {
    Static,
    Direct,
    Virtual,
    Interface,
    /// Dispatch begins at the caller's superclass.
    Super {
        caller_class: GIdx,
    },
}

impl Vm {
    /// Set a possibly-wide value: mirrors into `regs[i+1]` for `I64`/`F64`.
    fn set(fr: &mut Frame, i: usize, v: V) {
        // One V slot carries a whole wide value (I64/F64), so a
        // single-register op writes exactly one slot. Mirroring into
        // i+1 would clobber the *next live register* — e.g.
        // `move-result-object v2` returning an unboxed number must not
        // overwrite v3. Dex only ever addresses wide pairs by their
        // low slot, so the mirror slot is never read.
        if let Some(r) = fr.regs.get_mut(i) {
            *r = v;
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
            _ => Err(E::Vm(errd("dexrt.type", format!("i32v {v:?}")))),
        }
    }
    fn i64v(v: V) -> Rt<i64> {
        match v {
            V::I64(i) => Ok(i),
            _ => Err(E::Vm(errd("dexrt.type", format!("i64v {v:?}")))),
        }
    }
    fn f32v(v: V) -> Rt<f32> {
        match v {
            V::F32(f) => Ok(f),
            V::I32(i) => Ok(f32::from_bits(i as u32)),
            _ => Err(E::Vm(errd("dexrt.type", format!("f32v {v:?}")))),
        }
    }
    fn f64v(v: V) -> Rt<f64> {
        match v {
            V::F64(f) => Ok(f),
            V::I64(i) => Ok(f64::from_bits(i as u64)),
            _ => Err(E::Vm(errd("dexrt.type", format!("f64v {v:?}")))),
        }
    }
    fn refv(v: V) -> Rt<u32> {
        match v {
            V::Ref(h) => Ok(h),
            // `const/4 vx, 0` is how dex puts a null reference in a register.
            V::I32(0) => Ok(0),
            _ => Err(E::Vm(errd("dexrt.type", format!("refv {v:?}")))),
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
        // The decoded table is ordered by code-unit offset. A linear
        // scan here multiplies every executed instruction by the host
        // method's length; hot parser loops sit in large methods.
        let pos = fr
            .pcmap
            .get(pc)
            .copied()
            .filter(|p| *p != 0)
            .map(|p| p as usize - 1)
            .ok_or_else(|| E::Vm(err("dexrt.pc.bounds")))?;
        let ins = &fr.table[pos];
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
                            self.describe_method(fr.last_call)
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
                let sg = gi(ins.idx.unwrap_or(0));
                let hit = self.str_const_cache.borrow().get(&sg).copied();
                let h = if let Some(h) = hit {
                    h
                } else {
                    let s = self.dexes[di].string(ins.idx.unwrap_or(0)).map_err(E::Vm)?;
                    let h = self.heap.intern_str(&s);
                    self.str_const_cache.borrow_mut().insert(sg, h);
                    h
                };
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
                // A scalar reaching check-cast is valid when the target
                // is a box class it can satisfy (JSON numbers as Number).
                let h = match r(0)? {
                    V::Ref(h) => h,
                    // `const 0` is the null literal in a reference slot —
                    // null casts to anything.
                    v if Self::scalar_is_zero(v) => 0,
                    v if self.scalar_is_instance(v, gi(ins.idx.unwrap_or(0))) => 0,
                    _ => {
                        return Err(self.throw_builtin(
                            "Ljava/lang/ClassCastException;",
                            "check-cast on primitive",
                        ))
                    }
                };
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
                // Untyped dex registers can hand a number to
                // `instanceof` (JSON values map to I64/F64) — a
                // non-reference is never an instance: false, not fault.
                let res = match r(1)? {
                    V::Ref(h) => h != 0 && self.is_instance(h, gi(ins.idx.unwrap_or(0))),
                    v => self.scalar_is_instance(v, gi(ins.idx.unwrap_or(0))),
                };
                set!(regs[0], V::I32(res as i32));
            }
            0x21 => {
                let h = Self::refv(r(1)?)?;
                self.null_check(h, "array-length")?;
                let n = match self.heap.get(h) {
                    Some(Obj::Array { data, .. }) => data.len() as i32,
                    other => {
                        return Err(E::Vm(errd(
                            "dexrt.type",
                            format!("array-length h{h} {:?}", other.map(std::mem::discriminant)),
                        )))
                    }
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
                self.mark_view_alloc(h, ty);
                set!(regs[0], V::Ref(h));
            }
            0x23 => {
                let len = Self::i32v(r(1)?)?;
                if len < 0 {
                    return Err(
                        self.throw_builtin("Ljava/lang/NegativeArraySizeException;", "new-array")
                    );
                }
                let desc = self.dexes[di]
                    .type_descriptor(ins.idx.unwrap_or(0))
                    .map_err(E::Vm)?;
                let elem = desc.strip_prefix('[').unwrap_or(&desc).to_string();
                let h = self.heap.alloc(Obj::Array {
                    data: vec![V::default_for(&elem); len as usize],
                    elem,
                });
                set!(regs[0], V::Ref(h));
            }
            0x24 | 0x25 => {
                // filled-new-array {args}, type@idx — `/range` carries
                // (count, first_reg), the 35c form lists registers.
                let desc = self.dexes[di]
                    .type_descriptor(ins.idx.unwrap_or(0))
                    .map_err(E::Vm)?;
                let elem = desc.strip_prefix('[').unwrap_or(&desc).to_string();
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
                    return Err(
                        self.throw_builtin("Ljava/lang/NullPointerException;", "throw null")
                    );
                }
                // First-throw wins: a catch-all `throw` at the top of a
                // helper (FragmentManager force-complete & co) would
                // otherwise clobber the real origin site.
                if !self.stack.is_empty() {
                    let site = self.throw_site_desc();
                    self.throw_sites.entry(h).or_insert(site);
                } else {
                    let site = self.describe_method(fr.loc.midx).to_string();
                    self.throw_sites.entry(h).or_insert(site);
                }
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
                    tgt.map(|t| (pc as i64 + t as i64) as usize).unwrap_or(next),
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
                    if less_bias {
                        -1
                    } else {
                        1
                    }
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
                            _ => match (va, vb) {
                                (V::I32(a), V::I32(b)) => a == b,
                                // A reference (or null) never equals a
                                // wide/float value in dex's untyped
                                // registers — report unequal rather
                                // than faulting on i32v.
                                _ => false,
                            },
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
                if !t && op == 0x35 {
                    // Canonical constant-fill counted loop:
                    //   if-ge i, n, EXIT
                    //   const z, C
                    //   aput-object z, arr, i
                    //   add-int/lit8 i, i, +1
                    //   goto HEAD
                    // `new-array` already default-fills its data, so
                    // re-storing the same constant is a pure no-op —
                    // but Compose's SlotWriter.insertSlots emits this
                    // shape over arrays grown to ~2M slots, burning
                    // the whole instruction budget one element per
                    // step. When the write range already holds the
                    // constant, finish the loop in one step.
                    let pat = |o: usize| fr.table.get(o);
                    // Pull every operand out as a value first so the
                    // `fr.table` borrow ends before `Self::set(fr,…)`.
                    let fill = if let (Some(cst), Some(ap), Some(inc), Some(go)) =
                        (pat(pos + 1), pat(pos + 2), pat(pos + 3), pat(pos + 4))
                    {
                        let lit = match cst.opcode {
                            0x12..=0x14 => cst.lit.unwrap_or(0),
                            0x15 => cst.lit.unwrap_or(0) << 16,
                            _ => i64::MAX,
                        };
                        if lit != i64::MAX
                            && ap.opcode == 0x4d
                            && ap.regs.first() == cst.regs.first()
                            && ap.regs.get(2) == ins.regs.first()
                            && inc.opcode == 0xd8
                            && inc.regs.first() == ins.regs.first()
                            && inc.regs.get(1) == ins.regs.first()
                            && inc.lit == Some(1)
                            && go.opcode == 0x28
                            && go.target == Some(ins.offset)
                            // The const target must not alias the
                            // counter, bound or array register — an
                            // aliased store would also change what the
                            // loop compares or writes through.
                            && cst.regs.first() != ins.regs.first()
                            && cst.regs.first() != ins.regs.get(1)
                            && cst.regs.first() != ap.regs.get(1)
                        {
                            Some((cst.regs[0] as usize, ap.regs[1] as usize, lit as i32))
                        } else {
                            None
                        }
                    } else {
                        None
                    };
                    if let Some((z, arr_reg, c)) = fill {
                        let (i0, n) = (Self::i32v(va)?, Self::i32v(vb)?);
                        let ah = Self::refv(Self::reg(fr, arr_reg)?)?;
                        let probed = !self.fill_bad.contains(&ah);
                        if let Some(Obj::Array { data, .. }) = self.heap.get(ah).filter(|_| probed)
                        {
                            // The loop is a no-op iff every slot it
                            // would touch already holds the canonical
                            // value the store would leave — for
                            // aput-object that is Ref(0) (aput
                            // canonicalizes I32(0) → Ref(0) on object
                            // arrays), so a skipped range is bit-equal
                            // to a looped one, including under
                            // Arrays.equals.
                            let stored = if c == 0 { V::Ref(0) } else { V::I32(c) };
                            let uniform = i0 >= 0
                                && n >= 0
                                && n as usize <= data.len()
                                && data[i0 as usize..n as usize].iter().all(|v| *v == stored);
                            if uniform {
                                let counter_reg = ins.regs[0] as usize;
                                let target =
                                    ins.target.ok_or_else(|| E::Vm(err("dexrt.branch")))? as usize;
                                Self::set(fr, counter_reg, V::I32(n));
                                Self::set(fr, z, V::I32(c));
                                return Ok(Out::Next(target));
                            }
                            // Non-uniform: the real loop runs — and a
                            // rescan on every head hit would be O(n²),
                            // so the failed handle is remembered and
                            // later hits go straight to interpretation.
                            self.fill_bad.insert(ah);
                        }
                    }
                }
                if t {
                    return Ok(Out::Next(
                        ins.target.ok_or_else(|| E::Vm(err("dexrt.branch")))? as usize,
                    ));
                }
            }
            0x38..=0x3d => {
                let va = r(0)?;
                // if-eqz/if-nez also test references against null — and
                // unboxed scalars standing in for boxed values (a JSON
                // Number reaching the slot) count as non-null/nonzero.
                let a = match va {
                    V::Ref(h) => h as i32,
                    V::I64(i) => (i != 0) as i32,
                    V::F64(f) => (f != 0.0) as i32,
                    V::F32(f) => (f != 0.0) as i32,
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
                if crate::dbg_flag("OVELO_ARRDBG") {
                    if let V::Ref(vh) = v {
                        if let Some(Obj::Array { elem, data }) = self.heap.get(vh) {
                            eprintln!(
                                "[arrdbg] aget h{h}[{i}] -> array h{vh} elem={elem} len={} in {}",
                                data.len(),
                                self.describe_method(fr.loc.midx)
                            );
                        }
                    }
                }
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
                        "iget {} @{pc:04x} in {}",
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
                    other => {
                        if crate::dbg_flag("OVELO_VIEWDBG") {
                            let kind = match other {
                                Some(Obj::Array { .. }) => "array",
                                Some(Obj::Str(_)) => "string",
                                Some(Obj::Throwable { .. }) => "throwable",
                                Some(Obj::Class { .. }) => "class",
                                Some(Obj::Instance { .. }) | Some(Obj::Host { .. }) => "obj",
                                None => "freed",
                            };
                            eprintln!(
                                "[viewdbg] iget.type h{h} {kind} field={} in {}",
                                self.describe_field(gi(ins.idx.unwrap_or(0))),
                                self.describe_method(fr.loc.midx)
                            );
                        }
                        return Err(E::Vm(errd(
                            "dexrt.type",
                            format!(
                                "iget h{h} field={} kind={:?} arr={:?}",
                                self.describe_field(gi(ins.idx.unwrap_or(0))),
                                self.heap.get(h).map(std::mem::discriminant),
                                match self.heap.get(h) {
                                    Some(Obj::Array { elem, data }) => format!(
                                        "elem={elem} len={} e0={:?}",
                                        data.len(),
                                        data.first().and_then(|v| match v {
                                            V::Ref(eh) =>
                                                self.heap.get(*eh).map(std::mem::discriminant),
                                            _ => None,
                                        })
                                    ),
                                    _ => "-".into(),
                                }
                            ),
                        )));
                    }
                };
                if self
                    .trace_field
                    .as_deref()
                    .is_some_and(|t| self.describe_field(gi(ins.idx.unwrap_or(0))).contains(t))
                {
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
                    None => match self.builtin_ifield(h, f) {
                        Some(b) => b,
                        // A host placeholder's ctor never ran — ref-typed
                        // fields stay null and the next iget/iput on them
                        // NPEs (RecyclerView lp.mDecorInsets.top). Give
                        // them a stub of the declared type, stored so the
                        // same object comes back on later reads.
                        None => match self.ref_field_stub(h, f) {
                            Some(v) => v,
                            None => self.field_default(f).unwrap_or(V::I32(0)),
                        },
                    },
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
                if self
                    .trace_field
                    .as_deref()
                    .is_some_and(|t| self.describe_field(gi(ins.idx.unwrap_or(0))).contains(t))
                {
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
                    _ => {
                        return Err(E::Vm(errd(
                            "dexrt.type",
                            format!(
                                "iput h{h} field={} v={:?}",
                                self.describe_field(gi(ins.idx.unwrap_or(0))),
                                r(0).ok()
                            ),
                        )))
                    }
                }
            }
            // ---- static fields ----
            0x60..=0x66 => {
                let fl = ins.idx.unwrap_or(0);
                let f = self.canon_field(gi(fl));
                let owner =
                    self.canon(gi(self.dexes[di].field(fl).map_err(E::Vm)?.class_idx as u32));
                let main_dispatcher_static = self.dbg_field_name(f)
                    == "Lkotlinx/coroutines/internal/MainDispatcherLoader;.dispatcher";
                if !main_dispatcher_static {
                    self.ensure_init(owner)?;
                }
                let stored = self
                    .classes
                    .entry(owner)
                    .or_default()
                    .statics
                    .get(&f)
                    .copied();
                if crate::dbg_flag("OVELO_SGETDBG") && !matches!(stored, Some(V::Ref(h)) if h != 0)
                {
                    let fd = self.dexes[di].field(fl).map_err(E::Vm)?;
                    let cname = self.dexes[di]
                        .type_descriptor(fd.class_idx as u32)
                        .unwrap_or_default();
                    let fname = self.dexes[di].string(fd.name_idx).unwrap_or_default();
                    let keys: Vec<GIdx> = self
                        .classes
                        .get(&owner)
                        .map(|c| c.statics.keys().copied().collect())
                        .unwrap_or_default();
                    eprintln!(
                        "[sgetdbg] miss {cname}.{fname} fl={fl} f={f:?} owner={owner:?} stored={stored:?} keys={keys:?}"
                    );
                }
                let v = if main_dispatcher_static {
                    match stored {
                        Some(v) => v,
                        None => {
                            let v = jdk::main_dispatcher(self);
                            self.classes.entry(owner).or_default().statics.insert(f, v);
                            v
                        }
                    }
                } else {
                    match stored {
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
                                self.classes.entry(owner).or_default().statics.insert(f, v);
                            }
                            v
                        }
                        None => {
                            // A static on a guest class carries its
                            // initializer in the dex `static_values` array —
                            // that's where every `R.*` constant lives.
                            let v = self
                                .encoded_static(owner, f)
                                .unwrap_or_else(|| self.field_default(f).unwrap_or(V::I32(0)));
                            if matches!(v, V::Ref(h) if h != 0) {
                                self.classes.entry(owner).or_default().statics.insert(f, v);
                            }
                            v
                        }
                    }
                };
                set!(regs[0], v);
            }
            0x67..=0x6d => {
                let fl = ins.idx.unwrap_or(0);
                let f = self.canon_field(gi(fl));
                let owner =
                    self.canon(gi(self.dexes[di].field(fl).map_err(E::Vm)?.class_idx as u32));
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
                let args: Vec<V> = if ins.format == ovelo_dex::Format::F3rc {
                    // {vCCCC..+AA}: `regs`=[range start], `lit`=reg count
                    let (first, n) = (regs[0] as usize, ins.lit.unwrap_or(0) as usize);
                    (first..first + n)
                        .map(|i| Self::reg(fr, i))
                        .collect::<Rt<_>>()?
                } else {
                    regs.iter()
                        .map(|&i| Self::reg(fr, i as usize))
                        .collect::<Rt<_>>()?
                };
                let this_arg = args.first().copied();
                // Cheap gate first: only build the expensive arg dump
                // when the callee name actually matches the filter.
                let trace_hit = self
                    .trace_call
                    .as_deref()
                    .map(|p| {
                        let callee = self.describe_method(midx);
                        let caller = self.describe_method(fr.loc.midx);
                        p.split(',')
                            .any(|one| !one.is_empty() && callee.contains(one))
                            && self
                                .trace_from
                                .as_deref()
                                .is_none_or(|f| caller.contains(f))
                    })
                    .unwrap_or(false);
                let argfmt: Vec<String> = if trace_hit {
                    args.iter()
                        .skip(1)
                        .map(|v| match v {
                            V::Ref(h) if *h != 0 => {
                                let mut s = format!("h{h}:{}", self.dbg_class_of(*h));
                                if let Some(Obj::Array { data, .. }) = self.heap.get(*h) {
                                    s.push_str(&format!(";{}", data.len()));
                                }
                                if let Some(Obj::Host {
                                    state: HostState::List(l),
                                    ..
                                }) = self.heap.get(*h)
                                {
                                    s.push_str(&format!("(n={})", l.len()));
                                    let els: Vec<String> = l
                                        .iter()
                                        .take(12)
                                        .map(|e| match e {
                                            V::Ref(eh) => {
                                                format!("h{eh}:{}", self.dbg_class_of(*eh))
                                            }
                                            other => format!("{other:?}"),
                                        })
                                        .collect();
                                    s.push_str(&format!("[{}]", els.join(",")));
                                }
                                if let Some(Obj::Str(t)) = self.heap.get(*h) {
                                    s.push_str(&format!(
                                        "=\"{}\"",
                                        t.chars().take(80).collect::<String>()
                                    ));
                                }
                                if let Some(Obj::Class { type_idx }) = self.heap.get(*h) {
                                    if let Ok(d) = self.type_desc_of(*type_idx) {
                                        s.push_str(&format!("<{d}>"));
                                    }
                                }
                                s
                            }
                            V::Ref(_) => "null".into(),
                            other => format!("{other:?}"),
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                if let Some(p) = self.trace_call.as_deref() {
                    let callee = self.describe_method(midx);
                    let caller = self.describe_method(fr.loc.midx);
                    if p.split(',')
                        .any(|one| !one.is_empty() && callee.contains(one))
                        && self
                            .trace_from
                            .as_deref()
                            .is_none_or(|f| caller.contains(f))
                    {
                        eprintln!(
                            "[call>] {caller} +{pc:04x} -> {callee} this={} args={argfmt:?}",
                            match this_arg {
                                Some(V::Ref(0)) | None => "null".into(),
                                Some(V::Ref(h)) => format!("h{h}:{}", self.dbg_class_of(h)),
                                other => format!("{other:?}"),
                            }
                        );
                    }
                }
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
                fr.last_call = midx;
                if let Some(p) = self.trace_call.as_deref() {
                    let callee = self.describe_method(midx);
                    let caller = self.describe_method(fr.loc.midx);
                    if p.split(',')
                        .any(|one| !one.is_empty() && callee.contains(one))
                        && self
                            .trace_from
                            .as_deref()
                            .is_none_or(|f| caller.contains(f))
                    {
                        let thisfmt = match this_arg {
                            Some(V::Ref(h)) => match self.heap.get(h) {
                                Some(Obj::Str(t)) => {
                                    format!(
                                        "h{h}:String=\"{}\"",
                                        t.chars().take(80).collect::<String>()
                                    )
                                }
                                _ => format!("{:?}", this_arg.unwrap()),
                            },
                            _ => format!("{:?}", this_arg),
                        };
                        let resfmt = match res {
                            Some(V::Ref(h)) if h != 0 => match self.heap.get(h) {
                                Some(Obj::Str(t)) => {
                                    format!(
                                        "Some(h{h}:String=\"{}\")",
                                        t.chars().take(80).collect::<String>()
                                    )
                                }
                                _ => format!("{res:?}"),
                            },
                            _ => format!("{res:?}"),
                        };
                        eprintln!(
                            "[call] {} +{pc:04x} -> {} this={} args={argfmt:?} ({})",
                            caller, callee, thisfmt, resfmt
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

    /// Host placeholders have no ctor, so ref-typed fields that dex
    /// never explicitly wrote stay null; mint a stub of the field's
    /// declared type and remember it on the object.
    fn ref_field_stub(&mut self, h: u32, field_idx: GIdx) -> Option<V> {
        if !matches!(self.heap.get(h), Some(Obj::Host { .. })) {
            return None;
        }
        let d = self.dex_of(field_idx)?;
        let f = d.field(gl(field_idx)).ok()?;
        let ty = d.type_descriptor(f.type_idx as u32).ok()?;
        // Only eagerly-allocated value holders (AOSP ctors `new` these
        // up front — LayoutParams.mDecorInsets is the motivating case):
        // minting a stub for an ordinary ref field fakes `x != null`
        // and corrupts guest null checks (Message.obj, payloads).
        const REF_FIELD_STUB_TYPES: &[&str] = &[
            "Landroid/graphics/Rect;",
            "Landroid/graphics/RectF;",
            "Landroid/graphics/Point;",
            "Landroid/graphics/PointF;",
            "Landroid/graphics/Insets;",
            "Landroid/graphics/Region;",
        ];
        if !REF_FIELD_STUB_TYPES.contains(&ty.as_str()) {
            return None;
        }
        let v = jdk::host(self, &ty, HostState::Empty);
        if let Some(Obj::Host { fields, .. }) = self.heap.get_mut(h) {
            fields.insert(field_idx, v);
        }
        Some(v)
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
            let (name, ty) = self.dex_of(field).and_then(|d| {
                let f = d.field(gl(field)).ok()?;
                Some((
                    d.string(f.name_idx).ok()?,
                    d.type_descriptor(f.type_idx as u32).ok()?,
                ))
            })?;
            let cd = self.dex_of(cg)?.class_data_rc(def).ok()?;
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
        self.encoded_to_v(cg, &v)
    }

    fn aget(&mut self, h: u32, i: i32) -> Rt<V> {
        self.null_check(h, "aget")?;
        match self.heap.get(h) {
            Some(Obj::Array { data, .. }) => {
                let i = i as usize;
                if i >= data.len() {
                    return Err(
                        self.throw_builtin("Ljava/lang/ArrayIndexOutOfBoundsException;", "aget")
                    );
                }
                Ok(data[i])
            }
            _ => Err(E::Vm(errd("dexrt.type", format!("aget h{h}")))),
        }
    }

    fn aput(&mut self, h: u32, i: i32, v: V) -> Rt<()> {
        self.null_check(h, "aput")?;
        match self.heap.get_mut(h) {
            Some(Obj::Array { elem, data }) => {
                if i < 0 || i as usize >= data.len() {
                    let d = format!("aput idx {i} len {}", data.len());
                    return Err(
                        self.throw_builtin("Ljava/lang/ArrayIndexOutOfBoundsException;", &d)
                    );
                }
                // A `const/4 0` null written through aput-object must
                // read back as a reference — canonicalize to Ref(0) so a
                // stored null compares equal to the new-array default
                // (Arrays.equals, if-eq, Object[] of fresh nulls).
                let v =
                    if matches!(v, V::I32(0)) && (elem.starts_with('L') || elem.starts_with('[')) {
                        V::Ref(0)
                    } else {
                        v
                    };
                data[i as usize] = v;
                Ok(())
            }
            _ => Err(E::Vm(errd("dexrt.type", format!("aput h{h}")))),
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
            Some(Obj::Array { .. }) => Err(self.throw_builtin(
                "Ljava/lang/ArrayIndexOutOfBoundsException;",
                "fill-array-data",
            )),
            _ => Err(E::Vm(errd("dexrt.type", format!("fill-array h{h}")))),
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
            return Ok(Some((((raw[i + 1] as i32) << 16) | raw[i] as i32) as u32));
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
                return Ok(Some((((raw[ti + 1] as i32) << 16) | raw[ti] as i32) as u32));
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
                    return Err(
                        self.throw_builtin("Ljava/lang/ArithmeticException;", "divide by zero")
                    );
                }
                b.wrapping_div(lit)
            }
            0xd4 | 0xdc => {
                if lit == 0 {
                    return Err(
                        self.throw_builtin("Ljava/lang/ArithmeticException;", "rem by zero")
                    );
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
                    return Err(
                        self.throw_builtin("Ljava/lang/ArithmeticException;", "divide by zero")
                    );
                }
                V::I32(x.wrapping_div(y))
            }
            0x94 => {
                let (x, y) = (Self::i32v(b)?, Self::i32v(c)?);
                if y == 0 {
                    return Err(
                        self.throw_builtin("Ljava/lang/ArithmeticException;", "rem by zero")
                    );
                }
                V::I32(x.wrapping_rem(y))
            }
            0x95 => V::I32(Self::i32v(b)? & Self::i32v(c)?),
            0x96 => V::I32(Self::i32v(b)? | Self::i32v(c)?),
            0x97 => V::I32(Self::i32v(b)? ^ Self::i32v(c)?),
            0x98 => V::I32(Self::i32v(b)?.wrapping_shl(Self::i32v(c)? as u32 & 0x1f)),
            0x99 => V::I32(Self::i32v(b)?.wrapping_shr(Self::i32v(c)? as u32 & 0x1f)),
            0x9a => {
                V::I32((Self::i32v(b)? as u32).wrapping_shr(Self::i32v(c)? as u32 & 0x1f) as i32)
            }
            // long
            0x9b => V::I64(Self::i64v(b)?.wrapping_add(Self::i64v(c)?)),
            0x9c => V::I64(Self::i64v(b)?.wrapping_sub(Self::i64v(c)?)),
            0x9d => V::I64(Self::i64v(b)?.wrapping_mul(Self::i64v(c)?)),
            0x9e => {
                let (x, y) = (Self::i64v(b)?, Self::i64v(c)?);
                if y == 0 {
                    return Err(
                        self.throw_builtin("Ljava/lang/ArithmeticException;", "divide by zero")
                    );
                }
                V::I64(x.wrapping_div(y))
            }
            0x9f => {
                let (x, y) = (Self::i64v(b)?, Self::i64v(c)?);
                if y == 0 {
                    return Err(
                        self.throw_builtin("Ljava/lang/ArithmeticException;", "rem by zero")
                    );
                }
                V::I64(x.wrapping_rem(y))
            }
            0xa0 => V::I64(Self::i64v(b)? & Self::i64v(c)?),
            0xa1 => V::I64(Self::i64v(b)? | Self::i64v(c)?),
            0xa2 => V::I64(Self::i64v(b)? ^ Self::i64v(c)?),
            0xa3 => V::I64(Self::i64v(b)?.wrapping_shl(Self::i32v(c)? as u32 & 0x3f)),
            0xa4 => V::I64(Self::i64v(b)?.wrapping_shr(Self::i32v(c)? as u32 & 0x3f)),
            0xa5 => {
                V::I64((Self::i64v(b)? as u64).wrapping_shr(Self::i32v(c)? as u32 & 0x3f) as i64)
            }
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
        let mut m = dir(
            "f",
            0,
            vec![
                0x0012, // const/4 v0,0
                0x0093, 0x0000, // div-int v0,v0,v0
                0x000f, // return v0
                0x020d, // move-exception v2
                0xf112, // const/4 v1,-1
                0x010f, // return v1
            ],
        );
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
            methods: vec![dir(
                "f",
                0,
                vec![
                    0x1112, // const/4 v1,1
                    0x1023, 0x0000, // new-array v0,v1,type@I (patched)
                    0x2212, // const/4 v2,2
                    0x0044, 0x0200, // aget v0,v0,v2
                    0x000f,
                ],
            )],
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
            vec![dir(
                "main",
                0,
                vec![
                    0x0022, 0x0000, // new-instance v0,LC (patched)
                    0x106e, 0x0000, 0x0000, // invoke-virtual {v0},LB.who (patched)
                    0x010a, // move-result v1
                    0x010f, // return v1
                ],
            )],
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
                        0x1112, // const/4 v1,1
                        0x0169, 0x0000, // sput v1,LT.x (patched)
                        0x000e, // return-void
                    ],
                    tries: vec![],
                    lits: &[],
                },
                dir(
                    "main",
                    0,
                    vec![
                        0x0060, 0x0000, // sget v0,LT.x
                        0x0160, 0x0000, // sget v1,LT.x
                        0x0090, 0x0100, // add-int v0,v0,v1
                        0x000f,
                    ],
                ),
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
            vec![dir(
                "f",
                0,
                vec![
                    0x1012, // const/4 v0,1
                    0x002b, 0x0008, 0x0000, // packed-switch v0,+8 -> @9
                    0xa112, // const/4 v1,10
                    0x010f, // return v1
                    0x0113, 0x0014, // const/16 v1,20
                    0x010f, // return v1
                    0x0100, 0x0002, // payload: packed, size 2
                    0x0000, 0x0000, // first_key = 0
                    0x0003, 0x0000, // target[0] = +3 -> @4
                    0x0005, 0x0000, // target[1] = +5 -> @6
                ],
            )],
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
        let mut m = dir(
            "f",
            0,
            vec![
                0x0012, // const/4 v0,0
                0x0093, 0x0000, // div-int v0,v0,v0
                0x000f, // return v0
                0x020d, // move-exception v2
                0xf112, // const/4 v1,-1
                0x010f, // return v1
            ],
        );
        m.tries = vec![(1, 2, vec![], Some(4))];
        let (b, _ix) = build(&[cls("LT;", vec![m])]);
        let mut v = vm(b);
        assert_eq!(v.enable_aot(), 0);
        assert_eq!(v.run("LT;", "f", &[]).unwrap(), Some(V::I32(-1)));
    }
}

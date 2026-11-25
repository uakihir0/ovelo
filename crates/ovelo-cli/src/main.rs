//! ovelo CLI: `inspect` / `install` / `run` / `diagnose` (design doc §12).
//!
//! Exit codes: 0 ok, 1 runtime error (OveloError printed as
//! `CATEGORY [phase] cause`), 2 usage.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use ovelo_apk::install::{AppId, InstallRecord, InstallStore};
use ovelo_apk::zip::{parse_central_directory, read_entry, ZipLimits};
use ovelo_apk::{inspect_apk, install_split_set, ApkInfo, DeviceProfile};
use ovelo_dex::DexFile;
use ovelo_error::{ErrorCategory, OveloError, Phase};

fn err(cause: &str) -> OveloError {
    OveloError::new(ErrorCategory::PkgInvalid, Phase::Install, cause)
}

/// The default install store root: `~/.ovelo/store`.
fn store_root() -> Result<PathBuf, OveloError> {
    let home = std::env::var_os("HOME").ok_or_else(|| err("cli.home.missing"))?;
    Ok(PathBuf::from(home).join(".ovelo").join("store"))
}

fn default_store() -> Result<InstallStore, OveloError> {
    InstallStore::open(store_root()?)
}

/// Canonical signer fingerprint for `bytes`, or `""` when unsigned
/// (mirrors `install_apk`'s multi-signer join).
fn apk_signer(bytes: &[u8]) -> String {
    match ovelo_apk::verify_v2(bytes) {
        Ok(signers) => {
            let mut h: Vec<&str> = signers.iter().map(|s| s.cert_sha256.as_str()).collect();
            h.sort_unstable();
            h.join("+")
        }
        Err(_) => String::new(),
    }
}

/// Append a ledger row for `info` under the default store (§14).
fn ledger_record(
    info: &ApkInfo,
    signer: &str,
    backend: &str,
    scenario: &str,
    result: &str,
    detail: &str,
    announce: bool,
) {
    let rec = ledger::LedgerRecord {
        fixture_sha256: info.sha256.clone(),
        package: info.manifest.package.clone(),
        version_code: info.manifest.version_code,
        target_sdk: info.manifest.target_sdk,
        signer: signer.to_string(),
        os: ledger::host_os(),
        backend: backend.to_string(),
        scenario: scenario.to_string(),
        result: result.to_string(),
        detail: detail.to_string(),
        runtime_version: env!("CARGO_PKG_VERSION").to_string(),
    };
    match store_root().map(|r| ledger::Ledger::at(&r)).and_then(|l| {
        l.record(&rec).map_err(|e| {
            eprintln!("ovelo: ledger write failed: {e}");
            e
        })
    }) {
        Ok(p) if announce => println!("ledger    {}", p.display()),
        _ => {}
    }
}

mod gate;
mod ledger;
mod worker;

/// Private malloc zone for Rust allocations (macOS): guest `.so`
/// code can write wildly through bad pointers; when that damage
/// lands in the *default* zone's freelists the next `Vec`/`HashMap`
/// growth segfaults inside `malloc`. Parking Rust's heap in its own
/// zone both hardens the runtime and narrows diagnosis — if crashes
/// stop, the corruptor targets default-zone pages specifically.
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
mod rustzone {
    use std::alloc::{GlobalAlloc, Layout};
    use std::ffi::c_void;
    use std::sync::OnceLock;

    unsafe extern "C" {
        fn malloc_create_zone(start_size: usize, flags: u32) -> *mut c_void;
        fn malloc_zone_malloc(zone: *mut c_void, size: usize) -> *mut c_void;
        fn malloc_zone_realloc(zone: *mut c_void, ptr: *mut c_void, size: usize) -> *mut c_void;
        fn malloc_zone_free(zone: *mut c_void, ptr: *mut c_void);
        fn malloc_zone_memalign(zone: *mut c_void, align: usize, size: usize) -> *mut c_void;
        fn malloc(size: usize) -> *mut c_void;
        fn realloc(ptr: *mut c_void, size: usize) -> *mut c_void;
        fn free(ptr: *mut c_void);
        fn posix_memalign(memptr: *mut *mut c_void, align: usize, size: usize) -> i32;
    }

    /// Reentrancy-safe env flag read: OnceLock::get_or_init would
    /// deadlock — env::var_os allocates, and its temporaries' dealloc
    /// re-enters this flag check from inside the allocator. A tri-state
    /// atomic (0 unset, 1 probing, 2 on, 3 off) defaults to `false`
    /// while another thread probes.
    fn env_flag(name: &str, cell: &std::sync::atomic::AtomicU8) -> bool {
        use std::sync::atomic::Ordering::Relaxed;
        match cell.load(Relaxed) {
            2 => true,
            3 => false,
            _ => {
                if cell.compare_exchange(0, 1, Relaxed, Relaxed).is_ok() {
                    let v = std::env::var_os(name).is_some();
                    cell.store(if v { 2 } else { 3 }, Relaxed);
                    v
                } else {
                    false
                }
            }
        }
    }

    /// OVELO_NOZONE bypasses the private zone so GuardMalloc
    /// (`DYLD_INSERT_LIBRARIES=/usr/lib/libgmalloc.dylib`) can page-guard
    /// every Rust allocation and catch the corrupting write red-handed.
    fn nozone() -> bool {
        static S: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);
        env_flag("OVELO_NOZONE", &S)
    }

    /// OVELO_LEAK: never return memory to the zone — freed chunks are
    /// where a stray write gets picked up as a "freelist node"; if the
    /// crash signature changes/disappears, the corruptor's damage lives
    /// in free-chunk metadata rather than live allocations.
    fn leak() -> bool {
        static S: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);
        env_flag("OVELO_LEAK", &S)
    }

    fn zone() -> *mut c_void {
        static Z: OnceLock<usize> = OnceLock::new();
        *Z.get_or_init(|| unsafe { malloc_create_zone(0, 0) as usize }) as *mut c_void
    }

    /// Publish the zone pointer for dexrt's OVELO_ZONECHECK tagging.
    pub fn publish() {
        ovelo_dexrt::RUST_ZONE.store(zone() as usize, std::sync::atomic::Ordering::Relaxed);
    }

    pub struct ZoneAlloc;
    unsafe impl GlobalAlloc for ZoneAlloc {
        unsafe fn alloc(&self, l: Layout) -> *mut u8 {
            if nozone() {
                if l.align() > 16 {
                    let mut p: *mut c_void = std::ptr::null_mut();
                    if posix_memalign(&mut p, l.align(), l.size()) == 0 {
                        return p as *mut u8;
                    }
                    return std::ptr::null_mut();
                }
                return malloc(l.size()) as *mut u8;
            }
            if l.align() > 16 {
                malloc_zone_memalign(zone(), l.align(), l.size()) as *mut u8
            } else {
                malloc_zone_malloc(zone(), l.size()) as *mut u8
            }
        }
        unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
            // Route through `alloc` so >16-byte alignments are honored
            // in every mode (calloc/malloc_zone_calloc cap at 16).
            let p = self.alloc(l);
            if !p.is_null() {
                std::ptr::write_bytes(p, 0, l.size());
            }
            p
        }
        unsafe fn dealloc(&self, p: *mut u8, _l: Layout) {
            if leak() {
                return;
            }
            if nozone() {
                return free(p as *mut c_void);
            }
            malloc_zone_free(zone(), p as *mut c_void)
        }
        unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
            if leak() {
                let np = self.alloc(Layout::from_size_align_unchecked(n, l.align()));
                if !np.is_null() {
                    std::ptr::copy_nonoverlapping(p, np, l.size().min(n));
                }
                return np;
            }
            if l.align() > 16 {
                // libc realloc doesn't preserve over-aligned storage —
                // move through alloc so the new block honors `l.align()`.
                let np = self.alloc(Layout::from_size_align_unchecked(n, l.align()));
                if !np.is_null() {
                    std::ptr::copy_nonoverlapping(p, np, l.size().min(n));
                    self.dealloc(p, l);
                }
                return np;
            }
            if nozone() {
                // realloc (not reallocf): on failure the old block must
                // stay live — reallocf frees it and Rust would touch or
                // double-free a dead pointer on the error path.
                return realloc(p as *mut c_void, n) as *mut u8;
            }
            malloc_zone_realloc(zone(), p as *mut c_void, n) as *mut u8
        }
    }

    #[global_allocator]
    static A: ZoneAlloc = ZoneAlloc;
}

enum Cmd {
    Inspect {
        apk: PathBuf,
        json: bool,
    },
    Install {
        apks: Vec<PathBuf>,
        json: bool,
    },
    Run {
        app: String,
    },
    Diagnose {
        apk: PathBuf,
        json: bool,
    },
    Gate {
        path: PathBuf,
    },
    /// Disassemble a dex method for debugging (`apk class.method?`).
    Disas {
        apk: PathBuf,
        class: String,
        method: Option<String>,
    },
    Help,
}

fn parse_args(argv: &[String]) -> Result<Cmd, OveloError> {
    let sub = argv.get(1).map(|s| s.as_str()).unwrap_or("help");
    // `--` ends option parsing so `-foo.apk` filenames still work —
    // flags after it are positional args, not options.
    let mut json = false;
    let mut pos: Vec<&String> = Vec::new();
    let mut seen_sep = false;
    let mut unknown_flag = false;
    for a in argv.iter().skip(2) {
        if !seen_sep && a == "--" {
            seen_sep = true;
        } else if !seen_sep && a == "--json" {
            json = true;
        } else if !seen_sep && a.starts_with('-') {
            unknown_flag = true;
        } else {
            pos.push(a);
        }
    }
    // `install` accepts a split set (base + config.* APKs); every other
    // subcommand takes exactly one positional arg.
    let (min_args, max_args) = match sub {
        "install" => (1, usize::MAX),
        "disas" => (2, 3),
        _ => (1, 1),
    };
    if sub != "help"
        && sub != "--help"
        && sub != "-h"
        && (unknown_flag || pos.len() < min_args || pos.len() > max_args)
    {
        return Err(err("cli.usage"));
    }
    let arg = pos.first().copied();
    match (sub, arg) {
        ("inspect", Some(p)) => Ok(Cmd::Inspect {
            apk: p.into(),
            json,
        }),
        ("install", Some(_)) => Ok(Cmd::Install {
            apks: pos.iter().map(|p| PathBuf::from(*p)).collect(),
            json,
        }),
        ("run", Some(a)) => Ok(Cmd::Run { app: a.clone() }),
        ("diagnose", Some(p)) => Ok(Cmd::Diagnose {
            apk: p.into(),
            json,
        }),
        ("gate", Some(p)) => Ok(Cmd::Gate { path: p.into() }),
        ("disas", Some(a)) => Ok(Cmd::Disas {
            apk: a.into(),
            class: pos.get(1).map(|s| s.to_string()).unwrap_or_default(),
            method: pos.get(2).map(|s| s.to_string()),
        }),
        ("help", _) | ("--help", _) | ("-h", _) => Ok(Cmd::Help),
        _ => Err(err("cli.usage")),
    }
}

const USAGE: &str = "ovelo — Android app compatibility runtime for macOS

USAGE:
    ovelo inspect  <apk> [--json]   manifest/features/ABIs/dex inventory
    ovelo install  <apk> [splits.apk ...] [--json]
                                  verify signature(s) + commit to the store
    ovelo run      <package>        launch the installed app
    ovelo diagnose <apk>  [--json]  compatibility report (+ ledger record)
    ovelo gate     <binary|.app>    distribution gate: sign/entitlements/notarization
";

fn read_apk(p: &Path) -> Result<Vec<u8>, OveloError> {
    std::fs::read(p).map_err(|e| err("cli.apk.read").with_detail(format!("{}: {e}", p.display())))
}

/// Escape control characters before printing untrusted APK fields to a
/// terminal — a crafted versionName must not be able to inject escape
/// sequences and alter what the user sees.
fn disp(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_control() {
                format!("\\u{{{:x}}}", c as u32)
            } else {
                c.to_string()
            }
        })
        .collect()
}

/// `{:?}`-formatted JSON for an `Option` (`null` or the value).
fn jopt<T: std::fmt::Debug>(o: &Option<T>) -> String {
    match o {
        Some(v) => format!("{v:?}"),
        None => "null".to_string(),
    }
}

fn print_inspect(info: &ApkInfo, json: bool) {
    let m = &info.manifest;
    if json {
        println!("{{");
        println!("  \"package\": {:?},", m.package);
        println!("  \"versionCode\": {},", m.version_code);
        println!("  \"versionName\": {},", jopt(&m.version_name));
        println!("  \"minSdk\": {},", jopt(&m.min_sdk));
        println!("  \"targetSdk\": {},", jopt(&m.target_sdk));
        println!("  \"activities\": {:?},", m.activities);
        println!("  \"features\": {:?},", m.features);
        println!("  \"permissions\": {:?},", m.permissions);
        println!("  \"abis\": {:?},", info.abis);
        println!("  \"dexEntries\": {:?},", info.dex_entries);
        println!("  \"hasResources\": {},", info.has_resources);
        println!("  \"signedV2OrV3\": {},", info.signed_v2_or_v3);
        println!("  \"sha256\": {:?}", info.sha256);
        println!("}}");
    } else {
        println!("package       {}", disp(&m.package));
        println!(
            "version       {} ({})",
            m.version_code,
            m.version_name
                .as_deref()
                .map(disp)
                .unwrap_or_else(|| "-".into())
        );
        println!(
            "minSdk        {}",
            m.min_sdk
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".into())
        );
        println!(
            "targetSdk     {}",
            m.target_sdk
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".into())
        );
        println!("activities    {}", disp(&m.activities.join(", ")));
        println!("features      {}", disp(&m.features.join(", ")));
        println!("permissions   {}", disp(&m.permissions.join(", ")));
        println!("abis          {}", info.abis.join(", "));
        println!("dex           {}", info.dex_entries.join(", "));
        println!(
            "resources     {}",
            if info.has_resources { "yes" } else { "no" }
        );
        println!(
            "signed v2/v3  {}",
            if info.signed_v2_or_v3 { "yes" } else { "no" }
        );
        println!("sha256        {}", info.sha256);
    }
}

/// Per-dex verification result for the diagnose ledger.
struct DexReport {
    name: String,
    ok: bool,
    cause: String,
}

fn verify_dexes(bytes: &[u8], info: &ApkInfo) -> Result<Vec<DexReport>, OveloError> {
    let (_cd, entries) = parse_central_directory(bytes, &ZipLimits::default())?;
    let mut out = Vec::new();
    for d in &info.dex_entries {
        let e = entries
            .iter()
            .find(|e| &e.name == d)
            .ok_or_else(|| err("cli.dex.missing"))?;
        let raw = read_entry(bytes, e)?;
        let r = DexFile::open(raw).and_then(|f| f.verify());
        out.push(DexReport {
            name: d.clone(),
            ok: r.is_ok(),
            cause: r.err().map(|e| e.cause().to_string()).unwrap_or_default(),
        });
    }
    Ok(out)
}

/// Disassemble the methods of `class` (descriptor like
/// `Lpkg/Foo;`) in the APK's dexes — debugging aid for guest crashes.
fn disas(apk: &[u8], class: &str, method: Option<&str>) -> Result<(), OveloError> {
    let (_cd, entries) = parse_central_directory(apk, &ZipLimits::default())?;
    for e in entries
        .iter()
        .filter(|e| e.name.starts_with("classes") && e.name.ends_with(".dex"))
    {
        let dex = DexFile::open(read_entry(apk, e)?)?;
        for (class_idx, em) in dex.methods()? {
            if dex.type_descriptor(class_idx).ok().as_deref() != Some(class) {
                continue;
            }
            let mid = dex.method(em.method_idx)?;
            let name = dex.string(mid.name_idx)?;
            if let Some(want) = method {
                if name != want {
                    continue;
                }
            }
            let proto = dex
                .proto(mid.proto_idx as u32)
                .and_then(|p| {
                    let ps = dex
                        .proto_params(p)?
                        .iter()
                        .filter_map(|&t| dex.type_descriptor(t as u32).ok())
                        .collect::<Vec<_>>()
                        .join("");
                    let ret = dex
                        .type_descriptor(p.return_type_idx)
                        .unwrap_or_else(|_| "?".into());
                    Ok(format!("({ps}){ret}"))
                })
                .unwrap_or_else(|_| "(?)".into());
            println!(
                "{class}.{name}{proto} [#{midx} {e_name}]",
                midx = em.method_idx,
                e_name = e.name
            );
            let Some(code) = dex.code_item(em.code_off)? else {
                continue;
            };
            for i in ovelo_dex::decode_all(&code.insns)? {
                if i.is_payload() {
                    continue;
                }
                let mut line = format!("  {:04x} {:<22}", i.offset, i.name);
                if !i.regs.is_empty() {
                    line.push_str(&format!(" v{:?}", i.regs));
                }
                if let Some(idx) = i.idx {
                    line.push_str(&format!(" #{} {}", idx, resolve_idx(&dex, i.opcode, idx)));
                }
                if let Some(lit) = i.lit {
                    if i.idx.is_none() {
                        line.push_str(&format!(" {lit}"));
                    }
                }
                if let Some(t) = i.target {
                    line.push_str(&format!(" -> {:04x}", t));
                }
                println!("{line}");
            }
        }
    }
    Ok(())
}

/// Best-effort operand resolution for a dex table index.
fn resolve_idx(dex: &DexFile, op: u8, idx: u32) -> String {
    let r = match op {
        // const-string
        0x1a | 0x1b => dex.string(idx).map(|s| format!("\"{s}\"")),
        // type-table ops (const-class, cast, instanceof, new, arrays)
        0x1c | 0x1f..=0x26 => dex.type_descriptor(idx),
        // instance + static fields
        0x52..=0x6d => dex.field(idx).and_then(|f| {
            Ok(format!(
                "{}.{}:{}",
                dex.type_descriptor(f.class_idx as u32)?,
                dex.string(f.name_idx)?,
                dex.type_descriptor(f.type_idx as u32)?
            ))
        }),
        // invokes
        0x6e..=0x78 | 0xf0..=0xf2 => dex.method(idx).and_then(|m| {
            Ok(format!(
                "{}.{}",
                dex.type_descriptor(m.class_idx as u32)?,
                dex.string(m.name_idx)?
            ))
        }),
        _ => return String::new(),
    };
    r.unwrap_or_else(|e| format!("<{e}>"))
}

/// Find an installed record for `package` (any signer) in `store`.
fn find_record(store: &InstallStore, package: &str) -> Result<InstallRecord, OveloError> {
    let apps = store.root().join("apps");
    if let Ok(rd) = std::fs::read_dir(&apps) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if let Some((pkg, signer)) = name.split_once('@') {
                if pkg == package {
                    let app = AppId {
                        package: pkg.to_string(),
                        signer: signer.to_string(),
                    };
                    if let Some(rec) = store.current(&app)? {
                        return Ok(rec);
                    }
                }
            }
        }
    }
    Err(err("cli.app.not_installed").with_detail(package))
}

/// Host overrides on dex-defined library methods that can't run in the
/// runtime — appcompat's vector-drawable inflation, whose full path
/// needs a working XmlResourceParser + Drawable backend we don't have
/// yet. `getDrawable` answers a placeholder so menu/icon inflation
/// keeps going.
fn register_compat_shims(
    v: &mut ovelo_dexrt::Vm,
    initializers: &[String],
    components: std::rc::Rc<std::cell::RefCell<std::collections::HashMap<String, ovelo_dexrt::V>>>,
    provider_ran: std::rc::Rc<std::cell::Cell<bool>>,
    manifest: &ovelo_apk::manifest::ManifestInfo,
) {
    use ovelo_dexrt::{HostState, Obj, V};
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;

    // androidx menus (MenuBuilder/SubMenuBuilder/MenuInflater) share
    // the host menu model — see Vm::register_menu_overrides.
    v.register_menu_overrides();

    let drawable = |vm: &mut ovelo_dexrt::Vm, res_id: i64| -> V {
        if res_id > 0 {
            // Real pixels when the resource is a vector or bitmap;
            // a bare Drawable placeholder otherwise.
            return vm.drawable_for_res(res_id as u32);
        }
        V::Ref(vm.heap.alloc(Obj::Host {
            class_idx: u64::MAX,
            desc: "Landroid/graphics/drawable/Drawable;".to_string(),
            fields: HashMap::new(),
            state: HostState::Empty,
        }))
    };
    for (class, name, sig, arg_idx) in [
        // Instance method — compact args put the receiver first, so
        // the res id sits at index 2.
        (
            "Landroidx/appcompat/widget/ResourceManagerInternal;",
            "getDrawable",
            "(Landroid/content/Context;I)Landroid/graphics/drawable/Drawable;",
            2usize,
        ),
        (
            "Landroidx/appcompat/widget/ResourceManagerInternal;",
            "getDrawable",
            "(Landroid/content/Context;IZ)Landroid/graphics/drawable/Drawable;",
            2,
        ),
        (
            "Landroidx/appcompat/content/res/AppCompatResources;",
            "getDrawable",
            "(Landroid/content/Context;I)Landroid/graphics/drawable/Drawable;",
            1,
        ),
        (
            "Landroidx/appcompat/widget/TintTypedArray;",
            "getDrawable",
            "(I)Landroid/graphics/drawable/Drawable;",
            1,
        ),
        (
            "Landroidx/appcompat/widget/TintTypedArray;",
            "getDrawableIfKnown",
            "(I)Landroid/graphics/drawable/Drawable;",
            1,
        ),
    ] {
        let _ = v.register_host(
            class,
            name,
            sig,
            Rc::new(RefCell::new(move |vm: &mut ovelo_dexrt::Vm, a: &[V]| {
                let res_id = match a.get(arg_idx).copied().unwrap_or(V::I32(0)) {
                    V::I32(i) => i as i64,
                    V::I64(i) => i,
                    _ => 0,
                };
                // TintTypedArray's arg is a styleable index into the wrapped
                // TypedArray — resolve it there for real drawable pixels.
                if class.contains("TintTypedArray") {
                    let ta_h = match a.first() {
                        Some(V::Ref(h)) => *h,
                        _ => 0,
                    };
                    let inner = vm
                        .get_object_field(
                            ta_h,
                            "Landroidx/appcompat/widget/TintTypedArray;",
                            "mWrapped",
                        )
                        .and_then(|v| match v {
                            V::Ref(h) if h != 0 => Some(h),
                            _ => None,
                        })
                        .unwrap_or(ta_h);
                    if let Some(d) = vm.typed_array_drawable(inner, res_id as usize) {
                        return Ok(Some(d));
                    }
                    return Ok(Some(V::Ref(vm.heap.alloc(Obj::Host {
                        class_idx: u64::MAX,
                        desc: "Landroid/graphics/drawable/Drawable;".to_string(),
                        fields: HashMap::new(),
                        state: HostState::Empty,
                    }))));
                }
                Ok(Some(drawable(vm, res_id)))
            })) as ovelo_dexrt::HostCall,
        );
    }
    // kotlinx-coroutines: only the Android main dispatcher needs a host
    // shim — `MainDispatcherLoader` discovers it through ServiceLoader
    // (empty here) and otherwise manufactures a throwing placeholder.
    // `Dispatchers.getMain()` then stores this singleton in dex statics,
    // while Default/IO/Unconfined keep their real dex schedulers.
    let _ = v.register_host(
        "Lkotlinx/coroutines/internal/MainDispatcherLoader;",
        "loadMainDispatcher",
        "()Lkotlinx/coroutines/MainCoroutineDispatcher;",
        Rc::new(RefCell::new(move |vm: &mut ovelo_dexrt::Vm, _a: &[V]| {
            Ok(Some(V::Ref(vm.heap.alloc(Obj::Host {
                class_idx: u64::MAX,
                desc: "Lkotlinx/coroutines/MainCoroutineDispatcher;".to_string(),
                fields: HashMap::new(),
                state: HostState::Empty,
            }))))
        })) as ovelo_dexrt::HostCall,
    );
    // Compose 1.9 graphics layers normally record into RenderNode and
    // then replay that display list. Our Canvas backend is software —
    // declaring software support makes GraphicsLayer.draw invoke the
    // recorded drawBlock directly instead, preserving content without
    // implementing the Android display-list object graph.
    for owner in [
        "Landroidx/compose/ui/graphics/layer/GraphicsLayerImpl;",
        "Landroidx/compose/ui/graphics/layer/GraphicsViewLayer;",
    ] {
        let _ = v.register_host(
            owner,
            "getSupportsSoftwareRendering",
            "()Z",
            Rc::new(RefCell::new(|_vm: &mut ovelo_dexrt::Vm, _a: &[V]| {
                Ok(Some(V::I32(1)))
            })) as ovelo_dexrt::HostCall,
        );
    }
    // androidx.profileinstaller: `ProfileInstallerInitializer` schedules
    // `writeProfile` on the first doFrame — under interpretation that
    // write churns for minutes while the window never opens. The
    // baseline profile is advisory (a performance hint, not behavior),
    // so the call is a no-op here.
    let _ = v.register_host(
        "Landroidx/profileinstaller/ProfileInstaller;",
        "writeProfile",
        "(Landroid/content/Context;Ljava/util/concurrent/Executor;Landroidx/profileinstaller/ProfileInstaller$DiagnosticsCallback;Z)V",
        Rc::new(RefCell::new(|_vm: &mut ovelo_dexrt::Vm, _a: &[V]| Ok(None)))
            as ovelo_dexrt::HostCall,
    );
    // androidx.startup: `initializeComponent` enforces that the
    // initializer was discovered via manifest meta-data — our runtime
    // runs discovered initializers itself (see startup_initializers),
    // so the check is satisfied by construction; make it a no-op.
    for sig in [
        "(Ljava/lang/Class;)Ljava/lang/Object;",
        "(Ljava/lang/Class;Ljava/util/Set;)Ljava/lang/Object;",
    ] {
        let components2 = components.clone();
        let provider_ran2 = provider_ran.clone();
        let _ = v.register_host(
            "Landroidx/startup/AppInitializer;",
            "initializeComponent",
            sig,
            Rc::new(RefCell::new(move |vm: &mut ovelo_dexrt::Vm, a: &[V]| {
                // The real provider already ran discoverAndInitialize:
                // delegate to the dex body so the guest's own mInitialized
                // cache answers — never a second create().
                if provider_ran2.get() {
                    if let Some(V::Ref(ai)) = a.first().copied() {
                        if let Ok(r) = vm.call_virtual_direct(ai, "initializeComponent", &a[1..]) {
                            return Ok(r);
                        }
                    }
                }
                // Answer the component instance the caller expects:
                // instantiate the Initializer named by the Class argument
                // and return its `create(context)` result, the way AOSP's
                // AppInitializer.initializeComponent does. Results are
                // cached — AOSP returns the same component per class and
                // never runs create twice.
                let desc = match a.get(1).copied() {
                    Some(V::Ref(h)) => vm.class_descriptor_of(h),
                    _ => None,
                };
                if let Some(d) = desc {
                    if let Some(v) = components2.borrow().get(&d).copied() {
                        return Ok(Some(v));
                    }
                    if let Ok(ih) = vm.instantiate(&d) {
                        let ctx = match a.first().copied() {
                            Some(V::Ref(ai)) => vm
                                .get_object_field(
                                    ai,
                                    "Landroidx/startup/AppInitializer;",
                                    "mContext",
                                )
                                .unwrap_or(V::Ref(0)),
                            _ => V::Ref(0),
                        };
                        if let Ok(Some(v)) = vm.call_virtual(ih, "create", &[ctx]) {
                            components2.borrow_mut().insert(d, v);
                            return Ok(Some(v));
                        }
                        return Ok(Some(V::Ref(ih)));
                    }
                }
                Ok(Some(V::Ref(0)))
            })) as ovelo_dexrt::HostCall,
        );
    }
    // `AppInitializer.getInstance(context).mDiscovered` holds the
    // Class mirrors of manifest-discovered initializers — lazily
    // invoked initializers (ProcessLifecycleInitializer) check
    // membership before initializing.
    let discovered: Vec<String> = initializers
        .iter()
        .map(|i| format!("L{};", i.replace('.', "/")))
        .collect();
    let _ = v.register_host(
        "Landroidx/startup/AppInitializer;",
        "getInstance",
        "(Landroid/content/Context;)Landroidx/startup/AppInitializer;",
        Rc::new(RefCell::new(move |vm: &mut ovelo_dexrt::Vm, a: &[V]| {
            // Provider ran: return the guest's own singleton — its
            // mDiscovered/mInitialized carry the discovered state.
            if provider_ran.get() {
                if let Some(V::Ref(h)) =
                    vm.get_static_named("Landroidx/startup/AppInitializer;", "sInstance")
                {
                    if h != 0 {
                        return Ok(Some(V::Ref(h)));
                    }
                }
            }
            let set = discovered
                .iter()
                .map(|d| V::Ref(vm.class_handle(d)))
                .collect::<Vec<_>>();
            let hs = V::Ref(vm.heap.alloc(Obj::Host {
                class_idx: u64::MAX,
                desc: "Ljava/util/HashSet;".to_string(),
                fields: HashMap::new(),
                state: HostState::List(set),
            }));
            let ai = V::Ref(vm.heap.alloc(Obj::Host {
                class_idx: u64::MAX,
                desc: "Landroidx/startup/AppInitializer;".to_string(),
                fields: HashMap::new(),
                state: HostState::Empty,
            }));
            if let V::Ref(h) = ai {
                vm.set_object_field(h, "Landroidx/startup/AppInitializer;", "mDiscovered", hs);
                // `doInitialize(class, set)` reads mContext for the
                // initializer's create() argument.
                vm.set_object_field(
                    h,
                    "Landroidx/startup/AppInitializer;",
                    "mContext",
                    a.first().copied().unwrap_or(V::Ref(0)),
                );
                let init = V::Ref(vm.heap.alloc(Obj::Host {
                    class_idx: u64::MAX,
                    desc: "Ljava/util/HashMap;".to_string(),
                    fields: HashMap::new(),
                    state: HostState::Map(vec![]),
                }));
                vm.set_object_field(h, "Landroidx/startup/AppInitializer;", "mInitialized", init);
            }
            Ok(Some(ai))
        })) as ovelo_dexrt::HostCall,
    );
    // androidx.startup's `InitializationProvider.onCreate` reads
    // `getProviderInfo(component, GET_META_DATA).metaData` — the
    // manifest <meta-data> bundle mapping each initializer's class
    // name to "androidx.startup". The builtin arm hands back an
    // empty ProviderInfo, so attach the real manifest data here;
    // then the provider's own `discoverAndInitialize` populates
    // the (obfuscated) AppInitializer fields genuinely.
    let meta_names: Vec<String> = initializers.to_vec();
    let _ = v.register_host(
        "Landroid/content/pm/PackageManager;",
        "getProviderInfo",
        "(Landroid/content/ComponentName;I)Landroid/content/pm/ProviderInfo;",
        Rc::new(RefCell::new(move |vm: &mut ovelo_dexrt::Vm, a: &[V]| {
            // Only the startup provider gets the meta-data bundle —
            // read the requested ComponentName's mClass so other
            // providers don't see android.startup data.
            let wants_startup = match a.get(1).copied() {
                Some(V::Ref(cn)) => {
                    let cls = vm
                        .get_object_field(cn, "Landroid/content/ComponentName;", "mClass")
                        .and_then(|v| match v {
                            V::Ref(h) => vm.heap.str_val(h).map(|t| t.to_string()),
                            _ => None,
                        })
                        .unwrap_or_default();
                    cls.is_empty() || cls.ends_with("InitializationProvider")
                }
                _ => false,
            };
            if !wants_startup {
                return Ok(Some(V::Ref(vm.heap.alloc(Obj::Host {
                    class_idx: u64::MAX,
                    desc: "Landroid/content/pm/ProviderInfo;".to_string(),
                    fields: HashMap::new(),
                    state: HostState::Empty,
                }))));
            }
            let pairs: Vec<(ovelo_dexrt::V, ovelo_dexrt::V)> = meta_names
                .iter()
                .map(|n| {
                    (
                        V::Ref(vm.heap.intern_str(n)),
                        V::Ref(vm.heap.intern_str("androidx.startup")),
                    )
                })
                .collect();
            let bundle = V::Ref(vm.heap.alloc(Obj::Host {
                class_idx: u64::MAX,
                desc: "Landroid/os/Bundle;".to_string(),
                fields: HashMap::new(),
                state: HostState::Map(pairs),
            }));
            let pi = V::Ref(vm.heap.alloc(Obj::Host {
                class_idx: u64::MAX,
                desc: "Landroid/content/pm/ProviderInfo;".to_string(),
                fields: HashMap::new(),
                state: HostState::Empty,
            }));
            if let (V::Ref(h), V::Ref(b)) = (pi, bundle) {
                ovelo_dexrt::pseudo_set(vm, h, ovelo_dexrt::P_META, V::Ref(b));
            }
            Ok(Some(pi))
        })) as ovelo_dexrt::HostCall,
    );

    // Manifest-declared components: libraries that resolve components
    // through PackageManager — media3's SessionToken finds the app's
    // MediaBrowserService via queryIntentServices/resolveService and
    // reads ResolveInfo.serviceInfo — get real ResolveInfo objects
    // instead of the builtin's empty list. An Intent naming a specific
    // component filters to that class; no component means "all".
    // The Intent's target class: the component lives in P_COMPONENT
    // (setComponent/setClass/ctor) as a ComponentName, a Class mirror
    // or a bare class-name string; ComponentName keeps it in P_DATA.
    fn intent_class(vm: &ovelo_dexrt::Vm, v: Option<ovelo_dexrt::V>) -> Option<String> {
        let V::Ref(ih) = v? else {
            return None;
        };
        let V::Ref(ch) = ovelo_dexrt::pseudo_get(vm, ih, ovelo_dexrt::P_COMPONENT)? else {
            return None;
        };
        if let Some(t) = vm.heap.str_val(ch).map(|t| t.to_string()) {
            return Some(t);
        }
        if let Some(V::Ref(sh)) = ovelo_dexrt::pseudo_get(vm, ch, ovelo_dexrt::P_DATA) {
            if let Some(t) = vm.heap.str_val(sh).map(|t| t.to_string()) {
                return Some(t);
            }
        }
        vm.class_descriptor_of(ch).map(|d| {
            d.trim_start_matches('L')
                .trim_end_matches(';')
                .replace('/', ".")
        })
    }
    fn mint_info(
        vm: &mut ovelo_dexrt::Vm,
        info_desc: &str,
        name: &str,
        pkg: &str,
    ) -> ovelo_dexrt::V {
        let info = V::Ref(vm.heap.alloc(Obj::Host {
            class_idx: u64::MAX,
            desc: info_desc.to_string(),
            fields: HashMap::new(),
            state: HostState::Empty,
        }));
        if let V::Ref(h) = info {
            let n = V::Ref(vm.heap.intern_str(name));
            let p = V::Ref(vm.heap.intern_str(pkg));
            ovelo_dexrt::pseudo_set(vm, h, ovelo_dexrt::P_DATA, n);
            ovelo_dexrt::pseudo_set(vm, h, ovelo_dexrt::P_COMPONENT, p);
            // ComponentInfo.applicationInfo — guests read
            // `activityInfo.applicationInfo.packageName` on query
            // results (fossify home's launcher list).
            let ai = V::Ref(vm.heap.alloc(Obj::Host {
                class_idx: u64::MAX,
                desc: "Landroid/content/pm/ApplicationInfo;".to_string(),
                fields: HashMap::new(),
                state: HostState::Empty,
            }));
            if let V::Ref(ah) = ai {
                ovelo_dexrt::pseudo_set(vm, ah, ovelo_dexrt::P_COMPONENT, p);
            }
            ovelo_dexrt::pseudo_set(vm, h, ovelo_dexrt::P_APPINFO, ai);
        }
        info
    }
    fn mint_ri(vm: &mut ovelo_dexrt::Vm, info_desc: &str, name: &str, pkg: &str) -> ovelo_dexrt::V {
        let info = mint_info(vm, info_desc, name, pkg);
        let ri = V::Ref(vm.heap.alloc(Obj::Host {
            class_idx: u64::MAX,
            desc: "Landroid/content/pm/ResolveInfo;".to_string(),
            fields: HashMap::new(),
            state: HostState::Empty,
        }));
        if let (V::Ref(rh), V::Ref(ih)) = (ri, info) {
            ovelo_dexrt::pseudo_set(vm, rh, ovelo_dexrt::P_TARGET, V::Ref(ih));
        }
        ri
    }
    // queryIntent* → List<ResolveInfo> over the declared names
    // (activity, package) pairs — launchers additionally enumerate
    // every installed app's launcher activity so a home app can list
    // launchable apps like AOSP does.
    let mut launcher_pairs: Vec<(String, String)> = manifest
        .activities
        .iter()
        .map(|a| (a.clone(), manifest.package.clone()))
        .collect();
    for (p, a) in &v.installed_launchers {
        if *p != manifest.package {
            launcher_pairs.push((a.clone(), p.clone()));
        }
    }
    let own_pkg = manifest.package.clone();
    for (info_desc, meth, names, launcher_scope) in [
        (
            "Landroid/content/pm/ServiceInfo;",
            "queryIntentServices",
            manifest
                .services
                .iter()
                .map(|s| (s.clone(), manifest.package.clone()))
                .collect::<Vec<_>>(),
            false,
        ),
        (
            "Landroid/content/pm/ActivityInfo;",
            "queryIntentActivities",
            launcher_pairs.clone(),
            true,
        ),
        (
            "Landroid/content/pm/ActivityInfo;",
            "queryBroadcastReceivers",
            manifest
                .receivers
                .iter()
                .map(|s| (s.clone(), manifest.package.clone()))
                .collect::<Vec<_>>(),
            false,
        ),
        (
            "Landroid/content/pm/ProviderInfo;",
            "queryIntentContentProviders",
            manifest
                .providers
                .iter()
                .map(|s| (s.clone(), manifest.package.clone()))
                .collect::<Vec<_>>(),
            false,
        ),
    ] {
        let _ = v.register_host(
            "Landroid/content/pm/PackageManager;",
            meth,
            "(Landroid/content/Intent;I)Ljava/util/List;",
            {
                let own_pkg = own_pkg.clone();
                Rc::new(RefCell::new(move |vm: &mut ovelo_dexrt::Vm, a: &[V]| {
                    let want = intent_class(vm, a.get(1).copied());
                    // Other apps' launcher activities only answer a
                    // MAIN query — an implicit VIEW/EDIT intent must not
                    // see unrelated apps (their filters don't match).
                    let main_query = launcher_scope
                        && match a.get(1).copied() {
                            Some(V::Ref(ih)) => {
                                ovelo_dexrt::pseudo_get(vm, ih, ovelo_dexrt::P_ACTION)
                                    .and_then(|v| match v {
                                        V::Ref(s) => vm
                                            .heap
                                            .str_val(s)
                                            .map(|t| t == "android.intent.action.MAIN"),
                                        _ => None,
                                    })
                                    .unwrap_or(false)
                            }
                            _ => false,
                        };
                    let infos: Vec<V> = names
                        .iter()
                        .filter(|(n, p)| {
                            want.as_deref().is_none_or(|c| n.ends_with(c) || *n == c)
                                && (*p == own_pkg || want.is_some() || main_query)
                        })
                        .map(|(n, p)| mint_ri(vm, info_desc, n, p))
                        .collect();
                    if std::env::var_os("PMDEBUG").is_some() {
                        eprintln!(
                            "[pmdbg] {meth} want={want:?} names={names:?} -> {} infos",
                            infos.len()
                        );
                    }
                    Ok(Some(V::Ref(vm.heap.alloc(Obj::Host {
                        class_idx: u64::MAX,
                        desc: "Ljava/util/List;".to_string(),
                        fields: HashMap::new(),
                        state: HostState::List(infos),
                    }))))
                })) as ovelo_dexrt::HostCall
            },
        );
    }
    // resolveService / resolveActivity → the first match or null.
    for (info_desc, meth, names) in [
        (
            "Landroid/content/pm/ServiceInfo;",
            "resolveService",
            manifest.services.clone(),
        ),
        (
            "Landroid/content/pm/ActivityInfo;",
            "resolveActivity",
            manifest.activities.clone(),
        ),
    ] {
        let pkg = manifest.package.clone();
        let _ = v.register_host(
            "Landroid/content/pm/PackageManager;",
            meth,
            "(Landroid/content/Intent;I)Landroid/content/pm/ResolveInfo;",
            Rc::new(RefCell::new(move |vm: &mut ovelo_dexrt::Vm, a: &[V]| {
                let want = intent_class(vm, a.get(1).copied());
                let hit = names
                    .iter()
                    .find(|n| want.as_deref().is_none_or(|c| n.ends_with(c) || *n == c))
                    .map(|n| mint_ri(vm, info_desc, n, &pkg))
                    .unwrap_or(V::Ref(0));
                if std::env::var_os("PMDEBUG").is_some() {
                    eprintln!("[pmdbg] {meth} want={want:?} names={names:?} -> {hit:?}");
                }
                Ok(Some(hit))
            })) as ovelo_dexrt::HostCall,
        );
    }
    // getServiceInfo/getReceiverInfo/getActivityInfo → the declared
    // info object, or NameNotFoundException like a real PackageManager.
    for (info_desc, meth, ret_desc, names) in [
        (
            "Landroid/content/pm/ServiceInfo;",
            "getServiceInfo",
            "(Landroid/content/ComponentName;I)Landroid/content/pm/ServiceInfo;",
            manifest.services.clone(),
        ),
        (
            "Landroid/content/pm/ActivityInfo;",
            "getReceiverInfo",
            "(Landroid/content/ComponentName;I)Landroid/content/pm/ActivityInfo;",
            manifest.receivers.clone(),
        ),
        (
            "Landroid/content/pm/ActivityInfo;",
            "getActivityInfo",
            "(Landroid/content/ComponentName;I)Landroid/content/pm/ActivityInfo;",
            manifest.activities.clone(),
        ),
    ] {
        let pkg = manifest.package.clone();
        let _ = v.register_host(
            "Landroid/content/pm/PackageManager;",
            meth,
            ret_desc,
            Rc::new(RefCell::new(move |vm: &mut ovelo_dexrt::Vm, a: &[V]| {
                let want = match a.get(1).copied() {
                    Some(V::Ref(ch)) => vm
                        .get_object_field(ch, "Landroid/content/ComponentName;", "mClass")
                        .and_then(|v| match v {
                            V::Ref(sh) => vm.heap.str_val(sh).map(|t| t.to_string()),
                            _ => None,
                        }),
                    _ => None,
                };
                let hit = names
                    .iter()
                    .find(|n| want.as_deref().is_none_or(|c| n.ends_with(c) || *n == c))
                    .map(|n| mint_info(vm, info_desc, n, &pkg))
                    .unwrap_or_else(|| {
                        // Undeclared component — keep the builtin's
                        // empty-info answer (a guest catchable throw
                        // isn't expressible through HostCall).
                        V::Ref(vm.heap.alloc(Obj::Host {
                            class_idx: u64::MAX,
                            desc: info_desc.to_string(),
                            fields: HashMap::new(),
                            state: HostState::Empty,
                        }))
                    });
                if std::env::var_os("PMDEBUG").is_some() {
                    eprintln!("[pmdbg] {meth} want={want:?} -> {hit:?}");
                }
                Ok(Some(hit))
            })) as ovelo_dexrt::HostCall,
        );
    }
}
/// Intent) — the target replaces the finished one on the same VM and
/// window (single-task model). Its own `android:theme` and the caller's
/// Intent come along. Bounded: a chain of redirects still terminates.
fn drain_pending_activity(
    vm: &mut ovelo_dexrt::Vm,
    this: &mut u32,
    callbacks: &[String],
    manifest: &ovelo_apk::manifest::ManifestInfo,
) {
    for _ in 0..4 {
        let Some((next, intent)) = vm.pending_activity.take() else {
            break;
        };
        // Resolve the target's own theme before instantiation — theme-
        // scoped obtainStyledAttributes reads vm.theme_id. Restore it
        // when the launch fails: the previous Activity keeps running
        // under its own theme.
        let qualified = next
            .trim_start_matches('L')
            .trim_end_matches(';')
            .replace('/', ".");
        let prev_theme = vm.theme_id;
        vm.theme_id = manifest
            .activity_themes
            .get(&qualified)
            .copied()
            .or(manifest.theme)
            .unwrap_or(0);
        match vm.instantiate(&next) {
            Ok(h2) => {
                println!("activity {} (started)", disp(&next));
                vm.set_activity_intent(h2, intent);
                *this = h2;
                run_activity_cbs(vm, *this, callbacks);
            }
            Err(e) => {
                println!("activity {} not launched: {e}", disp(&next));
                vm.theme_id = prev_theme;
                break;
            }
        }
    }
}

/// Deliver the queued lifecycle callbacks to a guest Activity:
/// `finish()` stops the sequence, and the options menu is prepared
/// before `onResume` the way AOSP lazily creates it on first use.
fn run_activity_cbs(vm: &mut ovelo_dexrt::Vm, this: u32, callbacks: &[String]) {
    // Content/decor roots attach under the running activity — keep
    // each activity's trees scoped to itself for lookups and render.
    vm.set_activity(this);
    for cb in callbacks {
        if vm.is_finishing(this) {
            break;
        }
        if cb == "onResume" {
            let menu = vm.menu_for(this);
            for menu_cb in ["onCreateOptionsMenu", "onPrepareOptionsMenu"] {
                vm.call_virtual(this, menu_cb, &[menu])
                    .inspect_err(|e| println!("callback {menu_cb} crashed: {e}"))
                    .ok();
            }
        }
        let r = vm
            .call_virtual(this, cb, &[])
            .and_then(|v| match v {
                s @ Some(_) => Ok(s),
                None => vm.call_virtual(this, cb, &[ovelo_dexrt::V::Ref(0)]),
            })
            .inspect_err(|e| println!("callback {cb} crashed: {e}"));
        let ran = matches!(r, Ok(Some(_)));
        println!(
            "callback {} ({})",
            cb,
            if ran { "dex" } else { "framework" }
        );
        // AOSP ActivityThread calls onPostResume after onResume; androidx
        // FragmentActivity dispatches FragmentManager resume from there —
        // without it fragments stay capped at STARTED forever.
        if cb == "onResume" {
            let r2 = vm
                .call_virtual(this, "onPostResume", &[])
                .inspect_err(|e| println!("callback onPostResume crashed: {e}"));
            println!(
                "callback onPostResume ({})",
                if matches!(r2, Ok(Some(_))) {
                    "dex"
                } else {
                    "framework"
                }
            );
        }
        dispatch_alc(vm, this, cb);
    }
}

/// AOSP `Activity.performX` fans each lifecycle stage out to the
/// Application's registered `ActivityLifecycleCallbacks` (pre, mid and
/// post variants — impls override whichever they need; unresolved
/// names just skip). androidx `ReportFragment` registers one to drive
/// `ComponentActivity.mLifecycleRegistry` — without these calls the
/// registry stays INITIALIZED and Compose's `WrappedComposition` never
/// sees ON_RESUME, so `setContent` never runs.
fn dispatch_alc(vm: &mut ovelo_dexrt::Vm, this: u32, stage: &str) {
    use ovelo_dexrt::V;
    let (stem, bundle) = match stage {
        "onCreate" => ("Created", true),
        "onStart" => ("Started", false),
        "onResume" => ("Resumed", false),
        "onPause" => ("Paused", false),
        "onStop" => ("Stopped", false),
        "onSaveInstanceState" => ("SaveInstanceState", true),
        "onDestroy" => ("Destroyed", false),
        _ => return,
    };
    let names = [
        format!("onActivityPre{stem}"),
        format!("onActivity{stem}"),
        format!("onActivityPost{stem}"),
    ];
    for cb in vm.activity_callbacks() {
        for name in &names {
            let mut args = vec![V::Ref(this)];
            if bundle {
                args.push(V::Ref(0));
            }
            vm.call_virtual(cb, name, &args)
                .inspect_err(|e| println!("activity lifecycle {name} crashed: {e}"))
                .ok();
        }
    }
}

/// AOSP `AppInitializer.doInitialize`: an initializer's `dependencies()`
/// run before its own `create`. The `seen` set guards manifest/dex
/// dependency cycles and dedupes shared deps.
fn run_initializer(
    v: &mut ovelo_dexrt::Vm,
    desc: &str,
    ctx: ovelo_dexrt::V,
    seen: &mut std::collections::HashSet<String>,
    components: &std::rc::Rc<std::cell::RefCell<std::collections::HashMap<String, ovelo_dexrt::V>>>,
) {
    if !seen.insert(desc.to_string()) {
        return;
    }
    let Ok(ih) = v.instantiate(desc) else {
        println!("initializer {desc} not instantiated");
        return;
    };
    let mut deps: Vec<String> = Vec::new();
    if let Ok(Some(ovelo_dexrt::V::Ref(lh))) = v.call_virtual(ih, "dependencies", &[]) {
        if let Some(ovelo_dexrt::Obj::Host {
            state: ovelo_dexrt::HostState::List(items),
            ..
        }) = v.heap.get(lh)
        {
            deps = items
                .iter()
                .filter_map(|x| match x {
                    ovelo_dexrt::V::Ref(ch) => v.class_descriptor_of(*ch),
                    _ => None,
                })
                .collect();
        }
    }
    for d in deps {
        run_initializer(v, &d, ctx, seen, components);
    }
    match v.call_virtual(ih, "create", &[ctx]) {
        Ok(Some(out)) => {
            components.borrow_mut().insert(desc.to_string(), out);
        }
        Ok(None) => {}
        Err(e) => println!("initializer {desc} create failed: {e}"),
    }
}

fn run_app(store: &InstallStore, package: &str) -> Result<(), OveloError> {
    #[cfg(target_os = "macos")]
    rustzone::publish();
    let rec = find_record(store, package)?;
    let bytes = store.apk_bytes(&rec)?;
    let info = inspect_apk(&bytes)?;
    // Launch pipeline pieces that exist today: dex parse + verify.
    let dexes = verify_dexes(&bytes, &info)?;
    let bad: Vec<&str> = dexes
        .iter()
        .filter(|d| !d.ok)
        .map(|d| d.name.as_str())
        .collect();
    if !bad.is_empty() {
        return Err(OveloError::new(
            ErrorCategory::DexUnsupported,
            Phase::DexLoad,
            "cli.dex.verify",
        )
        .with_detail(bad.join(", ")));
    }
    let launcher = info
        .manifest
        .launcher_activity
        .clone()
        .or_else(|| info.manifest.activities.first().cloned())
        .unwrap_or_default();
    println!("ready   {}", disp(&rec.app.package));
    println!("activity {}", disp(&launcher));
    println!("dexes   {} verified", dexes.len());

    // Launch pipeline: Context → Activity lifecycle (via dexrt when the
    // class is in the DEX) → Presenter → window.
    let data_dir = store.app_dir(&rec.app).join("data");
    let _ctx = ovelo_framework::Context::new(rec.app.package.clone(), &data_dir);
    let mut activity = ovelo_framework::Activity::new();
    activity.launch().map_err(|e| {
        OveloError::new(
            ErrorCategory::ServiceMissing,
            Phase::Service,
            "cli.run.lifecycle",
        )
        .with_detail(e.to_string())
    })?;
    let callbacks: Vec<String> = {
        let mut v = Vec::new();
        while let Some(cb) = activity.delivered() {
            v.push(cb.to_string());
        }
        v
    };

    // dexrt dispatch: instantiate the launcher Activity's class, then run
    // each queued lifecycle callback as a virtual call. Callbacks that
    // take a Bundle get a null for now — real marshaling lands with the
    // object bridge; a class missing from the DEX keeps the framework
    // model's behavior.
    let desc = format!("L{};", launcher.replace('.', "/"));
    let desc_safe = disp(&desc);
    // Guest view bridge: guest view objects (`new TextView` etc.) bind to
    // host widgets hung off this root, and onClick listeners queue back
    // into the VM. Registered BEFORE instantiate so a constructor that
    // builds views in <init>/onCreate hits live stubs.
    let mut fonts = ovelo_ui::FontSet::new();
    fonts.load_dirs(&[
        "/System/Library/Fonts",
        "/System/Library/Fonts/Supplemental",
    ]);
    let fonts = std::rc::Rc::new(fonts);
    // P3 worker supervision: the whole guest dispatch (VM build → register
    // → instantiate → lifecycle callbacks) runs under catch_unwind. A panic
    // writes a crash report and bumps the generation; the worker restarts
    // once, then stays down — a guest failure never propagates to the host.
    // Each attempt gets a fresh GuestUi: guest view objects bind host
    // handles inside it, so a crashed attempt's views/listeners must not
    // leak into the restarted VM.
    let mut sup = worker::Supervisor::new(data_dir.join("diagnostics"));
    let (mut guest, ui) = loop {
        let fonts2 = fonts.clone();
        let out = sup.supervise("guest-dispatch", || {
            // VM mutex for the whole dispatch — serializes every
            // interpreter step + driver-side heap touch against JNI
            // calls from guest-spawned pthreads (call_native releases
            // all levels so .so threads can reach JNI while it runs).
            let _vmg = ovelo_dexrt::lock_vm();
            let ui = ovelo_framework::GuestUi::new(720, 1280, fonts2);
            // Components returned by initializer `create()` — shared
            // between the startup pass and `initializeComponent` so a
            // component initializes once per run.
            let components =
                std::rc::Rc::new(std::cell::RefCell::new(std::collections::HashMap::<
                    String,
                    ovelo_dexrt::V,
                >::new()));
            // Set when the real androidx.startup InitializationProvider
            // ran — the AppInitializer shims then delegate to dex code
            // (the guest singletons carry the discovered state).
            let provider_ran = std::rc::Rc::new(std::cell::Cell::new(false));
            let vm = (|| -> Result<_, OveloError> {
                let (_cd, entries) = parse_central_directory(&bytes, &ZipLimits::default())?;
                // Multi-dex: the Vm merges every classes*.dex into one
                // index space — a method defined in classes2.dex resolves
                // from an invoke in classes.dex. Put the launcher-defining
                // dex first so its refs become canonical identities.
                let mut dex_files: Vec<DexFile> = Vec::new();
                for e in entries
                    .iter()
                    .filter(|e| e.name.starts_with("classes") && e.name.ends_with(".dex"))
                {
                    dex_files.push(DexFile::open(read_entry(&bytes, e)?)?);
                }
                if dex_files.is_empty() {
                    return Err(err("cli.dex.missing"));
                }
                // The launcher class must be *defined* in the dex — a
                // type reference alone (type_index_of) is satisfied by
                // dexes that only invoke it.
                let pos = dex_files
                    .iter()
                    .position(|d| {
                        d.class_defs().iter().any(|cd| {
                            d.type_descriptor(cd.class_idx).ok().as_deref() == Some(desc.as_str())
                        })
                    })
                    .unwrap_or(0);
                let mut v = ovelo_dexrt::Vm::new(dex_files.swap_remove(pos))?;
                for d in dex_files {
                    v.add_dex(d)?;
                }
                v.guest_root = data_dir.join("files");
                // The app's real package name — `getPackageName` and
                // `Telephony.getDefaultSmsPackage` both answer with it, and
                // SMS apps compare the two to decide they are the default
                // handler (an empty answer sends them to a setup screen).
                v.guest_package = Some(rec.app.package.clone());
                // Launcher queries (home screen) enumerate every
                // installed app's launcher activity — parse each
                // record's manifest once at startup.
                for r in store.installed() {
                    let Ok(bytes) = store.apk_bytes(&r) else {
                        continue;
                    };
                    let Ok(ai) = ovelo_apk::inspect_apk(&bytes) else {
                        continue;
                    };
                    if let Some(act) = ai.manifest.launcher_activity {
                        v.installed_launchers
                            .push((ai.manifest.package.clone(), act));
                    }
                    // Icon/label readers re-read the APK lazily — keep
                    // the path, not the bytes, so non-launcher apps
                    // don't pin every installed APK in memory.
                    v.installed_apk_paths
                        .insert(ai.manifest.package, r.apk_path.clone());
                }
                // Seed shared external storage once so file managers
                // have something to list (safe: only fills what's absent).
                let ext = &v.external_root;
                for d in ["Documents", "Download", "Pictures"] {
                    let _ = std::fs::create_dir_all(ext.join(d));
                }
                for (f, body) in [
                    ("readme.md", "ovelo guest storage\n".as_bytes().to_vec()),
                    (
                        "Documents/notes.txt",
                        "hello from ovelo\n".as_bytes().to_vec(),
                    ),
                    (
                        "Pictures/sunset.png",
                        include_bytes!("../assets/seed-sunset.png").to_vec(),
                    ),
                    (
                        "Pictures/mountain.png",
                        include_bytes!("../assets/seed-mountain.png").to_vec(),
                    ),
                    (
                        "Pictures/forest.png",
                        include_bytes!("../assets/seed-forest.png").to_vec(),
                    ),
                    (
                        "Pictures/ocean.png",
                        include_bytes!("../assets/seed-ocean.png").to_vec(),
                    ),
                ] {
                    let p = ext.join(f);
                    if !p.exists() {
                        let _ = std::fs::write(p, body);
                    }
                }
                // Music-player apps list tracks via the MediaStore
                // audio cursor — give it real files to find.
                let music = ext.join("Music");
                let _ = std::fs::create_dir_all(&music);
                for (i, freq) in [440.0f32, 550.0, 660.0].iter().enumerate() {
                    let p = music.join(format!("seed-tone-{}.wav", i + 1));
                    if !p.exists() {
                        let _ = std::fs::write(p, wav_tone(*freq));
                    }
                }
                v.apk_res = ovelo_dexrt::res::ApkResources::open(bytes.clone()).map(Box::new);
                v.theme_id = info
                    .manifest
                    .activity_themes
                    .get(&launcher)
                    .copied()
                    .or(info.manifest.theme)
                    .unwrap_or(0);
                // Install-time grants: manifest <uses-permission> ∩
                // normal-level; the rest stay denied until the app
                // calls requestPermissions.
                v.grant_manifest_permissions(&info.manifest.permissions);
                if std::env::var_os("OVELO_NO_AOT").is_none() {
                    v.enable_aot();
                }
                register_compat_shims(
                    &mut v,
                    &info.manifest.startup_initializers,
                    components.clone(),
                    provider_ran.clone(),
                    &info.manifest,
                );
                Ok(v)
            })();
            let mut g = vm
                .and_then(|mut v| {
                    ovelo_framework::GuestUi::register(&ui, &mut v);
                    // Android startup order: androidx.startup initializers
                    // run *before* Application.onCreate — regardless of
                    // whether the manifest names an Application subclass.
                    let app_h = info
                        .manifest
                        .application_class
                        .as_ref()
                        .and_then(|app| v.instantiate(&format!("L{};", app.replace('.', "/"))).ok())
                        .unwrap_or_else(|| {
                            v.heap.alloc(ovelo_dexrt::Obj::Host {
                                class_idx: u64::MAX,
                                desc: "Landroid/app/Application;".to_string(),
                                fields: std::collections::HashMap::new(),
                                state: ovelo_dexrt::HostState::Empty,
                            })
                        });
                    // Prefer the real ContentProvider path: run the
                    // manifest's androidx.startup InitializationProvider
                    // onCreate so `discoverAndInitialize` populates
                    // AppInitializer's own discovered/initialized sets —
                    // lazy `initializeComponent` callers (e.g.
                    // ProcessLifecycleInitializer) depend on them, and
                    // the fields are R8-obfuscated so host writes can't
                    // reach them. Fall back to driving the initializers
                    // directly when the provider path fails.
                    if info
                        .manifest
                        .providers
                        .iter()
                        .any(|p| p.ends_with("InitializationProvider"))
                    {
                        match v.instantiate("Landroidx/startup/InitializationProvider;") {
                            Ok(ph) => {
                                let _ = v.call_virtual(
                                    ph,
                                    "attachInfo",
                                    &[ovelo_dexrt::V::Ref(app_h), ovelo_dexrt::V::Ref(0)],
                                );
                                match v.call_virtual(ph, "onCreate", &[]) {
                                    Ok(_) => provider_ran.set(true),
                                    Err(e) => {
                                        println!("startup provider onCreate failed: {e}")
                                    }
                                }
                            }
                            Err(e) => println!("startup provider not instantiated: {e}"),
                        }
                    }
                    if !provider_ran.get() {
                        let mut seen = std::collections::HashSet::new();
                        for init in &info.manifest.startup_initializers {
                            run_initializer(
                                &mut v,
                                &format!("L{};", init.replace('.', "/")),
                                ovelo_dexrt::V::Ref(app_h),
                                &mut seen,
                                &components,
                            );
                        }
                    }
                    if let Some(app) = &info.manifest.application_class {
                        if let Err(e) = v.call_virtual(app_h, "onCreate", &[]) {
                            println!("application {app} onCreate failed: {e}");
                        }
                    }
                    v.instantiate(&desc).map(|h| (v, h))
                })
                .map_err(|e| {
                    println!("guest    {desc_safe} not dispatched: {e}");
                })
                .ok();
            if let Some((vm, this)) = &mut g {
                run_activity_cbs(vm, *this, &callbacks);
                drain_pending_activity(vm, this, &callbacks, &info.manifest);
            }
            (g, ui)
        });
        match out {
            worker::Outcome::Ok(pair) => break pair,
            worker::Outcome::Crashed(path) => {
                println!("crash    guest dispatch panicked -> {}", path.display());
                if sup.restartable() {
                    sup.note_restart();
                    println!("restart  worker generation {}", sup.generation());
                    continue;
                }
                // Still need a ui for the fallback demo tree.
                break (
                    None,
                    ovelo_framework::GuestUi::new(720, 1280, fonts.clone()),
                );
            }
        }
    };
    println!("dispatch done — opening window");
    // Debug hook: OVELO_AUTOSTART_SERVICE=<descriptor> instantiates the
    // service and runs onCreate+onStartCommand once the window is up —
    // exercises the service path without a UI trigger.
    if let Some(desc) = std::env::var("OVELO_AUTOSTART_SERVICE")
        .ok()
        .filter(|d| !d.is_empty())
    {
        if let Some((vm, _)) = &mut guest {
            let intent = {
                let ih = vm.heap.alloc(ovelo_dexrt::Obj::Host {
                    class_idx: u64::MAX,
                    desc: "Landroid/content/Intent;".into(),
                    fields: std::collections::HashMap::new(),
                    state: ovelo_dexrt::HostState::Empty,
                });
                ovelo_dexrt::V::Ref(ih)
            };
            match vm.instantiate(&desc) {
                Ok(sh) => {
                    let _ = vm.call_virtual(sh, "onCreate", &[]);
                    let r = vm.call_virtual(
                        sh,
                        "onStartCommand",
                        &[intent, ovelo_dexrt::V::I32(0), ovelo_dexrt::V::I32(1)],
                    );
                    println!("[autosvc] {desc} onStartCommand -> {r:?}");
                }
                Err(e) => println!("[autosvc] {desc} instantiate failed: {e}"),
            }
        }
    }
    let mut presenter = ovelo_host::Presenter::new();
    let sid = presenter.attach(3, 720, 1280)?;
    let win = ovelo_host::HostWindow::new(&disp(&rec.app.package), sid, presenter);
    // Fallback demo tree when the Activity wasn't dispatched (e.g. class
    // missing): proves the render+tap path without a guest.
    if guest.is_none() {
        let mut col = ovelo_framework::ViewGroup::new(
            ovelo_framework::Length::MatchParent,
            ovelo_framework::Length::WrapContent,
        );
        let fonts2 = {
            let g = ui.borrow();
            g.fonts.clone()
        };
        let mut title = ovelo_framework::TextView::new(
            rec.app.package.clone(),
            24.0,
            ovelo_ui::Color::srgb(0x20, 0x20, 0x20),
            fonts2.clone(),
        );
        title.padding = 16;
        col.children.push(Box::new(title));
        let taps = std::rc::Rc::new(std::cell::Cell::new(0u32));
        let mut ok = ovelo_framework::Button::new(
            "Tap me",
            18.0,
            ovelo_ui::Color::srgb(0x30, 0x30, 0x30),
            fonts2,
        );
        let taps2 = taps.clone();
        ok.on_click = Some(Box::new(move || {
            taps2.set(taps2.get() + 1);
        }));
        col.children.push(Box::new(ok));
        ui.borrow_mut().root.set_content_view(Box::new(col));
    }
    let ui2 = ui.clone();
    let mut dex_frame = 0u32;
    let mut dex_grab = false;
    ovelo_host::run(win, move |w| {
        let mut dead = false;
        for ev in w.take_input() {
            let Some(pt) = ev.pointers.get(ev.action_index) else {
                continue;
            };
            let (tx, ty) = (pt.x as i32, pt.y as i32);
            // Android MotionEvent action codes.
            let a = match ev.action {
                ovelo_host::MotionAction::Down => 0,
                ovelo_host::MotionAction::Up => 1,
                ovelo_host::MotionAction::Move => 2,
                ovelo_host::MotionAction::PointerDown => 5,
                ovelo_host::MotionAction::PointerUp => 6,
                ovelo_host::MotionAction::Cancel => 3,
                _ => continue,
            };
            // The dex view tree is authoritative: when it claims the
            // point the guest framework (OnTouchListener / performClick
            // / onTouchEvent) owns the gesture. The host widget tree —
            // only covering host-materialized views — sees a touch
            // solely when dex did not claim it, so a view mirrored in
            // both trees never fires the same listener twice.
            let mut claimed = false;
            if let Some((vm, _)) = &mut guest {
                match sup.supervise("touch", || vm.dispatch_touch(tx, ty, a)) {
                    worker::Outcome::Ok(Ok(b)) => claimed = b,
                    worker::Outcome::Ok(Err(e)) => {
                        println!("touch crashed: {}", e.cause());
                    }
                    worker::Outcome::Crashed(p) => {
                        println!("touch panic -> {}", p.display());
                        dead = true;
                        break;
                    }
                }
            }
            if a == 0 {
                dex_grab = claimed;
            }
            if a == 3 {
                dex_grab = false;
            }
            if dex_grab || claimed {
                continue;
            }
            let mut u = ui2.borrow_mut();
            match a {
                0 => {
                    let _ = u.root.dispatch_touch(true, tx, ty);
                }
                1 => {
                    let _ = u.root.dispatch_touch(false, tx, ty);
                }
                _ => {}
            }
        }
        // Deferred onClick dispatches — queued during touch handling,
        // pumped here so the VM is free to run guest code that mutates
        // the same views (RefCell re-entrancy avoided).
        let pending = ui2.borrow().drain_clicks();
        if let Some((vm, this)) = &mut guest {
            for (l, vh) in pending {
                match sup.supervise("onClick", || {
                    vm.call_virtual(l, "onClick", &[ovelo_dexrt::V::Ref(vh)])
                }) {
                    worker::Outcome::Ok(Err(e)) => {
                        println!("onClick crashed: {}", e.cause());
                    }
                    worker::Outcome::Ok(_) => {}
                    worker::Outcome::Crashed(p) => {
                        println!("onClick panic -> {}", p.display());
                        dead = true;
                        break;
                    }
                }
            }
            // A click handler may call startActivity — launch the target
            // the same way the initial-lifecycle drain does. The drain
            // runs guest code (ctor + lifecycle), so it stays under
            // supervision like every other guest dispatch.
            if !dead && vm.pending_activity.is_some() {
                match sup.supervise("activity-start", || {
                    drain_pending_activity(vm, this, &callbacks, &info.manifest);
                }) {
                    worker::Outcome::Ok(_) => {}
                    worker::Outcome::Crashed(p) => {
                        println!("activity-start panic -> {}", p.display());
                        dead = true;
                    }
                }
            }
        }
        // Panicked worker: drop its surface generation so no stale frame
        // displays, and never dispatch into the dead VM again.
        if dead {
            w.presenter().stop_worker(sid);
            guest = None;
        }
        // Apps that inflate layout xml (setContentView(res)) live in the
        // VM's dex view tree — materialize it into drawable host views.
        if let Some((vm, this)) = &mut guest {
            // One lock for the whole tick — drain_runnables, GC and
            // the layout pass all touch the heap between invokes, and
            // guest-pthread JNI calls must not alias them.
            let _vmg = ovelo_dexrt::lock_vm();
            dex_frame = dex_frame.wrapping_add(1);
            // Drain queued background work (`Thread.start`/
            // `Executor.execute`/`Handler.post*`) on the same cadence
            // as the layout pass — media scans, DB loads and adapter
            // fills all live on this queue after the lifecycle drains
            // end. Without it they stall once the window opens.
            if dex_frame.is_multiple_of(30) || vm.has_queued_runnables() {
                let work = vm.drain_runnables(64);
                // GC when the heap passes its growth mark — interpreter
                // churn otherwise grows the heap unboundedly. Only runs
                // between dex activations (collect_garbage gates itself).
                if std::env::var_os("OVELO_MEMDBG").is_some() {
                    eprintln!(
                        "[mem] frame={dex_frame} heap={} mark={}",
                        vm.heap.len(),
                        vm.gc_mark
                    );
                }
                if vm.heap.len() >= vm.gc_mark {
                    let mut roots = ui2.borrow().guest_handles().into_iter();
                    vm.collect_garbage(&mut roots);
                }
                // Drive the expensive layout pass only on real change:
                // runnable work done this tick, guest view state
                // mutated since the last pass, or a periodic catch-all
                // for fills that don't bump view_gen (adapter attaches
                // do). Idle trees converge and stop paying interpreter
                // cost + RSS growth every half second.
                if work > 0
                    || vm.view_gen != vm.layout_gen_done.get()
                    || dex_frame.is_multiple_of(600)
                {
                    // Run adapter containers' layout pass OUTSIDE the ui
                    // borrow — guest code re-enters bridged view methods.
                    let (win_w, win_h) = {
                        let u = ui2.borrow();
                        (u.root.w, u.root.h)
                    };
                    vm.set_window_size(win_w, win_h);
                    ovelo_framework::drive_dex_layout_pass(vm, *this, dex_frame, win_w, win_h);
                }
            }
            ui2.borrow_mut().refresh_dex_tree(vm, *this);
            // View attachments queued during materialize — dispatch
            // outside the GuestUi borrow: the callback runs guest code
            // (data loads, runOnUiThread) that re-enters bridged calls.
            let attaches = ui2.borrow_mut().take_attach_pending();
            for h in attaches {
                if !vm.view_is_attached(h) {
                    vm.view_window_attached(h);
                }
            }
        }
        let mut u = ui2.borrow_mut();
        if let Ok(prod) = w.presenter().producer(sid) {
            // Track window resizes: the buffer queue follows the new size
            // and the root re-lays out on the next traversal.
            let (bw, bh) = (prod.queue().width() as i32, prod.queue().height() as i32);
            if (bw, bh) != (u.root.w, u.root.h) {
                u.root.resize(bw, bh);
            }
            let posted = u.root.perform_traversals(prod);
            if std::env::var_os("OVELO_VIEWDBG").is_some() && posted.ok().flatten().is_some() {
                if let Some(c) = &u.root.content {
                    eprintln!(
                        "[viewdbg] drew root measured={:?} lp={:?}",
                        c.measured(),
                        c.lp()
                    );
                }
            }
        }
    })
}

/// A tiny valid WAV (8 kHz mono 8-bit, ~0.4 s sine) — big enough for
/// MediaStore to index and players to probe, no asset file needed.
fn wav_tone(freq_hz: f32) -> Vec<u8> {
    const RATE: u32 = 8000;
    let n = (RATE as f32 * 0.4) as usize;
    let mut data = Vec::with_capacity(n + 44);
    let data_len = n as u32;
    data.extend_from_slice(b"RIFF");
    data.extend_from_slice(&(36 + data_len).to_le_bytes());
    data.extend_from_slice(b"WAVEfmt ");
    data.extend_from_slice(&16u32.to_le_bytes());
    data.extend_from_slice(&1u16.to_le_bytes()); // PCM
    data.extend_from_slice(&1u16.to_le_bytes()); // mono
    data.extend_from_slice(&RATE.to_le_bytes());
    data.extend_from_slice(&RATE.to_le_bytes()); // byte rate
    data.extend_from_slice(&1u16.to_le_bytes()); // block align
    data.extend_from_slice(&8u16.to_le_bytes()); // bits/sample
    data.extend_from_slice(b"data");
    data.extend_from_slice(&data_len.to_le_bytes());
    for i in 0..n {
        let t = i as f32 / RATE as f32;
        data.push((128.0 + 100.0 * (t * freq_hz * std::f32::consts::TAU).sin()) as u8);
    }
    data
}

fn cmd() -> Result<(), OveloError> {
    let argv: Vec<String> = std::env::args().collect();
    match parse_args(&argv)? {
        Cmd::Help => {
            print!("{USAGE}");
            Ok(())
        }
        Cmd::Inspect { apk, json } => {
            let info = inspect_apk(&read_apk(&apk)?)?;
            print_inspect(&info, json);
            Ok(())
        }
        Cmd::Install { apks, json } => {
            let store = default_store()?;
            let mut inputs: Vec<(String, Vec<u8>)> = Vec::with_capacity(apks.len());
            for p in &apks {
                let name = p
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| p.display().to_string());
                inputs.push((name, read_apk(p)?));
            }
            let refs: Vec<(&str, &[u8])> = inputs
                .iter()
                .map(|(n, b)| (n.as_str(), b.as_slice()))
                .collect();
            let rec = install_split_set(&store, &refs, &DeviceProfile::default())?;
            if json {
                println!(
                    "{{\n  \"package\": {:?},\n  \"versionCode\": {},\n  \"sha256\": {:?},\n  \"splits\": {}\n}}",
                    rec.app.package,
                    rec.version_code,
                    rec.apk_sha256,
                    rec.splits.len()
                );
            } else {
                println!(
                    "installed {} v{} -> {}",
                    rec.app.package,
                    rec.version_code,
                    rec.apk_path.display()
                );
                for s in &rec.splits {
                    println!("  split   {} -> {}", disp(&s.name), &s.sha256[..16]);
                }
            }
            Ok(())
        }
        Cmd::Run { app } => run_app(&default_store()?, &app),
        Cmd::Disas { apk, class, method } => disas(&read_apk(&apk)?, &class, method.as_deref()),
        Cmd::Gate { path } => {
            let items = gate::evaluate(&path)?;
            for i in &items {
                let v = match &i.verdict {
                    gate::Verdict::Pass => "pass".to_string(),
                    gate::Verdict::Warn(w) => format!("warn: {w}"),
                    gate::Verdict::Fail(f) => format!("FAIL: {f}"),
                    gate::Verdict::Skip(s) => format!("skip: {s}"),
                };
                println!("gate    {:<14} {v}", i.name);
            }
            if gate::passed(&items) {
                Ok(())
            } else {
                Err(OveloError::new(
                    ErrorCategory::PkgInvalid,
                    Phase::Install,
                    "gate.failed",
                ))
            }
        }
        Cmd::Diagnose { apk, json } => {
            let bytes = read_apk(&apk)?;
            let info = inspect_apk(&bytes)?;
            let dexes = verify_dexes(&bytes, &info)?;
            let sig = if info.signed_v2_or_v3 {
                "v2/v3"
            } else {
                "v1-or-none"
            };
            if json {
                let dx: Vec<String> = dexes
                    .iter()
                    .map(|d| {
                        format!(
                            "{{\"name\":{:?},\"ok\":{},\"cause\":{:?}}}",
                            d.name, d.ok, d.cause
                        )
                    })
                    .collect();
                println!(
                    "{{\n  \"package\": {:?},\n  \"signature\": {:?},\n  \"abis\": {:?},\n  \"dexes\": [{}],\n  \"sha256\": {:?}\n}}",
                    info.manifest.package,
                    sig,
                    info.abis,
                    dx.join(", "),
                    info.sha256
                );
            } else {
                println!("diagnose {}", disp(&info.manifest.package));
                println!("  signature : {sig}");
                println!("  abis      : {}", info.abis.join(", "));
                for d in &dexes {
                    println!(
                        "  dex {} : {}",
                        d.name,
                        if d.ok { "ok".into() } else { d.cause.clone() }
                    );
                }
            }
            // §14: every diagnose appends a ledger record — pass only for
            // this (apk sha, scenario, runtime) combination.
            let bad: Vec<String> = dexes
                .iter()
                .filter(|d| !d.ok)
                .map(|d| format!("{}:{}", d.name, d.cause))
                .collect();
            // A present-but-corrupt signature block is a fail: signing
            // must *verify*, not just exist.
            let signer = apk_signer(&bytes);
            let (result, detail) = if bad.is_empty() && !signer.is_empty() {
                ("pass", String::new())
            } else if bad.is_empty() {
                ("fail", "unsigned-or-invalid-signature".to_string())
            } else {
                ("fail", bad.join(", "))
            };
            ledger_record(
                &info,
                &signer,
                "inspect-only",
                "diagnose",
                result,
                &detail,
                !json,
            );
            Ok(())
        }
    }
}

fn main() -> ExitCode {
    match cmd() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ovelo: {e}");
            if e.cause() == "cli.usage" {
                eprint!("{USAGE}");
                ExitCode::from(2)
            } else {
                ExitCode::FAILURE
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arg_parse() {
        let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        match parse_args(&v(&["ovelo", "inspect", "a.apk", "--json"])).unwrap() {
            Cmd::Inspect { apk, json } => {
                assert_eq!(apk, PathBuf::from("a.apk"));
                assert!(json);
            }
            _ => panic!(),
        }
        match parse_args(&v(&["ovelo", "run", "com.x"])).unwrap() {
            Cmd::Run { app } => assert_eq!(app, "com.x"),
            _ => panic!(),
        }
        match parse_args(&v(&["ovelo", "install", "b.apk"])).unwrap() {
            Cmd::Install { json, .. } => assert!(!json),
            _ => panic!(),
        }
        match parse_args(&v(&["ovelo", "diagnose", "d.apk"])).unwrap() {
            Cmd::Diagnose { .. } => {}
            _ => panic!(),
        }
        assert!(parse_args(&v(&["ovelo", "bogus"])).is_err());
        assert!(parse_args(&v(&["ovelo", "inspect"])).is_err());
        assert!(parse_args(&v(&["ovelo", "inspect", "-demo.apk"])).is_err());
        match parse_args(&v(&["ovelo", "inspect", "--", "-demo.apk"])).unwrap() {
            Cmd::Inspect { apk, .. } => assert_eq!(apk, PathBuf::from("-demo.apk")),
            _ => panic!(),
        }
        assert!(parse_args(&v(&["ovelo", "inspect", "a.apk", "extra"])).is_err());
        matches!(parse_args(&v(&["ovelo"])).unwrap(), Cmd::Help);
    }
}

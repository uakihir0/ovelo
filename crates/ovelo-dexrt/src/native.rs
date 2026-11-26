//! Real ARM64 `.so` loading: mmap the APK's bundled shared library into
//! executable memory, apply dynamic relocations binding imports to host
//! symbols (`dlsym` on libSystem covers libc/libc++/libm basics; a small
//! shim table covers Bionic-only names), run initializers, then
//! `JNI_OnLoad` runs with a real JavaVM (see `jni`).

// This module is inherently unsafe: it maps and executes foreign machine
// code. Safety invariants are documented at each site.
#![allow(unsafe_code)]

use std::collections::{HashMap, HashSet};
use std::ffi::{c_void, CString};
#[cfg(target_os = "macos")]
use std::sync::OnceLock;
use std::sync::{Arc, LazyLock, Mutex};

use ovelo_error::{ErrorCategory, OveloError, Phase, Result};

fn err(cause: &str) -> OveloError {
    OveloError::new(ErrorCategory::AbiSymbol, Phase::Native, cause)
}
fn errd(cause: &str, detail: impl std::fmt::Display) -> OveloError {
    err(cause).with_detail(detail.to_string())
}

extern "C" {
    fn mmap(addr: *mut c_void, len: usize, prot: i32, flags: i32, fd: i32, off: i64)
        -> *mut c_void;
    fn mprotect(addr: *mut c_void, len: usize, prot: i32) -> i32;
    fn munmap(addr: *mut c_void, len: usize) -> i32;
    fn dlsym(handle: *mut c_void, symbol: *const i8) -> *mut c_void;
    #[cfg(target_os = "macos")]
    fn dlopen(path: *const i8, mode: i32) -> *mut c_void;
    fn strlen(s: *const i8) -> usize;
    fn memcpy(d: *mut c_void, s: *const c_void, n: usize) -> *mut c_void;
    fn memset(d: *mut c_void, c: i32, n: usize) -> *mut c_void;
    fn strcpy(d: *mut i8, s: *const i8) -> *mut i8;
    fn strncpy(d: *mut i8, s: *const i8, n: usize) -> *mut i8;
    fn strcat(d: *mut i8, s: *const i8) -> *mut i8;
    fn memmove(d: *mut c_void, s: *const c_void, n: usize) -> *mut c_void;
    fn vsnprintf(s: *mut i8, n: usize, fmt: *const i8, ap: *mut c_void) -> i32;
    fn open(path: *const i8, flags: i32, ...) -> i32;
    fn abort() -> !;
    #[cfg(target_os = "macos")]
    fn pthread_threadid_np(t: usize, id: *mut u64) -> i32;
    #[cfg(target_os = "macos")]
    fn pthread_self() -> usize;
    #[cfg(target_os = "macos")]
    fn pthread_get_stackaddr_np(t: usize) -> *mut c_void;
    #[cfg(target_os = "macos")]
    fn pthread_get_stacksize_np(t: usize) -> usize;
    #[cfg(target_os = "macos")]
    fn __error() -> *mut i32;
    #[cfg(not(target_os = "macos"))]
    fn __errno_location() -> *mut i32;
    fn lseek(fd: i32, off: i64, whence: i32) -> i64;
    fn read(fd: i32, buf: *mut c_void, n: usize) -> isize;
    #[cfg(target_os = "macos")]
    fn sys_icache_invalidate(start: *mut c_void, len: usize);
    #[cfg(target_os = "macos")]
    fn sigaction(sig: i32, act: *const Sigaction, old: *mut Sigaction) -> i32;
    #[cfg(target_os = "macos")]
    fn _exit(code: i32) -> !;
    #[cfg(target_os = "macos")]
    fn write(fd: i32, buf: *const c_void, n: usize) -> isize;
    #[cfg(target_os = "macos")]
    fn sigaltstack(ss: *const SigStack, old: *mut SigStack) -> i32;
    #[cfg(target_os = "macos")]
    fn dladdr(addr: *const c_void, info: *mut DlInfo) -> i32;
    #[cfg(target_os = "macos")]
    fn malloc_create_zone(start_size: usize, flags: u32) -> *mut c_void;
    #[cfg(target_os = "macos")]
    fn malloc_zone_malloc(zone: *mut c_void, size: usize) -> *mut c_void;
    #[cfg(target_os = "macos")]
    fn malloc_zone_calloc(zone: *mut c_void, n: usize, size: usize) -> *mut c_void;
    #[cfg(target_os = "macos")]
    fn malloc_zone_realloc(zone: *mut c_void, ptr: *mut c_void, size: usize) -> *mut c_void;
    #[cfg(target_os = "macos")]
    fn malloc_zone_free(zone: *mut c_void, ptr: *mut c_void);
    #[cfg(target_os = "macos")]
    fn malloc_zone_memalign(zone: *mut c_void, align: usize, size: usize) -> *mut c_void;
    #[cfg(target_os = "macos")]
    fn malloc_zone_from_ptr(ptr: *const c_void) -> *mut c_void;
    #[cfg(target_os = "macos")]
    fn malloc_size(ptr: *const c_void) -> usize;
    #[cfg(target_os = "macos")]
    fn malloc_default_zone() -> *mut c_void;
    #[cfg(target_os = "macos")]
    fn free(ptr: *mut c_void);
    #[cfg(target_os = "macos")]
    fn mach_task_self() -> u32;
    #[cfg(target_os = "macos")]
    fn mach_vm_read_overwrite(
        task: u32,
        addr: u64,
        size: u64,
        data: u64,
        out_size: *mut u64,
    ) -> i32;
    #[cfg(target_os = "macos")]
    fn mach_vm_region(
        task: u32,
        addr: *mut u64,
        size: *mut u64,
        flavor: i32,
        info: *mut i32,
        count: *mut u32,
        object: *mut u32,
    ) -> i32;
}

/// Is `p..p+n` writable user memory? Used to admit guest writes into
/// real buffers that aren't in the guest zone (host-heap or mmap'd
/// allocations the guest legitimately owns). Page-granular via Mach.
#[cfg(target_os = "macos")]
fn mem_writable(p: *const c_void, n: usize) -> bool {
    // vm_region_basic_info_data_64_t, flavor VM_REGION_BASIC_INFO_64,
    // count = 9 ints.
    const FLAVOR: i32 = 9; // VM_REGION_BASIC_INFO_64
    const VM_PROT_WRITE: i32 = 0x2;
    let mut a = p as u64;
    let end = a.saturating_add(n.max(1) as u64);
    while a < end {
        let mut size = 0u64;
        let mut info = [0i32; 9];
        let mut count = 9u32;
        let mut obj = 0u32;
        let kr = unsafe {
            mach_vm_region(
                mach_task_self(),
                &mut a,
                &mut size,
                FLAVOR,
                info.as_mut_ptr(),
                &mut count,
                &mut obj,
            )
        };
        if kr != 0 || size == 0 || info[0] & VM_PROT_WRITE == 0 {
            return false;
        }
        a = a.saturating_add(size);
    }
    true
}

/// Read `size` bytes of OUR address space via Mach — safe inside the
/// signal handler where the faulting address may itself be unmapped
/// (an instruction abort on a wild branch has pc == unmapped, so a
/// plain deref faults inside the handler: nested fault during signal
/// delivery is the kernel's "unrecoverable exception" -> SIGKILL).
#[cfg(target_os = "macos")]
unsafe fn safe_read_u32(addr: usize) -> Option<u32> {
    let mut out = 0u32;
    let mut n = 0u64;
    let kr = mach_vm_read_overwrite(
        mach_task_self(),
        addr as u64,
        4,
        &mut out as *mut u32 as u64,
        &mut n,
    );
    if kr == 0 && n == 4 {
        Some(out)
    } else {
        None
    }
}

#[cfg(target_os = "macos")]
unsafe fn safe_read_u64(addr: usize) -> Option<u64> {
    let mut out = 0u64;
    let mut n = 0u64;
    let kr = mach_vm_read_overwrite(
        mach_task_self(),
        addr as u64,
        8,
        &mut out as *mut u64 as u64,
        &mut n,
    );
    if kr == 0 && n == 8 {
        Some(out)
    } else {
        None
    }
}

#[cfg(target_os = "macos")]
#[repr(C)]
struct Sigaction {
    sa_sigaction: usize,
    sa_mask: u32,
    sa_flags: i32,
}
#[cfg(target_os = "macos")]
#[repr(C)]
struct SigStack {
    ss_sp: *mut c_void,
    ss_size: usize,
    ss_flags: i32,
}
#[cfg(target_os = "macos")]
#[repr(C)]
struct DlInfo {
    dli_fname: *const i8,
    dli_fbase: *mut c_void,
    dli_sname: *const i8,
    dli_saddr: *mut c_void,
}

/// (map_base, span) of every guest image — lets the crash handler say
/// whether the faulting PC is inside a loaded `.so`.
pub(crate) static LOADED_RANGES: Mutex<Vec<(usize, usize)>> = Mutex::new(Vec::new());

/// Bounds of the JIT thunk arena, published so the crash handler's
/// instruction scan-back knows which addresses it may read. Written
/// once when the arena is first mapped.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub(crate) static JIT_CODE_BASE: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub(crate) static JIT_CODE_END: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Async-signal-safe-ish mapped check for the segv handler: inside a
/// loaded `.so` or the thunk arena. `try_lock` keeps it deadlock-free
/// — when the range list is busy we conservatively report unmapped,
/// which only shrinks the provenance scan.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn code_mapped(addr: usize) -> bool {
    let base = JIT_CODE_BASE.load(std::sync::atomic::Ordering::Acquire);
    let end = JIT_CODE_END.load(std::sync::atomic::Ordering::Acquire);
    if base != 0 && addr >= base && addr < end {
        return true;
    }
    LOADED_RANGES
        .try_lock()
        .map(|v| v.iter().any(|(b, s)| addr >= *b && addr < b + s))
        .unwrap_or(false)
}

/// Image-relative offset of `addr` when it lies inside a loaded `.so`.
pub(crate) fn so_off(addr: usize) -> Option<usize> {
    LOADED_RANGES.lock().ok().and_then(|v| {
        v.iter()
            .find(|(b, s)| addr >= *b && addr < b + s)
            .map(|(b, _)| addr - *b)
    })
}

/// Fake Bionic TCB. Guest `.so`s read Bionic TLS slots off TPIDR_EL0 —
/// most visibly the `__stack_chk_guard` slot at +0x28 that
/// `-fstack-protector` emits into every function prologue/epilogue.
/// Darwin leaves TPIDR_EL0 = 0 (it keeps its own TLS in TPIDRRO_EL0),
/// so without a swap every protected guest function faults on its
/// first TLS read. The block is zeroed apart from a self pointer
/// (slot 0) and a fixed stack-guard cookie (slot 5, +0x28) — the same
/// value reads back at function exit, so the check passes.
///
/// The buffer is deliberately far larger than the slots we use: guest
/// code may resolve real Darwin pthread entry points
/// (`pthread_setspecific`, `pthread_mutex_lock`, …) via
/// `dlsym(RTLD_DEFAULT)`, and while TPIDR_EL0 is swapped those index
/// the host `_pthread` struct — whose TSD region reaches several
/// kilobytes past TPIDR — inside this block instead of clobbering
/// whatever Rust statics happen to follow a small array in BSS.
// Per-thread guest TCBs: Darwin leaves TPIDR_EL0 uninitialized on
// spawned threads (observed: 0,1,3,5,8,9 — small garbage, not
// pointers), so a single shared TCB + global saved-host cell raced
// across VM worker threads and corrupted the guest thread pointer —
// `ldr [tpidr,#0x28]` stack-guard reads then faulted on si=0x2c-style
// addresses inside the `.so`. Each thread gets its own 64KiB block;
// slot 4095 (+0x7ff8, the largest offset an `ldr` immediate reaches)
// holds the thread's saved host TPIDR for the env-call thunks.
#[cfg(target_arch = "aarch64")]
const TCB_WORDS: usize = 8192;
#[cfg(target_arch = "aarch64")]
const SLOT_HOST_TPIDR: usize = 4095;
#[cfg(target_arch = "aarch64")]
static TCB_MAP: std::sync::Mutex<Option<std::collections::HashMap<u64, usize>>> =
    std::sync::Mutex::new(None);

/// This thread's guest TCB block — created on first use, leaked
/// (threads outlive VM windows; freeing under a live TPIDR would be
/// worse than the leak).
#[cfg(target_arch = "aarch64")]
pub(crate) fn guest_tcb() -> *mut u64 {
    let tid = unsafe { mach_thread_id() };
    let mut g = TCB_MAP.lock().unwrap();
    let m = g.get_or_insert_with(std::collections::HashMap::new);
    if let Some(&p) = m.get(&tid) {
        return p as *mut u64;
    }
    let tcb = Box::leak(Box::new([0u64; TCB_WORDS]));
    tcb[0] = tcb.as_ptr() as u64; // TLS_SLOT_SELF
    tcb[5] = 0x9e3779b97f4a7c15; // bionic stack guard slot
    m.insert(tid, tcb.as_ptr() as usize);
    // Lock-free parallel list the signal handler can scan: the tid
    // slot is claimed by CAS (check-then-store raced — two threads
    // could take one slot and drop a TCB), then the tcb pointer is
    // filled in. A scanner matching the tid before the store lands
    // sees tcb=0 and falls through to the map.
    for i in (0..TCB_LIST.len()).step_by(2) {
        if TCB_LIST[i + 1]
            .compare_exchange(
                0,
                tid,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_ok()
        {
            TCB_LIST[i].store(
                tcb.as_ptr() as usize as u64,
                std::sync::atomic::Ordering::Release,
            );
            break;
        }
    }
    tcb.as_ptr() as *mut u64
}

/// (tcb, tid) pairs appended at TCB creation; never removed.
#[cfg(target_arch = "aarch64")]
static TCB_LIST: [std::sync::atomic::AtomicU64; 4096] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 4096];

/// Shared zeroed TCB used by the segv healers when the faulting
/// thread's own TCB can't be located (list full, map contended).
#[cfg(target_arch = "aarch64")]
static FALLBACK_TCB: [u64; TCB_WORDS] = [0; TCB_WORDS];

/// Address of FALLBACK_TCB with the self pointer and stack-guard
/// cookie primed (a zero guard trips `__stack_chk_fail` on retry).
#[cfg(target_arch = "aarch64")]
fn fallback_tcb() -> usize {
    let tcb = FALLBACK_TCB.as_ptr() as *mut u64;
    unsafe {
        if *tcb.add(5) == 0 {
            *tcb.add(5) = 0x9e37_79b9_7f4a_7c15;
            *tcb = tcb as u64;
        }
    }
    tcb as usize
}

/// Scribble target for *unproven* healed stores: a wild `str` patched
/// to the live TCB would overwrite the stack-guard cookie or the
/// bookkeeping slots — quarantine those writes here (primed the same
/// so reads from it stay consistent).
#[cfg(target_arch = "aarch64")]
/// TCB for `tid`, scannable from the async-signal handler.
#[cfg(target_arch = "aarch64")]
fn tcb_for(tid: u64) -> usize {
    for i in (0..TCB_LIST.len()).step_by(2) {
        if TCB_LIST[i + 1].load(std::sync::atomic::Ordering::Acquire) == tid {
            let p = TCB_LIST[i].load(std::sync::atomic::Ordering::Acquire) as usize;
            if p != 0 {
                return p;
            }
        }
    }
    // Fallback: the lock-free list can fill before a thread's slot
    // insert runs; the map is authoritative when we can take it
    // without blocking (deadlock risk is why this is only a fallback).
    if let Ok(g) = TCB_MAP.try_lock() {
        if let Some(m) = g.as_ref() {
            if let Some(&p) = m.get(&tid) {
                return p;
            }
        }
    }
    0
}

#[cfg(target_arch = "aarch64")]
unsafe fn mach_thread_id() -> u64 {
    extern "C" {
        fn pthread_threadid_np(t: usize, id: *mut u64) -> i32;
    }
    let mut id = 0u64;
    pthread_threadid_np(0, &mut id);
    id
}

/// Per-site repeated-fault counter for the livelock escape: returns how
/// many times this (thread, pc) pair has faulted. 8 direct-mapped
/// (tid^pc) slots — storm sites alternate between a TLS read and the
/// null deref, so a single counter never accumulates.
#[cfg(target_arch = "aarch64")]
unsafe fn storm_hits(pc: usize) -> usize {
    static KEYS: [std::sync::atomic::AtomicU64; 8] = [
        std::sync::atomic::AtomicU64::new(0),
        std::sync::atomic::AtomicU64::new(0),
        std::sync::atomic::AtomicU64::new(0),
        std::sync::atomic::AtomicU64::new(0),
        std::sync::atomic::AtomicU64::new(0),
        std::sync::atomic::AtomicU64::new(0),
        std::sync::atomic::AtomicU64::new(0),
        std::sync::atomic::AtomicU64::new(0),
    ];
    static CNTS: [std::sync::atomic::AtomicUsize; 8] = [
        std::sync::atomic::AtomicUsize::new(0),
        std::sync::atomic::AtomicUsize::new(0),
        std::sync::atomic::AtomicUsize::new(0),
        std::sync::atomic::AtomicUsize::new(0),
        std::sync::atomic::AtomicUsize::new(0),
        std::sync::atomic::AtomicUsize::new(0),
        std::sync::atomic::AtomicUsize::new(0),
        std::sync::atomic::AtomicUsize::new(0),
    ];
    let tid = mach_thread_id();
    let key = tid ^ (pc as u64);
    let slot = (pc >> 4) & 7;
    if KEYS[slot].load(std::sync::atomic::Ordering::Relaxed) == key {
        CNTS[slot].fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1
    } else {
        KEYS[slot].store(key, std::sync::atomic::Ordering::Relaxed);
        CNTS[slot].store(1, std::sync::atomic::Ordering::Relaxed);
        1
    }
}

/// Last TLS window transitions + last `msr tpidr` writes, for the
/// segv dump: [0..2]=entry site/tid/old, [3..5]=exit site/tid/old,
/// [6]=thunk swap value, [7]=thunk restore value, [8]=window-entry tcb
/// written, [9]=window-exit old written, [10]=tramp tcb written.
/// Read from the crashing thread's handler — single-writer per
/// thread, no lock.
#[cfg(target_arch = "aarch64")]
static TLS_TRACE: [std::sync::atomic::AtomicU64; 12] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

#[cfg(target_arch = "aarch64")]
fn tls_site_id(site: &'static str) -> u64 {
    match site {
        "jni_call" => 1,
        "call_native" => 2,
        "ctors" => 3,
        _ => 9,
    }
}

/// Run `f` with TPIDR_EL0 pointing at this thread's guest TCB, then
/// restore the thread pointer. Must wrap every entry into guest-native
/// code (JNI_OnLoad, DT_INIT*, exported natives).
#[cfg(target_arch = "aarch64")]
pub(crate) fn with_guest_tls_at<R>(site: &'static str, f: impl FnOnce() -> R) -> R {
    // OVELO_NOTLS: debug knob — run guest code on the host TPIDR_EL0 to
    // test whether the swap itself corrupts host state.
    if crate::dbg_flag("OVELO_NOTLS") {
        return f();
    }
    unsafe {
        let tcb = guest_tcb();
        let mut old: usize;
        core::arch::asm!("mrs {}, tpidr_el0", out(reg) old);
        TLS_TRACE[0].store(tls_site_id(site), std::sync::atomic::Ordering::Relaxed);
        TLS_TRACE[1].store(mach_thread_id(), std::sync::atomic::Ordering::Relaxed);
        TLS_TRACE[2].store(old as u64, std::sync::atomic::Ordering::Relaxed);
        TLS_TRACE[3].store(0, std::sync::atomic::Ordering::Relaxed);

        // A clobbered TPIDR (<4KiB) must not be saved as the thread's
        // host TPIDR — the thunk would reinstall garbage on every call.
        if old >= 0x1000 {
            *tcb.add(SLOT_HOST_TPIDR) = old as u64;
        }
        core::arch::asm!("msr tpidr_el0, {t}", t = in(reg) tcb as usize);
        TLS_TRACE[8].store(tcb as usize as u64, std::sync::atomic::Ordering::Relaxed);
        // OVELO_TLSPROBE: allocation inside the swapped window — if the
        // crash is malloc's TPIDR-keyed per-thread magazine state, a
        // plain Vec alloc+drop dies here with the same signature.
        if crate::dbg_flag("OVELO_TLSPROBE") {
            let mut v: Vec<u8> = Vec::with_capacity(4096);
            v.extend_from_slice(&[1u8, 2, 3, 4]);
            std::hint::black_box(v.as_ptr());
            drop(v);
            eprintln!("[tlsprobe] alloc+drop under guest TLS survived");
        }
        // A panic in a thunked env call unwinds through the JIT thunk
        // and these guest frames, skipping the thunk's `msr tpidr_el0`
        // restore — leaving TPIDR at the swapped host value (or whatever
        // the slot held). Catch here so the host TPIDR is restored before
        // the panic resumes to the supervising caller.
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        let mut now: usize;
        core::arch::asm!("mrs {}, tpidr_el0", out(reg) now);
        if now != tcb as usize {
            eprintln!(
                "[tls] {site} window ended tid={} TPIDR={now:#x} (expected {:#x})",
                mach_thread_id(),
                tcb as usize
            );
        }
        core::arch::asm!("msr tpidr_el0, {}", in(reg) old);
        TLS_TRACE[9].store(old as u64, std::sync::atomic::Ordering::Relaxed);
        TLS_TRACE[3].store(tls_site_id(site), std::sync::atomic::Ordering::Relaxed);
        TLS_TRACE[4].store(mach_thread_id(), std::sync::atomic::Ordering::Relaxed);
        TLS_TRACE[5].store(old as u64, std::sync::atomic::Ordering::Relaxed);
        match r {
            Ok(r) => r,
            Err(p) => std::panic::resume_unwind(p),
        }
    }
}

/// Guest `pthread_create`: the Bionic signature matches libsystem's, but
/// the start routine must run under a guest TCB — Darwin never sets
/// TPIDR_EL0 on the new thread, so the `.so`'s first
/// `ldr [tpidr,#0x28]` stack-guard read would fault.
#[cfg(target_arch = "aarch64")]
extern "C" fn sh_pthread_create(
    tid: *mut c_void,
    attr: *const c_void,
    entry: usize,
    arg: usize,
) -> i32 {
    extern "C" {
        fn pthread_create(
            t: *mut c_void,
            a: *const c_void,
            f: extern "C" fn(*mut c_void) -> *mut c_void,
            arg: *mut c_void,
        ) -> i32;
    }
    extern "C" fn tramp(p: *mut c_void) -> *mut c_void {
        let (entry, arg) = unsafe { *Box::from_raw(p as *mut (usize, usize)) };
        eprintln!(
            "[tls] guest pthread start entry={entry:#x} tid={}",
            unsafe { mach_thread_id() }
        );
        unsafe {
            // BSD signal delivery pushes a frame on the faulting
            // thread's SP; with a corrupt SP and no alternate stack
            // the kernel declares the exception unrecoverable and
            // SIGKILLs the task. Every guest thread gets its own
            // sigaltstack so the segv handler always runs.
            #[cfg(target_os = "macos")]
            {
                let mut old = SigStack {
                    ss_sp: std::ptr::null_mut(),
                    ss_size: 0,
                    ss_flags: 0,
                };
                sigaltstack(std::ptr::null(), &mut old);
                if old.ss_sp.is_null() {
                    let stk = mmap(std::ptr::null_mut(), 128 * 1024, 3, 0x1002, -1, 0);
                    if stk as isize != -1 {
                        let ss = SigStack {
                            ss_sp: stk,
                            ss_size: 128 * 1024,
                            ss_flags: 0,
                        };
                        sigaltstack(&ss, std::ptr::null_mut());
                    }
                }
            }
        }
        unsafe {
            // Per-thread guest TCB; env thunks on this thread swap to
            // whatever TPIDR the thread started with (host garbage).
            let tcb = guest_tcb();
            let mut old: usize;
            core::arch::asm!("mrs {}, tpidr_el0", out(reg) old);
            *tcb.add(SLOT_HOST_TPIDR) = old as u64;
            core::arch::asm!("msr tpidr_el0, {t}", t = in(reg) tcb as usize);
            TLS_TRACE[10].store(tcb as usize as u64, std::sync::atomic::Ordering::Relaxed);
            let f: extern "C" fn(usize) -> usize = std::mem::transmute(entry);
            let r = f(arg) as *mut c_void;
            // Restore before pthread exit cleanup — Darwin's TSD
            // destructors and stack teardown dereference TPIDR-relative
            // pthread fields and must not see the guest TCB.
            core::arch::asm!("msr tpidr_el0, {}", in(reg) old);
            TLS_TRACE[9].store(old as u64, std::sync::atomic::Ordering::Relaxed);
            r
        }
    }
    let b = Box::into_raw(Box::new((entry, arg)));
    let r = unsafe { pthread_create(tid, attr, tramp, b as *mut c_void) };
    if r != 0 {
        unsafe { drop(Box::from_raw(b)) }
    }
    r
}

#[cfg(not(target_arch = "aarch64"))]
pub(crate) fn with_guest_tls_at<R>(_site: &'static str, f: impl FnOnce() -> R) -> R {
    f()
}

/// Host-TPIDR call thunks: every function pointer handed to guest
/// code (JNIEnv/JavaVM vtable entries, builtin shims, dlsym'd
/// libSystem imports) is wrapped in a 64-byte stub that swaps
/// TPIDR_EL0 to the calling thread's saved host value for the call's
/// duration — read back out of slot +0x7ff8 of that thread's own guest
/// TCB, so no global cell can race garbage across VM worker threads.
/// Emitted lazily into a MAP_JIT page.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod thunk {
    use std::collections::HashMap;
    use std::ffi::c_void;
    use std::sync::Mutex;

    static MEMO: Mutex<Option<HashMap<usize, usize>>> = Mutex::new(None);

    struct Arena {
        base: usize,
        used: usize,
    }
    static ARENA: Mutex<Option<Arena>> = Mutex::new(None);

    extern "C" {
        fn pthread_jit_write_protect_np(enable: i32);
        fn mmap(
            addr: *mut c_void,
            len: usize,
            prot: i32,
            flags: i32,
            fd: i32,
            off: i64,
        ) -> *mut c_void;
        fn sys_icache_invalidate(start: *mut c_void, len: usize);
    }
    const PROT_RW: i32 = 3;
    const PROT_EXEC: i32 = 4;
    const MAP_PRIV_ANON_JIT: i32 = 0x0002 | 0x1000 | 0x0800;

    // AAPCS64 callers place stack args (9th+) at [sp] for the callee —
    // the thunk must shuttle them past its own frame or the callee
    // would read saved fp/lr as arguments. Frame: 192B = 160B arg copy
    // (up to 20 stack args) + fp/lr at +160.
    //   sub  sp, sp, #192
    //   stp  x29, x30, [sp, #160]
    //   add  x17, sp, #192           // src = caller's stack args
    //   add  x9,  sp, #0             // dst = our frame
    //   ldp  x10, x11, [x17], #16    // ×10 pairs
    //   stp  x10, x11, [x9],  #16
    //   mrs  x29, tpidr_el0          // this thread's guest TCB
    //   ldr  x16, [x29, #0x7ff8]     // saved host TPIDR (per-thread)
    //   msr  tpidr_el0, x16          //   (TPIDRRO was tried: malloc
    //                                  //   dereferences it as its own
    //                                  //   per-thread state and dies)
    //   nop
    //   ldr  x16, +32                // -> literal: target at +140
    //   blr  x16
    //   msr  tpidr_el0, x29          // guest TCB again
    //   ldp  x29, x30, [sp, #160]
    //   add  sp, sp, #192
    //   ret
    const SLOT: usize = 256;
    // One thunk per resolved import — big .so trees (godot+libc++)
    // need tens of thousands.
    const PAGE: usize = 0x400000;

    /// Varargs-call spill stub: JNIEnv `Call*Method`/`NewObject` varargs
    /// entries receive args in x3-x7, d0-d7 and the caller stack. This
    /// stub (run under the host TCB, after the outer thunk swaps) saves
    /// the full register set into a buffer and calls a Rust dispatcher:
    ///   fn(regs: *const u64, stack_args: *const u64, kind: u64) -> u64
    /// regs = [x0..x7 @ +0..64, d0..d7 @ +64..128]. kind selects the
    /// argument layout (obj, obj+clazz or clazz receiver).
    //   sub  sp, sp, #160
    //   stp  x0,x1,[sp];   stp x2,x3,[sp,#16]; stp x4,x5,[sp,#32]; stp x6,x7,[sp,#48]
    //   stp  d0,d1,[sp,#64]; stp d2,d3,[sp,#80]; stp d4,d5,[sp,#96]; stp d6,d7,[sp,#112]
    //   stp  x29,x30,[sp,#128]
    //   mov  x0, sp;  add x1, sp, #160   // regs buf + caller stack args
    //   movz x2, #kind
    //   ldr  x16, lit;  blr x16
    //   fmov d0, x0        // deliver FP results for float/double entries
    //   ldp  x29,x30,[sp,#128];  add sp,sp,#160;  ret
    //   lit: .quad target
    pub(super) fn emit_va(target: usize, kind: u16) -> usize {
        let mut guard = ARENA.lock().unwrap();
        let a = guard.get_or_insert_with(|| unsafe {
            let p = mmap(
                std::ptr::null_mut(),
                PAGE,
                PROT_RW | PROT_EXEC,
                MAP_PRIV_ANON_JIT,
                -1,
                0,
            );
            assert_ne!(p as usize, usize::MAX, "thunk arena mmap failed");
            super::JIT_CODE_BASE.store(p as usize, std::sync::atomic::Ordering::Release);
            super::JIT_CODE_END.store(p as usize + PAGE, std::sync::atomic::Ordering::Release);
            Arena {
                base: p as usize,
                used: 0,
            }
        });
        assert!(a.used + SLOT <= PAGE, "thunk arena exhausted");
        let at = a.base + a.used;
        a.used += SLOT;
        unsafe {
            pthread_jit_write_protect_np(0);
            let w = at as *mut u32;
            let ins: [u32; 20] = [
                0xD102_83FF, // sub sp, sp, #160
                0xA900_07E0,
                0xA901_0FE2,
                0xA902_17E4,
                0xA903_1FE6, // stp x pairs
                0x6D04_07E0,
                0x6D05_0FE2,
                0x6D06_17E4,
                0x6D07_1FE6,                        // stp d pairs
                0xA908_7BFD,                        // stp x29,x30,[sp,#128]
                0x9100_03E0,                        // mov x0, sp
                0x9102_83E1,                        // add x1, sp, #160
                0xD280_0002 | ((kind as u32) << 5), // movz x2, #kind
                0x5800_00D0,                        // ldr x16, +24 (lit at insn 15+... below)
                0xD63F_0200,                        // blr x16
                0x9E67_0000,                        // fmov d0, x0
                0xA948_7BFD,                        // ldp x29,x30,[sp,#128]
                0x9102_83FF,                        // add sp, sp, #160
                0xD65F_03C0,                        // ret
                0x0000_0000,                        // literal slot written below (as u32 pair)
            ];
            for (i, ins) in ins.iter().enumerate() {
                w.add(i).write_volatile(*ins);
            }
            // ldr at insn 13 (byte 52) -> literal at insn 19 (byte 76): off 24
            let lit = (at + 76) as *mut usize;
            lit.write_volatile(target);
            pthread_jit_write_protect_np(1);
            sys_icache_invalidate(at as *mut c_void, SLOT);
        }
        at
    }

    pub(super) fn emit(target: usize) -> usize {
        if let Some(&t) = MEMO.lock().unwrap().as_ref().and_then(|m| m.get(&target)) {
            return t;
        }
        let t = emit_slow(target);
        MEMO.lock()
            .unwrap()
            .get_or_insert_with(HashMap::new)
            .insert(target, t);
        t
    }

    #[cold]
    fn emit_slow(target: usize) -> usize {
        let mut guard = ARENA.lock().unwrap();
        let a = guard.get_or_insert_with(|| unsafe {
            let p = mmap(
                std::ptr::null_mut(),
                PAGE,
                PROT_RW | PROT_EXEC,
                MAP_PRIV_ANON_JIT,
                -1,
                0,
            );
            assert_ne!(p as usize, usize::MAX, "thunk arena mmap failed");
            super::JIT_CODE_BASE.store(p as usize, std::sync::atomic::Ordering::Release);
            super::JIT_CODE_END.store(p as usize + PAGE, std::sync::atomic::Ordering::Release);
            Arena {
                base: p as usize,
                used: 0,
            }
        });
        assert!(a.used + SLOT <= PAGE, "thunk arena exhausted");
        let at = a.base + a.used;
        a.used += SLOT;
        // Record each thunk's `msr` writes into TLS_TRACE[6]/[7] so the
        // segv dump can name the last TPIDR writer. adrp needs the trace
        // cell within ±4GB of the emitted insn — mmap and static data
        // normally land in the same AS cluster; if not, emit nops.
        let trace_stores = |base: usize, insn_off: usize, cell: usize, rt: u32| -> [u32; 2] {
            let addr = &super::TLS_TRACE[cell] as *const _ as usize;
            let pc = base + insn_off;
            let diff = ((addr & !0xFFF) as i64 - (pc & !0xFFF) as i64) / 4096;
            if !(-(1 << 20)..(1 << 20)).contains(&diff) {
                return [0xD503_201F; 2];
            }
            let adrp = 0x9000_0000
                | (((diff as u64 & 3) << 29) as u32)
                | ((((diff as u64) >> 2 & 0x7FFFF) << 5) as u32)
                | 10; // x10
            let poff = (addr & 0xFFF) / 8;
            let str_ = 0xF900_0000 | ((poff as u32) << 10) | (10 << 5) | rt;
            [adrp, str_]
        };
        unsafe {
            pthread_jit_write_protect_np(0);
            let w = at as *mut u32;
            let mut ins = Vec::with_capacity(38);
            ins.extend_from_slice(&[0xD103_03FF, 0xA90A_7BFD, 0x9103_03F1, 0x9100_03E9]);
            for _ in 0..10 {
                ins.extend_from_slice(&[0xA8C1_2E2A, 0xA881_2D2A]);
            }
            ins.extend_from_slice(&[
                0xAA1D_03F0, // 24: mov x16, x29 — guest fp
                0xD53B_D05D, // 25: mrs x29, tpidr_el0 — this thread's TCB
                0xF93F_EFB0, // 26: str x16, [x29, #0x7fd8] — guest fp
                0xF93F_FBBE, // 27: str x30, [x29, #0x7ff0] — guest call site
                0x9103_03F0, // 28: add x16, sp, #192 — guest sp at thunk entry
                0xF93F_EBB0, // 29: str x16, [x29, #0x7fd0] — guest sp
                0xF97F_FFB0, // 30: ldr x16, [x29, #0x7ff8] — saved host TPIDR
                0xD51B_D050, // 31: msr tpidr_el0, x16
            ]);
            ins.extend_from_slice(&trace_stores(at, 128, 6, 16)); // 32-33: TRACE[6]=swap val
            ins.extend_from_slice(&[
                0x5800_0210, // 34: ldr x16, +64 -> target literal at byte 200
                0xF93F_F7B0, // 35: str x16, [x29, #0x7fe8] — import target
                0xF97F_F3B1, // 36: ldr x17, [x29, #0x7fe0] — in-call depth
                0x9100_0631, // 37: add x17, x17, #1
                0xF93F_F3B1, // 38: str x17, [x29, #0x7fe0]
                0xD63F_0200, // 39: blr x16
                0xD51B_D05D, // 40: msr tpidr_el0, x29 — back to guest TCB
                0xF97F_F3B1, // 41: ldr x17, [x29, #0x7fe0] — depth--
                0xD100_0631, // 42: sub x17, x17, #1
                0xF93F_F3B1, // 43: str x17, [x29, #0x7fe0]
            ]);
            ins.extend_from_slice(&trace_stores(at, 176, 7, 29)); // 44-45: TRACE[7]=restore val
            ins.extend_from_slice(&[
                0xA94A_7BFD, // 46: ldp x29,x30,[sp,#168]
                0x9103_03FF, // 47: add sp, sp, #192
                0xD65F_03C0, // 48: ret
                0xD503_201F, // 49: pad before literal
            ]);
            debug_assert_eq!(ins.len(), 50);
            for (i, ins) in ins.iter().enumerate() {
                w.add(i).write_volatile(*ins);
            }
            // Target literal at byte 200, reached by `ldr x16, +64` from
            // insn 34 (byte 136).
            let lit = (at + 200) as *mut usize;
            lit.write_volatile(target);
            pthread_jit_write_protect_np(1);
            sys_icache_invalidate(at as *mut c_void, SLOT);
        }
        at
    }
}

/// Symbol names of thunk targets, for the watchdog's last-call
/// attribution: target addr -> import/shim name.
#[cfg(target_os = "macos")]
static TARGET_NAMES: Mutex<Option<HashMap<usize, String>>> = Mutex::new(None);

/// Record the import name a resolved address belongs to.
#[cfg(target_os = "macos")]
pub(crate) fn name_target(addr: usize, name: &str) {
    if let Ok(mut g) = TARGET_NAMES.lock() {
        g.get_or_insert_with(HashMap::new)
            .insert(addr, name.to_string());
    }
}

/// Import name for a thunk target, if registered.
#[cfg(target_os = "macos")]
pub(crate) fn target_name(addr: usize) -> Option<String> {
    TARGET_NAMES
        .try_lock()
        .ok()
        .and_then(|g| g.as_ref().and_then(|m| m.get(&addr).cloned()))
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub(crate) fn host_thunk(f: usize) -> usize {
    thunk::emit(f)
}

/// Thunk for JNIEnv varargs entries (`Call*Method`, `NewObject`): the
/// outer `host_thunk` swaps TLS; this inner stub spills the variadic
/// register state into a buffer a Rust dispatcher can read.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub(crate) fn va_thunk(f: usize, kind: u16) -> usize {
    thunk::emit(thunk::emit_va(f, kind))
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
pub(crate) fn va_thunk(f: usize, _kind: u16) -> usize {
    f
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
pub(crate) fn host_thunk(f: usize) -> usize {
    f
}

/// TLS self-heal: Darwin rewrites TPIDR_EL0 at kernel boundaries
/// (the handler reads a different TPIDR than the faulting `mrs`
/// produced), so guest `.so` code — and thunks entered from it — can
/// run with a smashed thread pointer. When the fault is a small-base
/// `ldr/str [xN,#imm]` (the bionic `ldr [tpidr,#slot]` shape and the
/// thunk's slot read alike), restore the thread's guest TCB into the
/// saved register and retry. Each smashed access heals itself once;
/// thunk exits re-arm TPIDR via `msr x29` going forward.
/// `mc` points at mcontext64: ss.x[] at +16 (xN = +16+8N).
/// True when `insn` may write GPR `rn` — used while scanning back for
/// the `mrs` that loaded a faulting base register. Over-inclusive by
/// design: an ambiguous instruction ends the scan (no heal), which is
/// the same outcome as before the heal existed.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn clobbers_gpr(insn: u32, rn: u32) -> bool {
    let rd = insn & 31;
    if (insn >> 25) & 0xF == 0x8 || (insn >> 25) & 0xF == 0x9 {
        return rd == rn; // data-processing immediate group (adr … extract)
    }
    if (insn >> 25) & 0x7 == 0x5 {
        return rd == rn; // data-processing register group
    }
    if (insn & 0x3B00_0000) == 0x3900_0000 || (insn & 0x3B20_0000) == 0x3820_0000 {
        // unsigned-imm / unscaled load-store group: only loads write —
        // opc (bits 23:22) 0 is a store (str x8,[sp] between `mrs` and
        // the faulting ldr is exactly the bionic prologue shape).
        return rd == rn && (insn >> 22) & 3 != 0;
    }
    if (insn & 0x3B00_0000) == 0x1800_0000 {
        return rd == rn; // ldr literal always writes
    }
    if (insn & 0x3A00_0000) == 0x2800_0000 {
        // pair class: bit 22 is L — only loads (ldp) write Rt/Rt2.
        return (insn >> 22) & 1 == 1 && (rd == rn || (insn >> 10) & 31 == rn);
    }
    if (insn >> 21) & 0x7FF == 0x6A9 {
        return rd == rn; // MRS into a GPR
    }
    false
}

/// Whether the instruction writes the architectural SP (x31). rd==31
/// is XZR, not SP, for everything except add/sub with S=0 (immediate
/// or extended-register form) — `cmp`/`subs` and logical ops discard
/// into XZR and must not mark SP dirty.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn writes_sp(insn: u32) -> bool {
    if insn & 31 != 31 {
        return false;
    }
    // add/sub immediate (groups 0b1000 plain / 0b1001 MTE-tagged),
    // S=0: rd=31 encodes SP.
    if ((insn >> 25) & 0xF == 0x8 || (insn >> 25) & 0xF == 0x9) && (insn >> 29) & 1 == 0 {
        return true;
    }
    // add/sub extended register (bit21=1 inside the DP-reg group),
    // S=0: rd=31 encodes SP. Shifted-register forms always use XZR.
    (insn >> 25) & 7 == 5 && (insn >> 21) & 1 == 1 && (insn >> 29) & 1 == 0
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
unsafe fn heal_tls_fault(si: usize, insn: u32, mc: *mut u8) -> bool {
    let is_ldr_str = (insn & 0xFFC0_0000) == 0xF940_0000 || (insn & 0xFFC0_0000) == 0xF900_0000;
    if !is_ldr_str {
        return false;
    }
    let rn = ((insn >> 5) & 31) as usize;
    let imm = (((insn >> 10) & 0xfff) * 8) as usize;
    let base = unsafe { *(mc.add(16 + rn * 8) as *const usize) };
    if si != base.wrapping_add(imm) {
        return false;
    }
    // A smashed TPIDR is only a plausible diagnosis when the base
    // register provably held the thread pointer: require an
    // `mrs xN,TPIDR_EL0` to have loaded it, with no clobber and no
    // control flow in between. Otherwise this is a genuine near-null
    // deref and must fault normally — healing a store here would
    // scribble into the guest TCB and keep running corrupted. (The
    // smashed value is not always small — Darwin can leave any
    // garbage in TPIDR, including a pointer into an unmapped hole.)
    let pc = unsafe { *(mc.add(272) as *const usize) };
    let want_mrs = 0xD53B_D040 | rn as u32;
    // `.so` text patched at load reads the TLS base from x18 instead
    // (`mrs xN,TPIDR_EL0` → `mov xN,x18`); both forms prove the base
    // held the thread pointer. The mov form additionally means x18
    // itself was unset — restore it in the context on heal.
    let want_mov = 0xAA12_03E0 | rn as u32;
    let mut x18_heal = false;
    // Stack slots the smashed value may have been spilled to between
    // the `mrs` and the fault: `str xN,[sp/x29,#off]`. The register is
    // patched by the retry, but a spilled copy stays corrupt — the
    // stack-protector epilogue reloads it (`ldr x8,[sp]` then
    // `ldr x8,[x8,#0x28]`) and faults unhealably. Patching the slot
    // keeps the spilled TPIDR consistent with the register.
    // A recorded spill is only patched when the slot still holds the
    // spilled TPIDR at fault time: skip it when sp/x29 was modified
    // between the store and the fault (the slot address moved), or
    // when a later store wrote the same slot (the value is stale).
    let mut spills: [(usize, usize); 4] = [(0, 0); 4]; // (base_reg, byte_offset)
    let mut nspill = 0usize;
    let mut seen: [(usize, usize); 8] = [(0, 0); 8];
    let mut nseen = 0usize;
    let mut dirty = [false; 2]; // [sp(x31), x29]
    let mut proven = false;
    for i in 1..=24usize {
        let addr = pc - i * 4;
        // Never read into an unmapped page: outside the faulting page
        // only scan addresses we know are mapped code.
        if addr & !0xFFF != pc & !0xFFF && !code_mapped(addr) {
            break;
        }
        let b = unsafe { *(addr as *const u32) };
        if b == want_mrs {
            proven = true;
            break;
        }
        if b == want_mov {
            x18_heal = true;
            proven = true;
            break;
        }
        // Writeback addressing on a sp/fp base changes that register:
        // single load/store unscaled group with pre/post index (bit10),
        // or load/store pair with pre/post index (bit23).
        let rb = ((b >> 5) & 31) as usize;
        if rb == 31 || rb == 29 {
            let wb_single = (b & 0x3B20_0000) == 0x3820_0000 && (b >> 10) & 1 == 1;
            let wb_pair = (b & 0x3A00_0000) == 0x2800_0000 && (b >> 23) & 1 == 1;
            if wb_single || wb_pair {
                dirty[usize::from(rb == 29)] = true;
            }
        }
        if writes_sp(b) {
            dirty[0] = true;
        }
        if clobbers_gpr(b, 29) {
            dirty[1] = true;
        }
        // `str x*,[xB,#imm]` scaled (opc=0): any later store to an
        // sp/fp slot invalidates an earlier spill of it; a store of the
        // suspect register to a fresh, unmoved slot is a live spill.
        if (b & 0xFFC0_0000) == 0xF900_0000 && (rb == 31 || rb == 29) {
            let off = (((b >> 10) & 0xFFF) * 8) as usize;
            let dup = seen[..nseen].contains(&(rb, off));
            if !dup && nseen < seen.len() {
                seen[nseen] = (rb, off);
                nseen += 1;
            }
            // Only record when the slot was tracked — an untracked
            // earlier store to it may exist once `seen` is full.
            if (b & 31) as usize == rn
                && !dup
                && nseen <= seen.len()
                && seen[..nseen].contains(&(rb, off))
                && !dirty[usize::from(rb == 29)]
                && nspill < spills.len()
            {
                spills[nspill] = (rb, off);
                nspill += 1;
            }
            continue;
        }
        if ((b >> 25) & 0x7) == 0x2
            || (b & 0xFF00_0000) == 0xD600_0000
            || clobbers_gpr(b, rn as u32)
        {
            break;
        }
    }
    if !proven {
        // A near-null base in guest code is a genuine null-object
        // deref, not a smashed thread pointer (the x18 carrier is dead
        // anyway — signal delivery zeroes it). Injecting the TCB here
        // made the callee run on TCB garbage and kept caller loops on a
        // permanently-null entry alive ~20k faults/s. Emulate instead:
        // `ldr` yields 0 so the usual `cbz` early-out runs, `str` is
        // dropped, and pc advances past the faulting access.
        if !code_mapped(pc) || base >= 0x1000 {
            return false;
        }
        let rt = (insn & 31) as usize;
        if (insn & 0xFFC0_0000) == 0xF940_0000 && rt != 31 {
            unsafe {
                *(mc.add(16 + rt * 8) as *mut usize) = 0;
            }
        }
        unsafe {
            *(mc.add(272) as *mut usize) = pc + 4;
        }
        return true;
    }
    // The value the thread expects depends on whose code faulted:
    // guest `.so`/thunk code runs on the guest TCB, while a host lib
    // entered through a thunk runs on the host TPIDR stashed in the
    // TCB slot — Darwin's boundary rewrite clobbers either one.
    // When the faulting thread's TCB can't be found (list full, map
    // contended), fall back to a shared zeroed block: thunk stores
    // land harmlessly, and the host-TPIDR slot reads 0 so a host-call
    // fault resolves via the dead-call heal.
    let mut tcb = tcb_for(mach_thread_id());
    if tcb == 0 {
        tcb = fallback_tcb();
    }
    let restore = if code_mapped(pc) {
        tcb
    } else {
        unsafe { *(tcb as *const usize).add(SLOT_HOST_TPIDR) }
    };
    if restore == 0 {
        return false;
    }
    unsafe {
        *(mc.add(16 + rn * 8) as *mut usize) = restore;
        for (rb, off) in spills.iter().take(nspill) {
            // mc+248 = saved fp (x29), mc+264 = saved sp. A zeroed or
            // smashed saved base makes `slot` wild — writing `restore`
            // through it faults inside the handler (nested segv → mach
            // exception loop → thread suspended). Only patch a slot
            // that is still writable.
            let base = *(mc.add(if *rb == 31 { 264 } else { 248 }) as *const usize);
            let slot = base.wrapping_add(*off) as *mut usize;
            if base != 0 && mem_writable(slot as *const c_void, 8) {
                *slot = restore;
            }
        }
        // Durable fix: restore the thread's real TPIDR too — patching
        // only this register leaves TPIDR smashed, so every later TLS
        // read faults again (thousands of signal round-trips) or reads
        // silently wrong data when the smashed value happens to map.
        core::arch::asm!("msr tpidr_el0, {}", in(reg) restore);
        if x18_heal {
            // x18 is the TLS carrier — always the real TCB, never the
            // scribble quarantine.
            *(mc.add(16 + 18 * 8) as *mut usize) = if code_mapped(pc) { tcb } else { restore };
        }
        let s = b"[segv-heal]\n";
        write(2, s.as_ptr() as *const c_void, s.len());
    }
    true
}

#[cfg(all(target_os = "macos", not(target_arch = "aarch64")))]
unsafe fn heal_tls_fault(_si: usize, _insn: u32, _mc: *mut u8) -> bool {
    false
}

/// Dead-call heal: the fault sits inside a host routine the guest
/// reached through an import thunk (the thunk's in-call depth at
/// TCB+0x7fe0 is nonzero), with an argument that is already wild —
/// retrying the faulting instruction cannot help, and TPIDR healing
/// is irrelevant because the bad pointer was computed earlier. Skip
/// the call instead: return NULL to the guest call site (TCB+0x7ff0),
/// restore the guest fp (TCB+0x7fd8) and this thread's guest TPIDR
/// (the handler runs on the faulting thread), and pop the depth the
/// thunk's epilogue would have. Better a stubbed call than a dead
/// process.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
unsafe fn heal_dead_thunk_call(mc: *mut u8) -> bool {
    let pc = unsafe { *(mc.add(272) as *const usize) };
    if code_mapped(pc) {
        return false;
    }
    let mut tcb = tcb_for(mach_thread_id());
    if tcb == 0 {
        tcb = fallback_tcb();
    }
    let base = tcb as *const usize;
    let depth = unsafe { *base.add(0x7fe0 / 8) };
    let site = unsafe { *base.add(0x7ff0 / 8) };
    let gfp = unsafe { *base.add(0x7fd8 / 8) };
    if depth == 0 || site == 0 || !code_mapped(site) {
        return false;
    }
    let target = unsafe { *base.add(0x7fe8 / 8) };
    let gsp = unsafe { *base.add(0x7fd0 / 8) };
    unsafe {
        *(tcb as *mut usize).add(0x7fe0 / 8) = depth - 1;
        if gfp != 0 {
            *(mc.add(248) as *mut usize) = gfp; // x29: guest fp
        }
        // Guest sp recorded by the thunk at entry — without it the
        // caller resumes on the interrupted host-frame sp and every
        // sp-relative access reads host stack garbage.
        if gsp != 0 && gsp % 16 == 0 {
            *(mc.add(264) as *mut usize) = gsp; // sp: guest stack
        }
        *(mc.add(16) as *mut usize) = 0; // x0: NULL-ish return
        *(mc.add(256) as *mut usize) = site; // lr
        *(mc.add(272) as *mut usize) = site; // pc: insn after `blr thunk`
        core::arch::asm!("msr tpidr_el0, {}", in(reg) tcb);
        let mut msg = [0u8; 128];
        let p = b"[segv-skipcall] t=";
        msg[..p.len()].copy_from_slice(p);
        let mut n = p.len();
        for i in (0..16).rev() {
            msg[n] = b"0123456789abcdef"[(target >> (i * 4)) & 0xf];
            n += 1;
        }
        msg[n] = b' ';
        n += 1;
        if let Some(name) = target_name(target) {
            for &c in name.as_bytes().iter().take(48) {
                msg[n] = c;
                n += 1;
            }
        }
        msg[n] = b'\n';
        write(2, msg.as_ptr() as *const c_void, n + 1);
    }
    true
}

#[cfg(all(target_os = "macos", not(target_arch = "aarch64")))]
unsafe fn heal_dead_thunk_call(_mc: *mut u8) -> bool {
    false
}

/// Debug crash dump: guest `.so`s run REAL arm64 code — a guest
/// SIGSEGV kills the process silently. A SIG_INFO handler prints the
/// fault address + PC (dladdr-resolved when possible) to stderr so
/// crashes inside loaded libraries are diagnosable. Async-signal-safe
/// ops only (write/dladdr/_exit); installed once per process.
pub fn install_crash_dump() {
    #[cfg(target_os = "macos")]
    unsafe {
        // macOS arm64: ucontext_t → uc_mcontext → mcontext64.
        extern "C" fn on_segv(_sig: i32, _info: *mut c_void, ctx: *mut c_void) {
            unsafe {
                // Prove BSD delivery happened before any op that could
                // itself fault (lock/malloc in eprintln is not safe).
                let mut tag = *b"[segv-in sig=0]\n";
                tag[12] = b'0' + (_sig.clamp(0, 9) as u8);
                write(2, tag.as_ptr() as *const c_void, tag.len());
                // ucontext_t: uc_mcontext at offset 48 → mcontext64:
                // es(16) then ss: x[0..28](232) fp lr sp pc — so
                // sp=+264, pc=+272 from uc_mcontext.
                let mc = *(ctx as *const usize).add(6) as *const u8;
                let pc = *(mc.add(272) as *const usize);
                let sp = *(mc.add(264) as *const usize);
                // siginfo_t: si_addr at offset 24 on 64-bit macOS.
                let si = *(_info as *const usize).add(3);
                // ss.x[] at +16: x0=+16, x8=+80, x20=+176, x30/lr=+256.
                let x0 = *(mc.add(16) as *const usize);
                let x8 = *(mc.add(80) as *const usize);
                let x20 = *(mc.add(176) as *const usize);
                let x30 = *(mc.add(256) as *const usize);
                let fp = *(mc.add(248) as *const usize); // x29
                let inlib = LOADED_RANGES
                    .lock()
                    .map(|v| v.iter().any(|(b, s)| pc >= *b && pc < b + s))
                    .unwrap_or(false);
                // pc may be unmapped (instruction abort on a wild
                // branch): read through Mach, never dereference.
                let insn = safe_read_u32(pc).unwrap_or(0);
                if crate::dbg_flag("OVELO_HEALDBG") {
                    static DBGN: std::sync::atomic::AtomicUsize =
                        std::sync::atomic::AtomicUsize::new(0);
                    let d = DBGN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    // Sample: first 32 faults, then every 50000th —
                    // catches both early sites and the storm phase.
                    if d < 32 || d.is_multiple_of(50_000) {
                        // pc si insn base_reg_val — pinpoints the site
                        // and the garbage carried into the access.
                        let rn = ((insn >> 5) & 31) as usize;
                        let bv = if rn == 31 {
                            sp
                        } else {
                            *(mc.add(16 + rn * 8) as *const usize)
                        };
                        let mut m = *b"[healdbg] pc=................ si=................ in=................ base=................ lr=................ fp=................ clr=................ tp=................ il=. site=................ tgt=................\n";
                        let mut put = |off: usize, v: usize| {
                            for i in 0..16 {
                                m[off + i] = b"0123456789abcdef"[(v >> ((15 - i) * 4)) & 0xf];
                            }
                        };
                        put(13, pc);
                        put(33, si);
                        put(52, insn as usize);
                        put(74, bv);
                        put(94, x30);
                        let clr =
                            if fp != 0 && fp.is_multiple_of(8) && safe_read_u32(fp + 8).is_some() {
                                *(fp as *const usize).add(1)
                            } else {
                                0
                            };
                        put(114, fp);
                        put(135, clr);
                        let mut tpv: usize = 0;
                        #[cfg(target_arch = "aarch64")]
                        core::arch::asm!("mrs {}, tpidr_el0", out(reg) tpv);
                        put(155, tpv);
                        let ilb = if inlib { b'1' } else { b'0' };
                        // The thunk's recorded import target + call
                        // site identify which host call was active
                        // when guest code ran on the host TPIDR.
                        let t = tcb_for(mach_thread_id());
                        let (site, tgt) = if t != 0 {
                            (
                                *(t as *const usize).add(0x7ff0 / 8),
                                *(t as *const usize).add(0x7fe8 / 8),
                            )
                        } else {
                            (0, 0)
                        };
                        put(181, site);
                        put(202, tgt);
                        m[174] = ilb;
                        write(2, m.as_ptr() as *const c_void, m.len());
                        // Walk the frame chain: each frame's saved lr
                        // names the caller — the recurring stack that
                        // re-arms host TPIDR under guest code.
                        let mut bt = *b"[healdbg-bt] ................ ................ ................ ................ ................ ................\n";
                        let mut putb = |off: usize, v: usize| {
                            for i in 0..16 {
                                bt[off + i] = b"0123456789abcdef"[(v >> ((15 - i) * 4)) & 0xf];
                            }
                        };
                        let mut cur = fp;
                        for i in 0..6 {
                            if cur == 0 || !cur.is_multiple_of(8) {
                                break;
                            }
                            let pfp = safe_read_u64(cur).unwrap_or(0) as usize;
                            let plr = safe_read_u64(cur + 8).unwrap_or(0) as usize;
                            putb(13 + i * 17, plr);
                            if pfp <= cur {
                                break;
                            }
                            cur = pfp;
                        }
                        write(2, bt.as_ptr() as *const c_void, bt.len());
                    }
                }
                if insn != 0 {
                    // Livelock escape: when a healed access faults at the
                    // same pc on the same thread over and over, the guest is
                    // spinning on data that never materializes (e.g. a pump
                    // calling a callback on a null object). Healing each
                    // access keeps the loop alive forever at ~20k signal
                    // round-trips/s, so after a per-site cap we suspend
                    // the stuck thread outright.
                    let hits = storm_hits(pc);
                    if hits > 256 {
                        // The site is permanently stuck (e.g. a pump
                        // re-entering on a null entry that never
                        // resolves). Suspending the thread is the only
                        // bounded outcome — the heal would otherwise
                        // keep this one livelock thread at ~20k
                        // signal round-trips/s forever.
                        eprintln!(
                            "[segv-storm] suspend pc={:#x?} si={si:#x} lr={:#x?}",
                            so_off(pc),
                            so_off(x30)
                        );
                        extern "C" {
                            fn mach_thread_self() -> u32;
                            fn thread_suspend(t: u32) -> i32;
                        }
                        thread_suspend(mach_thread_self());
                        return;
                    }
                    if heal_tls_fault(si, insn, mc as *mut u8) {
                        return;
                    }
                }
                if heal_dead_thunk_call(mc as *mut u8) {
                    return;
                }
                // Branch to a null target (pc 0..~4KiB, fault addr 0):
                // nothing on this thread can recover — its own retry
                // just livelocks on the exception port at ~20k/s and
                // BSD delivery can stop entirely. Dump the caller
                // (x30 = return address past the `blr`) and the first
                // guest-frame links, then suspend the thread.
                if pc < 0x1000 && si < 0x1000 {
                    // Guest thread jumped to a null call target: emulate
                    // the callee as a no-op returning 0 — pc := lr —
                    // so pumps that vtable-call a missing entry just
                    // advance. Host threads keep the dump+suspend.
                    if tcb_for(mach_thread_id()) != 0 && code_mapped(x30) {
                        // Same storm bound as the heal path: a pump that
                        // null-calls every iteration would otherwise loop
                        // through SIGSEGV delivery unboundedly. Key on
                        // x30 (the `blr` call site) — pc is ~0 for every
                        // null target, so it can't tell sites apart.
                        if storm_hits(x30) > 256 {
                            extern "C" {
                                fn mach_thread_self() -> u32;
                                fn thread_suspend(t: u32) -> i32;
                            }
                            thread_suspend(mach_thread_self());
                            return;
                        }
                        *(mc.add(16) as *mut usize) = 0; // x0
                        *(mc.add(272) as *mut usize) = x30; // pc := lr
                        let s = b"[segv-nullret]\n";
                        write(2, s.as_ptr() as *const c_void, s.len());
                        return;
                    }
                    let mut m = *b"[segv-null] lr=................ fp=................ l1=................ l2=................\n";
                    let mut put = |off: usize, v: usize| {
                        for i in 0..16 {
                            m[off + i] = b"0123456789abcdef"[(v >> ((15 - i) * 4)) & 0xf];
                        }
                    };
                    put(14, x30);
                    put(30, fp);
                    for (k, off) in [(0usize, 50usize), (1, 68)] {
                        if fp != 0
                            && fp.is_multiple_of(8)
                            && safe_read_u32(fp + 16 * k + 8).is_some()
                        {
                            let l = *(fp as *const usize).add(1 + 2 * k);
                            put(off, l);
                        }
                    }
                    write(2, m.as_ptr() as *const c_void, m.len());
                    extern "C" {
                        fn mach_thread_self() -> u32;
                        fn thread_suspend(t: u32) -> i32;
                    }
                    thread_suspend(mach_thread_self());
                    return;
                }
                // Classify the fault victim: which malloc zone (if any)
                // claims si_addr — g = guest zone, d = default, ? = none.
                // Tells a targeted heap hit apart from a wild address.
                let z = malloc_zone_from_ptr(si as *const c_void);
                let zt = if z == guest_zone() {
                    b'g'
                } else if z == malloc_default_zone() {
                    b'd'
                } else if z.is_null() {
                    b'?'
                } else {
                    b'o'
                };
                // Also which zone owns the faulting PAGE for context.
                let si_img = LOADED_RANGES
                    .lock()
                    .map(|v| v.iter().any(|(b, s)| si >= *b && si < b + s))
                    .unwrap_or(false);
                let x2 = *(mc.add(32) as *const usize);
                let x9 = *(mc.add(88) as *const usize);
                let x16 = *(mc.add(144) as *const usize);
                let x17 = *(mc.add(152) as *const usize);
                let mut msg = [0u8; 832];
                let s = b"[segv] pc=";
                msg[..s.len()].copy_from_slice(s);
                let mut n = s.len();
                for v in [pc, si, sp, x0, x8, x20, x30, insn as usize, inlib as usize] {
                    for i in (0..16).rev() {
                        msg[n] = b"0123456789abcdef"[(v >> (i * 4)) & 0xf];
                        n += 1;
                    }
                    msg[n] = b' ';
                    n += 1;
                }
                // Zone tag, image flag, then extra registers.
                for c in [
                    b'z',
                    b'=',
                    zt,
                    b' ',
                    b'i',
                    b'=',
                    if si_img { b'1' } else { b'0' },
                    b' ',
                ] {
                    msg[n] = c;
                    n += 1;
                }
                for v in [x2, x9, x16, x17] {
                    for i in (0..16).rev() {
                        msg[n] = b"0123456789abcdef"[(v >> (i * 4)) & 0xf];
                        n += 1;
                    }
                    msg[n] = b' ';
                    n += 1;
                }
                // Resolve pc itself the same way lrs are below.
                let pc_so = LOADED_RANGES
                    .lock()
                    .map(|v| {
                        v.iter()
                            .find(|(b, s)| pc >= *b && pc < b + s)
                            .map(|(b, _)| pc - *b)
                    })
                    .unwrap_or(None);
                if let Some(off) = pc_so {
                    for c in b"pc=[so]" {
                        msg[n] = *c;
                        n += 1;
                    }
                    for i in (0..16).rev() {
                        msg[n] = b"0123456789abcdef"[(off >> (i * 4)) & 0xf];
                        n += 1;
                    }
                    msg[n] = b' ';
                    n += 1;
                } else {
                    let mut di: DlInfo = std::mem::zeroed();
                    if dladdr(pc as *const c_void, &mut di) != 0 && !di.dli_fname.is_null() {
                        let off = pc - di.dli_fbase as usize;
                        let mut p = di.dli_fname;
                        let mut end = p;
                        while *end != 0 {
                            end = end.add(1)
                        }
                        let mut last = p;
                        while p != end {
                            if *p == b'/' as i8 {
                                last = p.add(1)
                            }
                            p = p.add(1)
                        }
                        for c in b"pc=<" {
                            msg[n] = *c;
                            n += 1;
                        }
                        let l = (end as usize - last as usize).min(40);
                        std::ptr::copy_nonoverlapping(
                            last as *const u8,
                            msg.as_mut_ptr().add(n),
                            l,
                        );
                        n += l;
                        msg[n] = b'+';
                        n += 1;
                        for i in (0..8).rev() {
                            msg[n] = b"0123456789abcdef"[(off >> (i * 4)) & 0xf];
                            n += 1;
                        }
                        msg[n] = b' ';
                        n += 1;
                    }
                }
                // Guest frame chain: fp → [prev_fp, saved_lr] — best-effort
                // backtrace of the guest/native call path. Each lr is
                // dladdr-resolved to <image>+<off>, or marked [so] when
                // it lands inside a loaded guest library's map.
                let mut f = fp;
                for _ in 0..6 {
                    if f == 0 || !f.is_multiple_of(8) {
                        break;
                    }
                    let nf = *(f as *const usize);
                    let lr = *((f + 8) as *const usize);
                    let so_off = LOADED_RANGES
                        .lock()
                        .map(|v| {
                            v.iter()
                                .find(|(b, s)| lr >= *b && lr < b + s)
                                .map(|(b, _)| lr - *b)
                        })
                        .unwrap_or(None);
                    if let Some(off) = so_off {
                        for c in b"[so]" {
                            msg[n] = *c;
                            n += 1;
                        }
                        for i in (0..16).rev() {
                            msg[n] = b"0123456789abcdef"[(off >> (i * 4)) & 0xf];
                            n += 1;
                        }
                        msg[n] = b' ';
                        n += 1;
                        if nf <= f {
                            break;
                        }
                        f = nf;
                        continue;
                    }
                    let mut di: DlInfo = std::mem::zeroed();
                    if dladdr(lr as *const c_void, &mut di) != 0 && !di.dli_fname.is_null() {
                        let off = lr - di.dli_fbase as usize;
                        let mut p = di.dli_fname;
                        let mut end = p;
                        while *end != 0 {
                            end = end.add(1)
                        }
                        let mut last = p;
                        while p != end {
                            if *p == b'/' as i8 {
                                last = p.add(1)
                            }
                            p = p.add(1)
                        }
                        msg[n] = b'<';
                        n += 1;
                        let l = (end as usize - last as usize).min(40);
                        std::ptr::copy_nonoverlapping(
                            last as *const u8,
                            msg.as_mut_ptr().add(n),
                            l,
                        );
                        n += l;
                        msg[n] = b'+';
                        n += 1;
                        for i in (0..8).rev() {
                            msg[n] = b"0123456789abcdef"[(off >> (i * 4)) & 0xf];
                            n += 1;
                        }
                    } else {
                        for i in (0..16).rev() {
                            msg[n] = b"0123456789abcdef"[(lr >> (i * 4)) & 0xf];
                            n += 1;
                        }
                    }
                    msg[n] = b' ';
                    n += 1;
                    if nf <= f {
                        break;
                    }
                    f = nf;
                }
                // TLS state at crash: live TPIDR_EL0, this thread's id,
                // and the last with_guest_tls_at entry/exit records.
                #[cfg(target_arch = "aarch64")]
                {
                    let mut tp: usize;
                    core::arch::asm!("mrs {}, tpidr_el0", out(reg) tp);
                    for c in b"tp=" {
                        msg[n] = *c;
                        n += 1;
                    }
                    for i in (0..16).rev() {
                        msg[n] = b"0123456789abcdef"[(tp >> (i * 4)) & 0xf];
                        n += 1;
                    }
                    for c in b" tid=" {
                        msg[n] = *c;
                        n += 1;
                    }
                    let tid = mach_thread_id() as usize;
                    for i in (0..16).rev() {
                        msg[n] = b"0123456789abcdef"[(tid >> (i * 4)) & 0xf];
                        n += 1;
                    }
                    for c in b" win=" {
                        msg[n] = *c;
                        n += 1;
                    }
                    for cell in TLS_TRACE.iter() {
                        let v = cell.load(std::sync::atomic::Ordering::Relaxed) as usize;
                        for j in (0..16).rev() {
                            msg[n] = b"0123456789abcdef"[(v >> (j * 4)) & 0xf];
                            n += 1;
                        }
                        msg[n] = b' ';
                        n += 1;
                    }
                }
                msg[n] = b'\n';
                write(2, msg.as_ptr() as *const c_void, n + 1);
                _exit(139);
            }
        }
        if std::env::var_os("OVELO_NOSEGV").is_none() {
            // SA_ONSTACK + a private alternate stack: a guest that
            // corrupts its own SP still gets the handler delivered —
            // without it the kernel can't push the frame and kills the
            // process with an "unrecoverable exception" (SIGKILL, no
            // trace). Map it once per process; each thread would
            // ideally get its own but the main thread covers the dex
            // dispatch path and .so-spawned pthreads rarely fault on
            // a broken stack.
            let mut old = SigStack {
                ss_sp: std::ptr::null_mut(),
                ss_size: 0,
                ss_flags: 0,
            };
            sigaltstack(std::ptr::null(), &mut old);
            if old.ss_sp.is_null() {
                let stk = mmap(
                    std::ptr::null_mut(),
                    128 * 1024,
                    3,      // PROT_READ|PROT_WRITE
                    0x1002, // MAP_PRIVATE|MAP_ANON
                    -1,
                    0,
                );
                if !stk.is_null() && stk as isize != -1 {
                    let ss = SigStack {
                        ss_sp: stk,
                        ss_size: 128 * 1024,
                        ss_flags: 0,
                    };
                    sigaltstack(&ss, std::ptr::null_mut());
                }
            }
            let sa = Sigaction {
                sa_sigaction: on_segv as *const () as usize,
                sa_mask: 0,
                sa_flags: 0x40 | 0x1, // SA_SIGINFO | SA_ONSTACK
            };
            sigaction(11, &sa, std::ptr::null_mut());
            sigaction(10, &sa, std::ptr::null_mut()); // SIGBUS — writes to non-writable guest pages
            sigaction(4, &sa, std::ptr::null_mut()); // SIGILL — bad guest insn
            excport::start();
            excport::start_watchdog();
        }
        let _ = dladdr as *const (); // keep decl referenced
    }
}

/// Flush the instruction cache over a freshly-written code region.
/// macOS arm64 requires `sys_icache_invalidate`; ovelo only runs guest
/// code on macOS/Apple Silicon, elsewhere this is unreachable anyway.
#[cfg(target_os = "macos")]
unsafe fn icache_flush(p: *mut c_void, n: usize) {
    unsafe { sys_icache_invalidate(p, n) };
}
#[cfg(not(target_os = "macos"))]
unsafe fn icache_flush(_p: *mut c_void, _n: usize) {}

const PROT_READ: i32 = 1;
const PROT_WRITE: i32 = 2;
const PROT_EXEC: i32 = 4;
const MAP_PRIVATE: i32 = 0x2;
#[cfg(target_os = "macos")]
const MAP_ANON: i32 = 0x1000;
#[cfg(not(target_os = "macos"))]
const MAP_ANON: i32 = 0x20;
const MAP_FAILED: *mut c_void = !0usize as *mut c_void;
const RTLD_DEFAULT: *mut c_void = (-2isize) as *mut c_void;

const PAGE: usize = 0x4000; // macOS arm64 page

const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;

const DT_NULL: i64 = 0;
const DT_NEEDED: i64 = 1;
const DT_STRTAB: i64 = 5;
const DT_SYMTAB: i64 = 6;
const DT_RELA: i64 = 7;
const DT_RELASZ: i64 = 8;
const DT_STRSZ: i64 = 10;
const DT_INIT: i64 = 12;
const DT_INIT_ARRAY: i64 = 25;
const DT_INIT_ARRAYSZ: i64 = 27;
const DT_JMPREL: i64 = 23;
const DT_PLTRELSZ: i64 = 2;
const DT_PLTGOT: i64 = 3;
const DT_HASH: i64 = 4;
const DT_GNU_HASH: i64 = 0x6fff_fef5;
const DT_ANDROID_REL: i64 = 0x6000_000d;
const DT_ANDROID_RELA: i64 = 0x6000_000f;
const DT_ANDROID_RELSZ: i64 = 0x6000_0010;
const DT_ANDROID_RELASZ: i64 = 0x6000_0011;
const DT_RELR: i64 = 36;
const DT_RELRSZ: i64 = 35;

const R_AARCH64_ABS64: u32 = 257;
const R_AARCH64_GLOB_DAT: u32 = 1025;
const R_AARCH64_JUMP_SLOT: u32 = 1026;
const R_AARCH64_RELATIVE: u32 = 1027;

const SHN_UNDEF: u16 = 0;
const STB_WEAK: u8 = 2;

/// A `.so` mapped into real executable memory. `Drop` unmaps.
pub struct NativeLib {
    map: *mut u8,
    span: usize,
    /// load bias: guest vaddr + bias = real address
    #[allow(dead_code)]
    bias: usize,
    /// exported symbol name → real address
    pub exports: HashMap<String, usize>,
    /// RegisterNatives: (class-desc, name, sig) → real address
    pub registered: HashMap<(String, String, String), usize>,
    pub name: String,
}

unsafe impl Send for NativeLib {}
unsafe impl Sync for NativeLib {}

impl Drop for NativeLib {
    fn drop(&mut self) {
        // Unregister first — later dl_iterate_phdr walks must not
        // report this lib's phdrs, and the global scope must not hand
        // out addresses inside this image, after it is unmapped.
        PHDR_LIBS.lock().unwrap().retain(|l| l.bias != self.bias);
        let (lo, hi) = (self.map as usize, self.map as usize + self.span);
        globals_scrub(&mut GLOBAL_SYMS.lock().unwrap(), lo, hi);
        LOADED_RANGES
            .lock()
            .unwrap()
            .retain(|(b, _)| *b != self.map as usize);
        unsafe { munmap(self.map as *mut c_void, self.span) };
    }
}

fn rd16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes(b[o..o + 2].try_into().unwrap())
}
fn rd32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn rd64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

fn cstr_at(_base: usize, strtab: usize, off: u32) -> String {
    let mut s = Vec::new();
    let mut p = (strtab + off as usize) as *const u8;
    unsafe {
        while *p != 0 {
            s.push(*p);
            p = p.add(1);
        }
    }
    String::from_utf8_lossy(&s).into_owned()
}

fn host_dlsym(name: &str) -> Option<usize> {
    // dlsym prepends the Mach-O '_' itself — pass the ELF name verbatim
    // (`_Znwm` resolves to `__Znwm`, `malloc` to `_malloc`).
    let c = CString::new(name).ok()?;
    if name.starts_with("gl") {
        ensure_gl_framework();
    }
    let p = unsafe { dlsym(RTLD_DEFAULT, c.as_ptr()) };
    (!p.is_null()).then_some(p as usize)
}

/// Most GLES3 entry points exist with the same signature in macOS's
/// OpenGL 4.1 driver. Loading the framework globally once lets the
/// generic `host_dlsym` path satisfy `gl*` imports with real code.
#[cfg(target_os = "macos")]
fn ensure_gl_framework() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| unsafe {
        dlopen(
            c"/System/Library/Frameworks/OpenGL.framework/OpenGL".as_ptr(),
            2 | 8, // RTLD_NOW | RTLD_GLOBAL
        );
    });
}

#[cfg(not(target_os = "macos"))]
fn ensure_gl_framework() {}

/// Bionic/libc++-abi imports libSystem doesn't export under these
/// names. Addresses are filled per-process where they need storage.
extern "C" fn sh_pure_virtual() -> ! {
    eprintln!("[native] __cxa_pure_virtual called");
    std::process::abort()
}
/// Find the epilogue entry for a `__stack_chk_fail` trip: the guest
/// reached us via `bl` inside the fail block, so the thunk's saved x30
/// is the insn right after that `bl`. Scan backward for the `b.cond`
/// whose target lands inside the fail block (the handful of insns
/// ending at saved_lr-4); its fallthrough (pc+4) is the check-passed
/// path — the function's epilogue, which unwinds the frame and returns
/// to the real caller.
#[cfg(target_arch = "aarch64")]
unsafe fn chk_fail_epilogue(fail_ret: usize) -> Option<usize> {
    let block_lo = fail_ret.saturating_sub(64);
    let block_hi = fail_ret.saturating_sub(4);
    let mut p = fail_ret.saturating_sub(8);
    let lo = p.saturating_sub(2048);
    while p > lo {
        if let Some(w) = safe_read_u32(p) {
            // b.cond: 0x54000000 | imm19<<5 | cond (cond < 0xE)
            if w & 0xFF00_0010 == 0x5400_0000 && w & 0xF < 0xE {
                let imm19 = ((w >> 5) & 0x7FFFF) as i32;
                let off = ((imm19 << 13) >> 13) as i64 * 4;
                let tgt = (p as i64 + off) as usize;
                if (block_lo..=block_hi).contains(&tgt) {
                    return Some(p + 4);
                }
            }
        }
        p -= 4;
    }
    None
}
extern "C" fn sh_stack_chk_fail() {
    let tcb = tcb_for(unsafe { mach_thread_id() });
    let site = if tcb != 0 {
        unsafe { *(tcb as *const usize).add(0x7ff0 / 8) }
    } else {
        0
    };
    // Most trips here are our own false positive: a kernel-clobbered
    // x18/TPIDR that happens to point into a mapped page makes the
    // prologue's cookie read silently wrong — the epilogue then
    // detects a "smash" no one committed. `__stack_chk_fail` is
    // noreturn, so resuming at lr would land in the fail block's tail —
    // typically the NEXT function's prologue. Instead emulate the
    // check-passed path: rewrite the thunk's saved x30 to the epilogue
    // address (the `b.cond` fallthrough), which unwinds the frame and
    // returns to the real caller.
    let mut resumed = false;
    #[cfg(target_arch = "aarch64")]
    if tcb != 0 {
        unsafe {
            let depth = *(tcb as *const usize).add(0x7fe0 / 8);
            if depth > 0 {
                let entry_sp = *(tcb as *const usize).add(0x7fd0 / 8);
                // Saved guest x30 sits at thunk_sp+176 = entry_sp-16.
                let saved_lr_slot = entry_sp - 16;
                if let Some(epi) = chk_fail_epilogue(site) {
                    *(saved_lr_slot as *mut usize) = epi;
                    resumed = true;
                }
            }
        }
    }
    eprintln!(
        "[native] __stack_chk_fail site={:#x?} (resumed={})",
        so_off(site),
        resumed
    );
}
extern "C" fn sh_cxa_finalize(_p: *mut c_void) {}
extern "C" fn sh_cxa_atexit(_f: *mut c_void, _a: *mut c_void, _d: *mut c_void) -> i32 {
    0
}
extern "C" fn sh_sincosf(x: f32, s: *mut f32, c: *mut f32) {
    unsafe {
        *s = x.sin();
        *c = x.cos();
    }
}
extern "C" fn sh_sincos(x: f64, s: *mut f64, c: *mut f64) {
    unsafe {
        *s = x.sin();
        *c = x.cos();
    }
}
// FORTIFY `_chk` variants — guest image memory is real host memory, so the
// libc bodies apply directly, but the object-size bound is still enforced:
// Bionic's __chk_fail aborts the process (SIGABRT) when a write/copy would
// exceed the destination's declared size.
extern "C" fn sh_chk_fail() -> ! {
    eprintln!("[native] __chk_fail");
    std::process::abort()
}
extern "C" fn sh_strlen_chk(s: *const i8, slen: usize) -> usize {
    #[cfg(target_os = "macos")]
    let n = if guest_dst_ok(s as usize, 1) {
        unsafe { strlen(s) }
    } else {
        match safe_strlen(s as *const c_void) {
            Some(n) => n,
            None => return 0,
        }
    };
    #[cfg(not(target_os = "macos"))]
    let n = unsafe { strlen(s) };
    if n >= slen {
        sh_chk_fail();
    }
    n
}
// Bionic locale query used by libc++/godot ctype paths; the runtime
// always runs a UTF-8 locale where MB_CUR_MAX is 4.
extern "C" fn sh_mb_cur_max() -> usize {
    4
}
extern "C" fn sh_strcpy_chk(d: *mut i8, s: *const i8, dlen: usize) -> *mut i8 {
    unsafe {
        #[cfg(target_os = "macos")]
        if !guest_dst_ok(s as usize, 1) {
            match safe_strlen(s as *const c_void) {
                Some(n) if n < dlen => return strncpy(d, s, n + 1),
                Some(_) => sh_chk_fail(),
                None => return d,
            }
        }
        if strlen(s) + 1 > dlen {
            sh_chk_fail();
        }
        strcpy(d, s)
    }
}
// Bionic scans at most `slen` bytes: a match or NUL inside the bound
// succeeds even when the buffer is unterminated past it.
extern "C" fn sh_strchr_chk(s: *const i8, c: i32, slen: usize) -> *mut i8 {
    let p = s as *const u8;
    unsafe {
        for i in 0..slen {
            #[cfg(target_os = "macos")]
            if !guest_dst_ok(p as usize, 1) && !src_readable(p.add(i) as *const c_void, 1) {
                return std::ptr::null_mut();
            }
            let b = *p.add(i);
            if b == c as u8 {
                return p.add(i) as *mut i8;
            }
            if b == 0 {
                return std::ptr::null_mut();
            }
        }
        sh_chk_fail();
    }
}
extern "C" fn sh_memmove_chk(
    d: *mut c_void,
    s: *const c_void,
    n: usize,
    dlen: usize,
) -> *mut c_void {
    if n > dlen {
        sh_chk_fail();
    }
    unsafe { memmove(d, s, n) }
}
extern "C" fn sh_strcat_chk(d: *mut i8, s: *const i8, dlen: usize) -> *mut i8 {
    #[cfg(target_os = "macos")]
    {
        let (dl, sl) = (
            safe_strlen(d as *const c_void),
            safe_strlen(s as *const c_void),
        );
        match (dl, sl) {
            (Some(dl), Some(sl)) if dl + sl < dlen => unsafe { strcat(d, s) },
            (Some(_), Some(_)) => sh_chk_fail(),
            _ => d,
        }
    }
    #[cfg(not(target_os = "macos"))]
    unsafe {
        if strlen(d) + strlen(s) + 1 > dlen {
            sh_chk_fail();
        }
        strcat(d, s)
    }
}
// `__vsnprintf_chk(dst, size, flag, dlen, fmt, ap)` — the `flag` arg is
// FORTIFY source info; only the size bound is enforced.
extern "C" fn sh_vsnprintf_chk(
    d: *mut i8,
    size: usize,
    _flag: i32,
    dlen: usize,
    fmt: *const i8,
    ap: *mut c_void,
) -> i32 {
    if size > dlen {
        sh_chk_fail();
    }
    unsafe { vsnprintf(d, size, fmt, ap) }
}
// `__errno()` must return the REAL host errno cell — host libc calls
// (open, read) write failures there and the guest reads them back.
extern "C" fn sh_errno() -> *mut i32 {
    unsafe {
        #[cfg(target_os = "macos")]
        {
            __error()
        }
        #[cfg(not(target_os = "macos"))]
        {
            __errno_location()
        }
    }
}
extern "C" fn sh_assert2(file: *const i8, line: i32, func: *const i8, msg: *const i8) -> ! {
    let (f, g, m) = unsafe {
        (
            std::ffi::CStr::from_ptr(file).to_string_lossy(),
            std::ffi::CStr::from_ptr(func).to_string_lossy(),
            std::ffi::CStr::from_ptr(msg).to_string_lossy(),
        )
    };
    eprintln!("[native] assertion failed {f}:{line} {g}: {m}");
    unsafe { abort() }
}
extern "C" fn sh_zero() -> i64 {
    0
}
extern "C" fn sh_neg1() -> i64 {
    -1
}
#[cfg(target_os = "macos")]
extern "C" fn sh_gettid() -> i32 {
    unsafe {
        let mut id = 0u64;
        pthread_threadid_np(0, &mut id);
        id as i32
    }
}
#[cfg(not(target_os = "macos"))]
extern "C" fn sh_gettid() -> i32 {
    0
}
#[cfg(target_os = "macos")]
extern "C" fn sh_terminate() -> ! {
    eprintln!("[native] cxa handler invoked");
    unsafe { abort() }
}
// Bionic's `__strncpy_chk2` enforces both compile-time bounds:
// n over the dst size truncates, n over the src size reads past the
// source object — FORTIFY aborts on either.
extern "C" fn sh_strncpy_chk2(
    d: *mut i8,
    s: *const i8,
    n: usize,
    dlen: usize,
    max_n: usize,
) -> *mut i8 {
    if n > dlen || n > max_n {
        sh_chk_fail();
    }
    #[cfg(target_os = "macos")]
    if !guest_dst_ok(s as usize, 1) && !src_readable(s as *const c_void, n) {
        guestptr_log("strncpy_chk2-src", s as *mut c_void, n);
        return d;
    }
    unsafe { strncpy(d, s, n) }
}
extern "C" fn sh_system_property_get(_name: *const i8, value: *mut i8) -> i32 {
    unsafe { *value = 0 };
    0
}
/// Bionic `pthread_once_t` is a bare int — handing it to Darwin's
/// `pthread_once` makes the host validate a signature word the guest
/// never wrote and then `__libc_fatal`-trap (a `BRK`: uncatchable,
/// observed as the silent SIGKILL inside `GodotLib_setup`). Keyed
/// guest-side once instead, like the mutex/cond/rwlock shims.
#[cfg(target_os = "macos")]
extern "C" fn sh_pthread_once(ctrl: *mut i32, init: extern "C" fn()) -> i32 {
    if ctrl.is_null() || (init as usize) == 0 {
        return -1;
    }
    static DONE: Mutex<Option<HashMap<usize, bool>>> = Mutex::new(None);
    let key = ctrl as usize;
    if DONE
        .lock()
        .map(|m| m.as_ref().and_then(|h| h.get(&key)) == Some(&true))
        .unwrap_or(false)
    {
        return 0;
    }
    static GATE: Mutex<()> = Mutex::new(());
    let _g = match GATE.lock() {
        Ok(g) => g,
        Err(_) => return -1,
    };
    if let Ok(mut m) = DONE.lock() {
        let h = m.get_or_insert_with(HashMap::new);
        if h.get(&key) == Some(&true) {
            return 0;
        }
        h.insert(key, false);
    }
    // `init` is a guest function pointer — it must run with this
    // thread's guest TCB installed, exactly like DT_INIT entries.
    #[cfg(target_arch = "aarch64")]
    with_guest_tls_at("pthread_once", || init());
    #[cfg(not(target_arch = "aarch64"))]
    init();
    if let Ok(mut m) = DONE.lock() {
        m.get_or_insert_with(HashMap::new).insert(key, true);
    }
    unsafe {
        *ctrl = 1; // bionic done marker
    }
    0
}
extern "C" fn sh_getaffinity(_pid: i32, size: usize, mask: *mut u8) -> i32 {
    unsafe {
        memset(mask as *mut c_void, 0, size);
        let cpus = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        let bytes = cpus.div_ceil(8).min(size);
        for i in 0..bytes {
            *mask.add(i) = 0xff;
        }
        if !cpus.is_multiple_of(8) && bytes > 0 {
            *mask.add(bytes - 1) = (1 << (cpus % 8)) - 1;
        }
    }
    0
}
/// Translate Bionic `O_*` bits (asm-generic values) to the host's.
/// Access-mode bits 0-1 are identical everywhere; named flags differ —
/// e.g. Bionic O_APPEND=0x400 is Darwin's O_TRUNC.
#[cfg(target_os = "macos")]
fn guest_oflags(flags: i32) -> i32 {
    const MAP: &[(i32, i32)] = &[
        (0x00040, 0x00200),   // O_CREAT
        (0x00080, 0x00800),   // O_EXCL
        (0x00100, 0x20000),   // O_NOCTTY
        (0x00200, 0x00400),   // O_TRUNC
        (0x00400, 0x00008),   // O_APPEND
        (0x00800, 0x00004),   // O_NONBLOCK
        (0x01000, 0x400000),  // O_DSYNC
        (0x10000, 0x100000),  // O_DIRECTORY
        (0x20000, 0x00100),   // O_NOFOLLOW
        (0x80000, 0x1000000), // O_CLOEXEC
        (0x101000, 0x00080),  // O_SYNC (Bionic's value includes DSYNC)
        (0x200000, 0x00000),  // O_PATH — no Darwin equivalent
        (0x04000, 0x00000),   // O_DIRECT — drop (Darwin uses fcntl)
        (0x08000, 0x00000),   // O_LARGEFILE — implicit on 64-bit
        (0x40000, 0x00000),   // O_NOATIME — no Darwin equivalent
    ];
    let mut out = flags & 0x3;
    for &(b, d) in MAP {
        if flags & b == b {
            out |= d;
        }
    }
    out
}
#[cfg(not(target_os = "macos"))]
fn guest_oflags(flags: i32) -> i32 {
    flags // Linux host: Bionic values are the native ones
}
extern "C" fn sh_open(path: *const i8, flags: i32, mode: i32) -> i32 {
    unsafe { open(path, guest_oflags(flags), mode) }
}
extern "C" fn sh_open2(path: *const i8, flags: i32) -> i32 {
    unsafe { open(path, guest_oflags(flags)) }
}
/// `memcpy`/`memset` resolved through `host_dlsym` run blind: a guest
/// with a bad dst or length writes straight into host heap. Wrapping
/// them lets `guest_dst_ok` reject (and log) destinations that aren't
/// guest-zone, `.so` image, or stack — the write is skipped so the
/// app keeps running and we see who asked for it.
#[cfg(target_os = "macos")]
fn guestptr_log(tag: &str, d: *mut c_void, n: usize) {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    if N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 20 {
        let tcb = tcb_for(unsafe { mach_thread_id() });
        let site = if tcb != 0 {
            unsafe { *(tcb as *const usize).add(0x7ff0 / 8) }
        } else {
            0
        };
        eprintln!(
            "[guestptr] {tag} dst {d:p}+{n:#x} rd={} not guest/stack/image — skipped (from {:#x?})",
            src_readable(d, n.min(16)),
            so_off(site)
        );
    }
}
/// Read-probe for a possibly-wild memcpy source: a src that fails the
/// guest/stack/image test is usually a legit host pointer, but a wild
/// one crashes inside the real memcpy (observed in Godot's step loop).
/// Probe one byte per page via Mach and cache the verdict per page.
#[cfg(target_os = "macos")]
fn src_readable(p: *const c_void, n: usize) -> bool {
    use std::collections::HashSet;
    static GOOD: std::sync::LazyLock<Mutex<HashSet<usize>>> =
        std::sync::LazyLock::new(|| Mutex::new(HashSet::new()));
    static BAD: std::sync::LazyLock<Mutex<HashSet<usize>>> =
        std::sync::LazyLock::new(|| Mutex::new(HashSet::new()));
    if n == 0 {
        return true;
    }
    let a = p as usize;
    let last = a.saturating_add(n - 1) & !0xfff;
    let mut page = a & !0xfff;
    loop {
        let ok = if GOOD.lock().map(|g| g.contains(&page)).unwrap_or(false) {
            true
        } else if BAD.lock().map(|b| b.contains(&page)).unwrap_or(false) {
            false
        } else {
            let mut got = 0u64;
            let mut byte = 0u8;
            let kr = unsafe {
                mach_vm_read_overwrite(
                    mach_task_self(),
                    page as u64,
                    1,
                    &mut byte as *mut u8 as u64,
                    &mut got,
                )
            };
            let ok = kr == 0 && got == 1;
            let _ = (if ok { &GOOD } else { &BAD }).lock().map(|mut s| {
                if s.len() < 8192 {
                    s.insert(page);
                }
            });
            ok
        };
        if !ok {
            return false;
        }
        if page >= last || page > usize::MAX - 0x1000 {
            return true;
        }
        page += 0x1000;
    }
}
/// strlen that cannot fault: scans through `mach_vm_read_overwrite`
/// page chunks instead of dereferencing. `None` = first page unreadable
/// (wild pointer); a string hitting an unmapped page mid-scan returns
/// the truncated length (caller copies what's there).
#[cfg(target_os = "macos")]
fn safe_strlen(s: *const c_void) -> Option<usize> {
    let mut buf = [0u8; 4096];
    let mut off = 0usize;
    loop {
        let mut got = 0u64;
        let kr = unsafe {
            mach_vm_read_overwrite(
                mach_task_self(),
                s.add(off) as u64,
                4096,
                buf.as_mut_ptr() as u64,
                &mut got,
            )
        };
        if kr != 0 || got == 0 {
            return if off == 0 { None } else { Some(off) };
        }
        let n = got as usize;
        if let Some(i) = buf[..n].iter().position(|&b| b == 0) {
            return Some(off + i);
        }
        if n < 4096 {
            return Some(off + n);
        }
        off += 4096;
        if off > 64 * 1024 * 1024 {
            return Some(off);
        }
    }
}
#[cfg(target_os = "macos")]
extern "C" fn sh_memcpy(d: *mut c_void, s: *const c_void, n: usize) -> *mut c_void {
    if !guest_dst_ok(d as usize, n) {
        guestptr_log("memcpy", d, n);
        return d;
    }
    if !guest_dst_ok(s as usize, 1) && !src_readable(s, n) {
        guestptr_log("memcpy-src", s as *mut c_void, n);
        return d;
    }
    unsafe { memcpy(d, s, n) }
}
#[cfg(target_os = "macos")]
extern "C" fn sh_memmove(d: *mut c_void, s: *const c_void, n: usize) -> *mut c_void {
    if !guest_dst_ok(d as usize, n) {
        guestptr_log("memmove", d, n);
        return d;
    }
    if !guest_dst_ok(s as usize, 1) && !src_readable(s, n) {
        guestptr_log("memmove-src", s as *mut c_void, n);
        return d;
    }
    unsafe { memmove(d, s, n) }
}
#[cfg(target_os = "macos")]
extern "C" fn sh_memset(d: *mut c_void, c: i32, n: usize) -> *mut c_void {
    if !guest_dst_ok(d as usize, n) {
        guestptr_log("memset", d, n);
        return d;
    }
    unsafe { memset(d, c, n) }
}
#[cfg(target_os = "macos")]
extern "C" fn sh_strcpy(d: *mut c_void, s: *const c_void) -> *mut c_void {
    let n = if guest_dst_ok(s as usize, 1) {
        unsafe { strlen(s as *const i8) + 1 }
    } else {
        match safe_strlen(s) {
            Some(n) => n + 1,
            None => {
                guestptr_log("strcpy-src", s as *mut c_void, 0);
                return d;
            }
        }
    };
    if !guest_dst_ok(d as usize, n) {
        guestptr_log("strcpy", d, n);
        return d;
    }
    unsafe { memcpy(d, s, n) }
}
#[cfg(target_os = "macos")]
extern "C" fn sh_strncpy(d: *mut c_void, s: *const c_void, n: usize) -> *mut c_void {
    if !guest_dst_ok(d as usize, n) {
        guestptr_log("strncpy", d, n);
        return d;
    }
    if !guest_dst_ok(s as usize, 1) && !src_readable(s, n) {
        guestptr_log("strncpy-src", s as *mut c_void, n);
        return d;
    }
    unsafe { strncpy(d as *mut i8, s as *const i8, n) as *mut c_void }
}
extern "C" fn sh_memcpy_chk(
    d: *mut c_void,
    s: *const c_void,
    n: usize,
    dlen: usize,
) -> *mut c_void {
    if n > dlen {
        sh_chk_fail();
    }
    unsafe { memcpy(d, s, n) }
}
extern "C" fn sh_memset_chk(d: *mut c_void, c: i32, n: usize, dlen: usize) -> *mut c_void {
    if n > dlen {
        sh_chk_fail();
    }
    unsafe { memset(d, c, n) }
}
// `__read_chk(fd, buf, count, buf_size)` — real read when the request
// fits the caller's declared buffer size.
extern "C" fn sh_read_chk(fd: i32, buf: *mut c_void, n: usize, dlen: usize) -> isize {
    if n > dlen {
        sh_chk_fail();
    }
    unsafe { read(fd, buf, n) }
}

// dl_iterate_phdr: libunwind-style users (glide-webp's crash handler,
// exception tables) walk the loaded objects' program headers. The shim
// reports every guest `.so` loaded so far with a leaked copy of its
// phdrs — guest memory is real memory, so the callback is a plain
// function-pointer call.
struct PhdrLib {
    bias: usize,
    name: CString,
    phdrs: Box<[u8]>,
    phnum: u16,
}
unsafe impl Send for PhdrLib {}
static PHDR_LIBS: Mutex<Vec<Arc<PhdrLib>>> = Mutex::new(Vec::new());

#[repr(C)]
struct DlPhdrInfo {
    dlpi_addr: u64,
    dlpi_name: *const i8,
    dlpi_phdr: *const c_void,
    dlpi_phnum: u16,
    // Bionic extends the struct over releases (adds/subs/tls); report
    // the full modern size with the tail zeroed so callbacks that size
    // their reads by `size` don't touch garbage.
    _pad: [u8; 6],
    dlpi_adds: u64,
    dlpi_subs: u64,
    dlpi_tls_modid: u64,
    dlpi_tls_data: u64,
}

type PhdrCb = unsafe extern "C" fn(*mut c_void, usize, *mut c_void) -> i32;

/// Host symbols a guest may import as *data* — dlsym can't tell
/// OBJECT from FUNC, so keep the classic libc/Bionic variable list:
/// these are read, never called, and must bypass the host-TCB thunk.
fn is_data_sym(name: &str) -> bool {
    matches!(
        name,
        "optarg"
            | "opterr"
            | "optind"
            | "optopt"
            | "optreset"
            | "environ"
            | "__progname"
            | "sys_nerr"
            | "sys_errlist"
            | "timezone"
            | "daylight"
            | "altzone"
            | "tzname"
            | "signgam"
            | "stdin"
            | "stdout"
            | "stderr"
    )
}

extern "C" fn sh_dl_iterate_phdr(cb: *const c_void, data: *mut c_void) -> i32 {
    if cb.is_null() {
        return -1;
    }
    eprintln!("[jni] dl_iterate_phdr cb={cb:p}");
    let cb: PhdrCb = unsafe { std::mem::transmute(cb) };
    // Snapshot under the lock, then run guest callbacks unlocked —
    // libunwind-style callbacks may re-enter dl_iterate_phdr.
    let libs: Vec<Arc<PhdrLib>> = PHDR_LIBS.lock().unwrap().clone();
    for l in libs.iter() {
        let mut info = DlPhdrInfo {
            dlpi_addr: l.bias as u64,
            dlpi_name: l.name.as_ptr(),
            dlpi_phdr: l.phdrs.as_ptr().cast(),
            dlpi_phnum: l.phnum,
            _pad: [0; 6],
            dlpi_adds: 0,
            dlpi_subs: 0,
            dlpi_tls_modid: 0,
            dlpi_tls_data: 0,
        };
        // The callback is guest code — it needs the guest TCB (the shim
        // itself runs on the host TCB inside the thunk).
        let r = with_guest_tls_at("call_native", || unsafe {
            cb(
                &mut info as *mut _ as *mut c_void,
                std::mem::size_of::<DlPhdrInfo>(),
                data,
            )
        });
        if r != 0 {
            return r;
        }
    }
    0
}

// Bionic-only: stores a message shown on abort; we just keep the last
// one for diagnostics.
extern "C" fn sh_set_abort_message(s: *const i8) {
    if s.is_null() {
        return;
    }
    unsafe {
        eprintln!(
            "[native] abort-msg: {}",
            std::ffi::CStr::from_ptr(s).to_string_lossy()
        );
    }
}
// NDK jnigraphics — glide-webp decodes into Android Bitmap objects.
// jobjects are guest heap handles; `jni::vm()` is bound for the
// duration of every native call so these can reach the heap.
#[repr(C)]
struct AndroidBitmapInfo {
    width: u32,
    height: u32,
    stride: u32,
    format: i32,
    flags: u32,
}

fn bitmap_at(handle: usize) -> Option<(u32, u32)> {
    match crate::jni::vm().heap.get(handle as u32) {
        Some(crate::Obj::Host {
            state: crate::HostState::Bitmap { w, h, .. },
            ..
        }) => Some((*w, *h)),
        _ => None,
    }
}

extern "C" fn sh_ab_get_info(_env: usize, bitmap: usize, info: *mut AndroidBitmapInfo) -> i32 {
    if info.is_null() {
        return -1;
    }
    let Some((w, h)) = bitmap_at(bitmap) else {
        return -1;
    };
    unsafe {
        *info = AndroidBitmapInfo {
            width: w,
            height: h,
            stride: w * 4,
            format: 1, // ANDROID_BITMAP_FORMAT_RGBA_8888
            flags: 0,
        };
    }
    0
}

/// Buffers pinned by AndroidBitmap_lockPixels until unlockPixels: the
/// guest `.so` keeps writing through the raw pointer for the whole
/// lock, so the Rc must outlive the Bitmap object even if GC reclaims
/// the host object mid-decode.
struct PinnedRc(#[allow(dead_code)] std::rc::Rc<Vec<u8>>, usize);
// Stored in a static: the value is only pushed/dropped from threads
// that already entered the VM — the refcount is never raced from two
// threads at once in practice.
unsafe impl Send for PinnedRc {}
static LOCKED_PIXELS: Mutex<Vec<PinnedRc>> = Mutex::new(Vec::new());

extern "C" fn sh_ab_lock_pixels(_env: usize, bitmap: usize, addr: *mut *mut c_void) -> i32 {
    if addr.is_null() {
        return -1;
    }
    match crate::jni::vm().heap.get_mut(bitmap as u32) {
        Some(crate::Obj::Host {
            state: crate::HostState::Bitmap { rgba, .. },
            ..
        }) => {
            // make_mut clones on shared refs (COW) — the returned
            // pointer then stays stable through unlockPixels.
            unsafe { *addr = std::rc::Rc::make_mut(rgba).as_mut_ptr().cast() };
            let p = unsafe { *addr } as usize;
            allow_guest_dst(p, rgba.len());
            if let Ok(mut l) = LOCKED_PIXELS.lock() {
                l.push(PinnedRc(rgba.clone(), p));
            }
            0
        }
        _ => -1,
    }
}

extern "C" fn sh_ab_unlock_pixels(_env: usize, _bitmap: usize) -> i32 {
    // Release one pinned buffer — a leaked pin only wastes memory.
    if let Ok(mut l) = LOCKED_PIXELS.lock() {
        if let Some(PinnedRc(_, p)) = l.pop() {
            deny_guest_dst(p);
        }
    }
    0
}

// __android_log_print is variadic; stable Rust can't define variadic
// extern "C" fns, so a fixed 7-arg signature captures the register
// args — stack-spilled extras are ignored and the format goes out
// unsubstituted.
extern "C" fn sh_log_print(
    prio: i32,
    tag: *const i8,
    fmt: *const i8,
    a3: usize,
    a4: usize,
    a5: usize,
    a6: usize,
) -> i32 {
    unsafe {
        let t = if tag.is_null() {
            "?".into()
        } else {
            std::ffi::CStr::from_ptr(tag).to_string_lossy()
        };
        let f = if fmt.is_null() {
            "?".into()
        } else {
            std::ffi::CStr::from_ptr(fmt).to_string_lossy()
        };
        // Minimal %s/%d/%i/%u/%x/%p/%f substitution over the four
        // captured register args — enough to read Godot's error text.
        let arg_arr = [a3, a4, a5, a6];
        let mut args = arg_arr.iter().copied();
        let mut out = String::with_capacity(f.len() + 64);
        let mut ch = f.chars().peekable();
        while let Some(c) = ch.next() {
            if c != '%' {
                out.push(c);
                continue;
            }
            if ch.peek() == Some(&'%') {
                ch.next();
                out.push('%');
                continue;
            }
            // Skip flags/width/precision/length modifiers.
            while matches!(ch.peek(), Some(&m) if m.is_ascii_alphanumeric() && !matches!(m, 's'|'d'|'i'|'u'|'x'|'X'|'p'|'f'|'c') || matches!(m, '.'|'-'|'+'|' '|'#'))
            {
                ch.next();
            }
            match ch.next() {
                Some('s') => {
                    let p = args.next().unwrap_or(0) as *const u8;
                    let s = if p.is_null() {
                        "(null)".into()
                    } else if safe_strlen(p as *const c_void).is_none() {
                        format!("({:#x}?)", p as usize)
                    } else {
                        // Godot error args are CharString/Char16 — sniff
                        // UTF-16/32 (ASCII followed by NULs) and decode.
                        let b0 = *p;
                        let b1 = *p.add(1);
                        let b2 = *p.add(2);
                        let b3 = *p.add(3);
                        if b1 == 0 && b2 == 0 && b3 == 0 && b0.is_ascii_graphic() {
                            (0..256usize)
                                .map(|i| *(p as *const u32).add(i))
                                .take_while(|&c| c != 0)
                                .filter_map(char::from_u32)
                                .collect()
                        } else if b1 == 0 && b0.is_ascii_graphic() {
                            (0..512usize)
                                .map(|i| *(p as *const u16).add(i))
                                .take_while(|&c| c != 0)
                                .map(|c| char::from_u32(c as u32).unwrap_or('?'))
                                .collect()
                        } else {
                            std::ffi::CStr::from_ptr(p as *const i8)
                                .to_string_lossy()
                                .to_string()
                        }
                    };
                    out.push_str(&s);
                }
                Some('d') | Some('i') => {
                    out.push_str(&(args.next().unwrap_or(0) as i64).to_string());
                }
                Some('u') | Some('x') | Some('X') | Some('p') => {
                    out.push_str(&format!("{:x}", args.next().unwrap_or(0)));
                }
                Some('f') => {
                    out.push_str(&format!("{}", args.next().unwrap_or(0) as f64));
                }
                Some('c') => {
                    out.push(char::from_u32(args.next().unwrap_or(0) as u32).unwrap_or('?'));
                }
                Some(other) => {
                    out.push('%');
                    out.push(other);
                }
                None => out.push('%'),
            }
        }
        eprintln!("[alog:{prio}] {t}: {out}");
    }
    0
}
extern "C" fn sh_log_write(prio: i32, tag: *const i8, text: *const i8) -> i32 {
    sh_log_print(prio, tag, text, 0, 0, 0, 0)
}
/// Guest-private malloc zone. Guest `.so` code runs in this address
/// space and its imports resolve to real host functions — if `malloc`
/// resolved straight to libSystem's default zone, a guest heap bug
/// (overflow, double free) would corrupt the *host* heap and kill an
/// unrelated allocation later (observed as nondeterministic segvs in
/// `format!`/cstr_ro while loading libglide-webp). Routing all guest
/// allocation through its own zone confines that damage to the guest
/// heap: host code keeps running and a real bug surfaces as a crash
/// inside zone functions with `[so]` frames pointing at the culprit.
#[cfg(target_os = "macos")]
pub(crate) fn guest_zone() -> *mut c_void {
    static ZONE: OnceLock<usize> = OnceLock::new();
    *ZONE.get_or_init(|| unsafe { malloc_create_zone(0, 0) as usize }) as *mut c_void
}

/// Is `p..p+len` a place guest code may legitimately have us write?
/// Guest buffers live in the guest zone, inside a loaded `.so` image
/// (BSS/statics), or on the calling thread's stack — guest natives
/// run on the host stack. Anything else means the guest handed us a
/// wild pointer; writing through it lands in host heap metadata.
#[cfg(target_os = "macos")]
pub(crate) fn guest_dst_ok(p: usize, len: usize) -> bool {
    if p == 0 || len == 0 {
        return p != 0 || len == 0;
    }
    if unsafe { malloc_zone_from_ptr(p as *const c_void) } == guest_zone() {
        return true;
    }
    // Host-heap buffers handed to the guest (strdup, realpath, host
    // callback buffers): malloc_zone_from_ptr resolves any address
    // inside a zone's bookkeeping — a wild pointer almost never lands
    // in one, so this admits real allocations only.
    if !unsafe { malloc_zone_from_ptr(p as *const c_void) }.is_null() {
        return true;
    }
    if LOADED_RANGES
        .lock()
        .map(|v| v.iter().any(|(b, s)| p >= *b && p + len <= b + s))
        .unwrap_or(false)
    {
        return true;
    }
    if EXTRA_DST
        .lock()
        .map(|v| v.iter().any(|(b, s)| p >= *b && p + len <= b + s))
        .unwrap_or(false)
    {
        return true;
    }
    unsafe {
        let t = pthread_self();
        let top = pthread_get_stackaddr_np(t) as usize;
        let size = pthread_get_stacksize_np(t);
        if p >= top.saturating_sub(size) && p.saturating_add(len) <= top {
            return true;
        }
    }
    // Cold path: a real writable mapping the other tests missed —
    // guest mmap/arena buffers. Wild pointers fail the region probe.
    mem_writable(p as *const c_void, len)
}

/// Host-owned ranges the guest may legitimately write through —
/// buffers we hand it, like AndroidBitmap_lockPixels' pixel Vec
/// (not in the guest zone, not in a .so, not on the stack).
#[cfg(target_os = "macos")]
static EXTRA_DST: Mutex<Vec<(usize, usize)>> = Mutex::new(Vec::new());

/// Permit guest writes through `p..p+len` (a host buffer we own).
#[cfg(target_os = "macos")]
pub(crate) fn allow_guest_dst(p: usize, len: usize) {
    if let Ok(mut v) = EXTRA_DST.lock() {
        v.push((p, len));
    }
}

/// Drop a previously allowed range (e.g. on unlockPixels).
#[cfg(target_os = "macos")]
pub(crate) fn deny_guest_dst(p: usize) {
    if let Ok(mut v) = EXTRA_DST.lock() {
        v.retain(|(b, _)| *b != p);
    }
}

/// Non-macOS: no guest-dst bookkeeping.
#[cfg(not(target_os = "macos"))]
pub(crate) fn allow_guest_dst(_p: usize, _len: usize) {}
/// Non-macOS: no guest-dst bookkeeping.
#[cfg(not(target_os = "macos"))]
fn deny_guest_dst(_p: usize) {}
/// Non-macOS: no zone bookkeeping — accept everything.
#[cfg(not(target_os = "macos"))]
pub(crate) fn guest_dst_ok(_p: usize, _len: usize) -> bool {
    true
}

/// `malloc_zone_check` every zone in the process; abort at the first
/// bad one with the milestone tag. Any caller is a milestone — the
/// tag it prints bounds the corruption window to "since the previous
/// milestone". OVELO_ZONECHECK-gated.
#[cfg(target_os = "macos")]
pub(crate) fn check_all_zones(tag: &str) {
    if !crate::dbg_flag("OVELO_ZONECHECK") {
        return;
    }
    unsafe {
        extern "C" {
            fn malloc_zone_check(zone: *mut c_void) -> bool;
            fn malloc_get_all_zones(
                task: u32,
                reader: usize,
                zones: *mut *mut c_void,
                count: *mut u32,
            ) -> i32;
            fn mach_task_self() -> u32;
        }
        let mut zs: *mut c_void = std::ptr::null_mut();
        let mut n: u32 = 0;
        if malloc_get_all_zones(mach_task_self(), 0, &mut zs, &mut n) != 0 {
            eprintln!("[zonecheck] {tag}: get_all_zones failed");
            return;
        }
        let rz = crate::RUST_ZONE.load(std::sync::atomic::Ordering::Relaxed) as *mut c_void;
        let dz = malloc_default_zone();
        let gz = guest_zone();
        for i in 0..n as usize {
            let z = *(zs as *mut *mut c_void).add(i);
            let nm = if z == rz {
                "rust"
            } else if z == dz {
                "default"
            } else if z == gz {
                "guest"
            } else {
                "?"
            };
            // Print BEFORE checking — when the check itself faults
            // inside the corrupt freelist, the last printed name is
            // the bad zone.
            eprintln!("[zonecheck] {tag}: checking {nm} zone {z:?}");
            if !malloc_zone_check(z) {
                eprintln!("[zonecheck] {tag}: BAD {nm} ZONE {z:?}");
                std::process::abort();
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn check_all_zones(_tag: &str) {}

/// Allocate in the guest zone — for buffers host code hands to guest
/// code (Get*ArrayElements, GetStringCritical, …). A guest-side overrun
/// then damages guest metadata, not the Rust heap, and faults inside
/// zone functions with a `[so]` frame naming the culprit.
#[cfg(target_os = "macos")]
pub(crate) fn guest_malloc(size: usize) -> usize {
    let extra = if crate::dbg_flag("OVELO_FREEDBG") {
        16
    } else {
        0
    };
    let p = unsafe { malloc_zone_malloc(guest_zone(), size.max(1) + extra) as usize };
    dbg_alloc(p, size.max(1));
    p
}

/// Free wherever the pointer actually lives — buffers from either zone.
#[cfg(target_os = "macos")]
pub(crate) fn guest_free(p: usize) {
    if p == 0 {
        return;
    }
    if dbg_free(p, "guest_free") {
        return; // quarantined
    }
    unsafe {
        let zone = malloc_zone_from_ptr(p as *const c_void);
        if !zone.is_null() {
            malloc_zone_free(zone, p as *mut c_void);
        } else {
            free(p as *mut c_void);
        }
    }
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn guest_malloc(size: usize) -> usize {
    extern "C" {
        fn malloc(n: usize) -> *mut c_void;
    }
    unsafe { malloc(size.max(1)) as usize }
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn guest_free(p: usize) {
    extern "C" {
        fn free(p: *mut c_void);
    }
    if p != 0 {
        unsafe { free(p as *mut c_void) }
    }
}

#[cfg(target_os = "macos")]
static LIVE_ALLOCS: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<usize, usize>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));
/// ptr -> poisoned region length. Under OVELO_FREEDBG frees are
/// quarantined: the block is poisoned and STAYS allocated, so a
/// use-after-free write lands in memory malloc no longer manages and
/// shows up as a poison violation instead of freelist damage.
#[cfg(target_os = "macos")]
static FREED_ALLOCS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<usize, usize>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));
#[cfg(target_os = "macos")]
const GDB_CANARY: u64 = 0x0e10_dead_c0de_f00d;
/// Bytes the guest may legally write — the real usable size of the
/// block (`malloc_size`), which exceeds the request on all but the
/// tightest buckets. The canary sits at the END of the usable region:
/// a write between request size and usable size is legal and must not
/// trip the alarm.
#[cfg(target_os = "macos")]
fn dbg_alloc(p: usize, size: usize) {
    if p == 0 || std::env::var_os("OVELO_FREEDBG").is_none() {
        return;
    }
    let usable = unsafe { malloc_size(p as *const c_void) }.max(size);
    let off = usable.saturating_sub(8);
    unsafe { ((p + off) as *mut u64).write_unaligned(GDB_CANARY) };
    if let Ok(mut m) = LIVE_ALLOCS.lock() {
        m.insert(p, off);
    }
}
/// Check every live guest buffer's tail canary; report the first
/// smashed one. Called at every env entry under OVELO_FREEDBG.
#[cfg(target_os = "macos")]
pub(crate) fn dbg_check_all(who: &'static str) {
    if std::env::var_os("OVELO_FREEDBG").is_none() {
        return;
    }
    if let Ok(m) = LIVE_ALLOCS.lock() {
        for (p, off) in m.iter() {
            let got = unsafe { ((*p + *off) as *const u64).read_unaligned() };
            if got != GDB_CANARY {
                let so = so_off(*p + *off);
                eprintln!(
                    "[freedbg] {who}: CANARY SMASHED at {:#x} (buf {:#x}+{}) (so_off={so:?})",
                    *p + *off,
                    *p,
                    *off
                );
                break;
            }
        }
    }
    if let Ok(m) = FREED_ALLOCS.lock() {
        for (p, len) in m.iter() {
            // Sample the whole poisoned body (cap 64KiB): any non-0xEE
            // byte is a stale write into a quarantined block.
            let scan = (*len).min(65536);
            let bad = unsafe {
                std::slice::from_raw_parts(*p as *const u8, scan)
                    .iter()
                    .position(|b| *b != 0xEE)
            };
            if let Some(d) = bad {
                let so = so_off(*p + d);
                eprintln!(
                    "[freedbg] {who}: WRITE-AFTER-FREE at {:#x} (buf {:#x}+{d} of {len}) (so_off={so:?})",
                    *p + d,
                    *p
                );
                break;
            }
        }
    }
}
/// A free of a pointer we never handed out — or a double free — is the
/// classic zone corruptor; flag it under OVELO_FREEDBG.
#[cfg(target_os = "macos")]
/// Returns true when the caller should SKIP the real free — tracked
/// blocks are poisoned and kept (quarantine), so use-after-free
/// writes trip dbg_check_all instead of corrupting the freelist.
#[cfg(target_os = "macos")]
fn dbg_free(p: usize, who: &'static str) -> bool {
    if std::env::var_os("OVELO_FREEDBG").is_none() {
        return false;
    }
    unsafe {
        let z = malloc_zone_from_ptr(p as *mut c_void);
        if !z.is_null() && z != guest_zone() {
            // The splice lands in a NON-guest zone — this is the write
            // that corrupts the Rust/default heap.
            let so = so_off(p);
            eprintln!("[freedbg] {who}: free into FOREIGN zone ptr={p:#x} (so_off={so:?})");
            return false;
        }
    }
    let mut m = LIVE_ALLOCS.lock().unwrap_or_else(|e| e.into_inner());
    match m.remove(&p) {
        Some(off) => {
            let got = unsafe { ((p + off) as *const u64).read_unaligned() };
            if got != GDB_CANARY {
                let so = so_off(p + off);
                eprintln!("[freedbg] {who}: canary smashed freeing {p:#x}+{off} (so_off={so:?})");
            }
            unsafe { std::ptr::write_bytes(p as *mut u8, 0xEE, off) };
            FREED_ALLOCS
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(p, off);
            true
        }
        None => {
            let so = so_off(p);
            eprintln!("[freedbg] {who}: free of untracked/double {p:#x} (so_off={so:?})");
            false
        }
    }
}

#[cfg(target_os = "macos")]
extern "C" fn sh_malloc(size: usize) -> *mut c_void {
    let extra = if crate::dbg_flag("OVELO_FREEDBG") {
        16
    } else {
        0
    };
    let p = unsafe { malloc_zone_malloc(guest_zone(), size.max(1) + extra) };
    dbg_alloc(p as usize, size.max(1));
    p
}

/// Route the free to whichever zone owns the pointer — covers the
/// (intentional or accidental) case of a guest freeing memory that a
/// shim or a host-resolved libc call (strdup, realpath) returned from
/// the default zone.
#[cfg(target_os = "macos")]
extern "C" fn sh_free(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    if dbg_free(ptr as usize, "sh_free") {
        return; // quarantined
    }
    unsafe {
        let zone = malloc_zone_from_ptr(ptr);
        if zone.is_null() {
            // Not a malloc-family pointer — let real free() abort at
            // this call site so the guest frame is the one that faults.
            eprintln!("[shim] free() of non-malloc pointer {ptr:p}");
            free(ptr);
            return;
        }
        malloc_zone_free(zone, ptr);
    }
}

#[cfg(target_os = "macos")]
extern "C" fn sh_calloc(n: usize, size: usize) -> *mut c_void {
    let extra = if crate::dbg_flag("OVELO_FREEDBG") {
        16
    } else {
        0
    };
    let Some(total) = n
        .max(1)
        .checked_mul(size.max(1))
        .and_then(|t| t.checked_add(extra))
    else {
        return std::ptr::null_mut(); // overflow — calloc must fail, not wrap
    };
    let p = unsafe { malloc_zone_calloc(guest_zone(), 1, total) };
    dbg_alloc(p as usize, total);
    p
}

#[cfg(target_os = "macos")]
extern "C" fn sh_realloc(ptr: *mut c_void, size: usize) -> *mut c_void {
    if crate::dbg_flag("OVELO_FREEDBG") {
        // realloc can't grow the tail canary in place — do alloc+copy+free
        // so every live buffer keeps its canary at p+size.
        if ptr.is_null() {
            return sh_malloc(size);
        }
        let old_size = LIVE_ALLOCS
            .lock()
            .ok()
            .and_then(|m| m.get(&(ptr as usize)).copied())
            .unwrap_or(0);
        let np = sh_malloc(size);
        if np.is_null() {
            return np; // realloc keeps the old block live on failure
        }
        if old_size > 0 {
            unsafe { std::ptr::copy_nonoverlapping(ptr, np, old_size.min(size)) };
        }
        sh_free(ptr);
        return np;
    }
    unsafe {
        if ptr.is_null() {
            malloc_zone_malloc(guest_zone(), size)
        } else {
            let zone = malloc_zone_from_ptr(ptr);
            let zone = if zone.is_null() { guest_zone() } else { zone };
            malloc_zone_realloc(zone, ptr, size)
        }
    }
}

#[cfg(target_os = "macos")]
extern "C" fn sh_memalign(align: usize, size: usize) -> *mut c_void {
    let extra = if crate::dbg_flag("OVELO_FREEDBG") {
        16
    } else {
        0
    };
    let p = unsafe { malloc_zone_memalign(guest_zone(), align.max(16), size + extra) };
    dbg_alloc(p as usize, size);
    p
}

#[cfg(target_os = "macos")]
extern "C" fn sh_valloc(size: usize) -> *mut c_void {
    unsafe { malloc_zone_memalign(guest_zone(), PAGE, size) }
}

#[cfg(target_os = "macos")]
extern "C" fn sh_pvalloc(size: usize) -> *mut c_void {
    unsafe { malloc_zone_memalign(guest_zone(), PAGE, size.div_ceil(PAGE) * PAGE) }
}

/// Linux `mremap` — no Darwin equivalent; always behaves like
/// MREMAP_MAYMOVE: fresh zone buffer, copy the live prefix, free old.
/// Callers that mmap'd the old range get a heap block back — our own
/// `free`/`realloc` shims handle it, and guest `munmap` is a stub below.
#[cfg(target_os = "macos")]
extern "C" fn sh_mremap(
    old: *mut c_void,
    old_size: usize,
    new_size: usize,
    _flags: i32,
) -> *mut c_void {
    let np = unsafe { malloc_zone_malloc(guest_zone(), new_size.max(1)) };
    if np.is_null() {
        return usize::MAX as *mut c_void; // MAP_FAILED
    }
    if !old.is_null() {
        unsafe { std::ptr::copy_nonoverlapping(old, np, old_size.min(new_size)) };
        sh_free(old);
    }
    np
}

/// Guest `mmap`/`munmap` for code paths that treat the result as a
/// plain buffer (sqlite's shm/remap). `mmap` hands out a zone block —
/// no file mapping; `munmap` frees it.
#[cfg(target_os = "macos")]
extern "C" fn sh_guest_mmap(
    _addr: *mut c_void,
    len: usize,
    _prot: i32,
    _flags: i32,
    fd: i32,
    _off: i64,
) -> *mut c_void {
    if fd >= 0 {
        // File-backed mappings are not emulated — shared-memory
        // consumers (sqlite WAL) fall back to heap when we fail.
        return usize::MAX as *mut c_void;
    }
    let p = unsafe { malloc_zone_calloc(guest_zone(), 1, len.max(1)) };
    if p.is_null() {
        usize::MAX as *mut c_void
    } else {
        p
    }
}

#[cfg(target_os = "macos")]
extern "C" fn sh_guest_munmap(addr: *mut c_void, _len: usize) -> i32 {
    if !addr.is_null() && addr as usize != usize::MAX {
        sh_free(addr);
    }
    0
}

#[cfg(target_os = "macos")]
extern "C" fn sh_posix_memalign(out: *mut *mut c_void, align: usize, size: usize) -> i32 {
    if out.is_null() || align < 8 || !align.is_power_of_two() {
        return 22; // EINVAL
    }
    unsafe {
        *out = malloc_zone_memalign(guest_zone(), align, size);
        if (*out).is_null() {
            12 // ENOMEM
        } else {
            0
        }
    }
}

#[cfg(target_os = "macos")]
extern "C" fn sh_malloc_usable_size(ptr: *const c_void) -> usize {
    if ptr.is_null() {
        return 0;
    }
    unsafe { malloc_size(ptr) }
}

#[cfg(target_os = "macos")]
extern "C" fn sh_strdup(s: *const i8) -> *mut i8 {
    if s.is_null() {
        return std::ptr::null_mut();
    }
    unsafe {
        let n = if guest_dst_ok(s as usize, 1) {
            strlen(s) + 1
        } else {
            match safe_strlen(s as *const c_void) {
                Some(l) => l + 1,
                None => return std::ptr::null_mut(),
            }
        };
        let extra = if crate::dbg_flag("OVELO_FREEDBG") {
            16
        } else {
            0
        };
        let p = malloc_zone_malloc(guest_zone(), n + extra) as *mut i8;
        if !p.is_null() {
            memcpy(p.cast(), s.cast(), n);
        }
        dbg_alloc(p as usize, n);
        p
    }
}

#[cfg(target_os = "macos")]
extern "C" fn sh_strndup(s: *const i8, n: usize) -> *mut i8 {
    if s.is_null() {
        return std::ptr::null_mut();
    }
    unsafe {
        // strnlen semantics: stop at n even without a terminator —
        // strlen could read past the caller's bounded buffer.
        let mut len = 0;
        while len < n {
            if !guest_dst_ok(s as usize, 1) && !src_readable(s.add(len) as *const c_void, 1) {
                break;
            }
            if *s.add(len) == 0 {
                break;
            }
            len += 1;
        }
        let extra = if crate::dbg_flag("OVELO_FREEDBG") {
            16
        } else {
            0
        };
        let p = malloc_zone_malloc(guest_zone(), len + 1 + extra) as *mut i8;
        if !p.is_null() {
            memcpy(p.cast(), s.cast(), len);
            *p.add(len) = 0;
        }
        dbg_alloc(p as usize, len + 1);
        p
    }
}

#[cfg(target_os = "macos")]
extern "C" fn sh_guest_new(size: usize) -> *mut c_void {
    // operator new must not return null — spin until it succeeds is
    // what libc++ does under -fno-exceptions; for us a null means the
    // guest heap is genuinely exhausted, so abort loudly instead.
    let extra = if crate::dbg_flag("OVELO_FREEDBG") {
        16
    } else {
        0
    };
    let p = unsafe { malloc_zone_malloc(guest_zone(), size.max(1) + extra) };
    if p.is_null() {
        eprintln!("[shim] guest operator new({size}) failed");
        std::process::abort();
    }
    dbg_alloc(p as usize, size.max(1));
    p
}

#[cfg(target_os = "macos")]
extern "C" fn sh_guest_new_aligned(size: usize, align: usize) -> *mut c_void {
    let extra = if crate::dbg_flag("OVELO_FREEDBG") {
        16
    } else {
        0
    };
    let p = unsafe { malloc_zone_memalign(guest_zone(), align.max(16), size.max(1) + extra) };
    if p.is_null() {
        eprintln!("[shim] guest aligned operator new({size},{align}) failed");
        std::process::abort();
    }
    dbg_alloc(p as usize, size.max(1));
    p
}

/// nothrow new variants must return null on failure, never abort.
#[cfg(target_os = "macos")]
extern "C" fn sh_guest_new_nothrow(size: usize) -> *mut c_void {
    let extra = if crate::dbg_flag("OVELO_FREEDBG") {
        16
    } else {
        0
    };
    let p = unsafe { malloc_zone_malloc(guest_zone(), size.max(1) + extra) };
    dbg_alloc(p as usize, size.max(1));
    p
}

#[cfg(target_os = "macos")]
extern "C" fn sh_guest_new_aligned_nothrow(size: usize, align: usize) -> *mut c_void {
    let extra = if crate::dbg_flag("OVELO_FREEDBG") {
        16
    } else {
        0
    };
    let p = unsafe { malloc_zone_memalign(guest_zone(), align.max(16), size.max(1) + extra) };
    dbg_alloc(p as usize, size.max(1));
    p
}

#[cfg(target_os = "macos")]
extern "C" fn sh_guest_delete(ptr: *mut c_void) {
    sh_free(ptr)
}

extern "C" fn sh_atoi(s: *const i8) -> i32 {
    unsafe {
        let mut n = 0i32;
        let mut p = s;
        while (*p as u8).is_ascii_digit() {
            n = n * 10 + (*p as u8 - b'0') as i32;
            p = p.add(1);
        }
        n
    }
}

/// Bionic data symbols (STT_OBJECT) — storage is guest-zone memory so
/// the guest reads real bytes. `_ctype_` is indexed by signed char
/// (-128..=255): the export points 128 entries into a 384-byte table.
fn data_shim(name: &str) -> Option<usize> {
    match name {
        "_ctype_" => Some(ctype_table()),
        // libc++abi handler variables: a cell the guest reads/writes;
        // defaulted to a terminating stub like Bionic's defaults.
        "__cxa_terminate_handler" | "__cxa_unexpected_handler" | "__cxa_new_handler" => {
            Some(cxa_handler_cell(name))
        }
        // Bionic `__sF` — array of 3 FILE* cells (stdin/out/err).
        #[cfg(target_os = "macos")]
        "__sF" => Some(sf_cells()),
        // OpenSL interface IDs are `const SLInterfaceID` pointer
        // variables (wilhelm/libOpenSLES.so exports).
        #[cfg(target_os = "macos")]
        _ if name.starts_with("SL_IID_") => sl_iid_guid(name).map(|g| sl_iid_cell(name, g)),
        _ => None,
    }
}

#[cfg(target_os = "macos")]
fn cxa_handler_cell(name: &str) -> usize {
    static C: std::sync::LazyLock<Mutex<HashMap<String, usize>>> =
        std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));
    let mut m = C.lock().unwrap();
    if let Some(&a) = m.get(name) {
        return a;
    }
    // One cell per name, leaked into the guest zone.
    let a = unsafe {
        let p = malloc_zone_calloc(guest_zone(), 8, 1) as *mut u64;
        *p = sh_terminate as *const () as usize as u64;
        p as usize
    };
    m.insert(name.to_string(), a);
    a
}

#[cfg(not(target_os = "macos"))]
fn cxa_handler_cell(_name: &str) -> usize {
    0
}

#[cfg(target_os = "macos")]
fn ctype_table() -> usize {
    static T: OnceLock<usize> = OnceLock::new();
    *T.get_or_init(|| unsafe {
        // Bionic bits/ctype.h flags.
        const U: u8 = 0x01;
        const L: u8 = 0x02;
        const N: u8 = 0x04;
        const S: u8 = 0x08;
        const P: u8 = 0x10;
        const C: u8 = 0x20;
        const X: u8 = 0x40;
        const B: u8 = 0x80;
        // `_ctype_` is a `const unsigned char*` variable — the guest
        // dereferences the symbol to a pointer first, then indexes it by
        // signed char (-128..=255), so the stored pointer is offset 128.
        let p = malloc_zone_calloc(guest_zone(), 384 + 8, 1) as *mut u8;
        let table = p.add(8);
        for c in 0..=127u8 {
            let f = match c {
                9 => C | S | B,
                10..=13 => C | S,
                0..=8 | 14..=31 | 127 => C,
                32 => S | B,
                b'0'..=b'9' => N | X,
                b'A'..=b'F' => U | X,
                b'G'..=b'Z' => U,
                b'a'..=b'f' => L | X,
                b'g'..=b'z' => L,
                _ => P, // ASCII punctuation runs
            };
            *table.add(c as usize + 128) = f;
        }
        *(p as *mut u64) = table.add(128) as u64;
        p as usize
    })
}

#[cfg(not(target_os = "macos"))]
fn ctype_table() -> usize {
    0
}

// ---- OpenSL ES: IID data symbols + a minimal silent object model ----
// Godot's libgodot_android.so links libOpenSLES at load time. The
// exported `SL_IID_*` symbols are `const SLInterfaceID` pointer
// variables — the guest reads a cell to get the GUID's address, so
// each export is an 8-byte cell pointing at the 16-byte GUID.
//
// Objects are arrays of vtable pointers: obj[0] is &ObjectItf so
// `(*obj)->Realize` works; GetInterface returns &obj[i] (a pointer to
// the slot holding that interface's vtable). obj[-1] stores the state
// key into SL_OBJS, which owns everything the C layout can't express.

#[cfg(target_os = "macos")]
const SL_NODE: [u8; 6] = [0x00, 0x02, 0xa5, 0xd5, 0xc5, 0x1b];

#[cfg(target_os = "macos")]
fn iid_bytes(lo: u32, mid: u16, hi: u16, cs: u16, node: [u8; 6]) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[0..4].copy_from_slice(&lo.to_le_bytes());
    b[4..6].copy_from_slice(&mid.to_le_bytes());
    b[6..8].copy_from_slice(&hi.to_le_bytes());
    b[8..10].copy_from_slice(&cs.to_le_bytes());
    b[10..16].copy_from_slice(&node);
    b
}

#[cfg(target_os = "macos")]
fn sl_iid_guid(name: &str) -> Option<[u8; 16]> {
    Some(match name {
        "SL_IID_OBJECT" => iid_bytes(0x7921638f, 0xddd4, 0x11db, 0xa0e0, SL_NODE),
        "SL_IID_ENGINE" => iid_bytes(0x8d97c260, 0xddd4, 0x11db, 0x958f, SL_NODE),
        "SL_IID_PLAY" => iid_bytes(0xef0bd9c0, 0xddd7, 0x11db, 0xbf49, SL_NODE),
        "SL_IID_RECORD" => iid_bytes(0xc5657aa0, 0xdddb, 0x11db, 0x82f7, SL_NODE),
        "SL_IID_SEEK" => iid_bytes(0xd43135a0, 0xdddc, 0x11db, 0xb62b, SL_NODE),
        "SL_IID_BUFFERQUEUE" => iid_bytes(0x2bc99cc0, 0xddd4, 0x11db, 0x8d99, SL_NODE),
        "SL_IID_EFFECTSEND" => iid_bytes(0x56e7d200, 0xddd4, 0x11db, 0xaefb, SL_NODE),
        "SL_IID_VOLUME" => iid_bytes(0xda9ee320, 0xddd4, 0x11db, 0x82f2, SL_NODE),
        "SL_IID_ENVIRONMENTALREVERB" => iid_bytes(
            0xc2e5d5f0,
            0x94bd,
            0x4763,
            0x9cac,
            [0x4e, 0x23, 0x4d, 0x06, 0x83, 0x9e],
        ),
        "SL_IID_ANDROIDCONFIGURATION" => iid_bytes(0x89f6a7e0, 0xbeac, 0x11df, 0x8b5c, SL_NODE),
        "SL_IID_ANDROIDSIMPLEBUFFERQUEUE" => iid_bytes(0x198e4940, 0xc5d7, 0x11df, 0xa2a6, SL_NODE),
        _ => return None,
    })
}

#[cfg(target_os = "macos")]
fn sl_iid_cell(name: &str, guid: [u8; 16]) -> usize {
    static C: std::sync::LazyLock<Mutex<HashMap<String, usize>>> =
        std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));
    let mut m = C.lock().unwrap();
    if let Some(&a) = m.get(name) {
        return a;
    }
    let a = unsafe {
        let p = malloc_zone_calloc(guest_zone(), 24, 1) as *mut u64;
        *p = (p as usize + 8) as u64; // cell -> GUID
        core::ptr::copy_nonoverlapping(guid.as_ptr(), (p as usize + 8) as *mut u8, 16);
        p as usize
    };
    m.insert(name.to_string(), a);
    a
}

#[cfg(target_os = "macos")]
const SL_RESULT_SUCCESS: usize = 0;
#[cfg(target_os = "macos")]
const SL_RESULT_FEATURE_UNSUPPORTED: usize = 0x11;
#[cfg(target_os = "macos")]
const SL_OBJECT_REALIZED: u32 = 3;
#[cfg(target_os = "macos")]
const SL_PLAYSTATE_STOPPED: u32 = 1;
#[cfg(target_os = "macos")]
const SL_PLAYSTATE_PLAYING: u32 = 3;

#[cfg(target_os = "macos")]
struct SlObj {
    realized: bool,
    play_state: u32,
    ifaces: Vec<([u8; 16], usize)>, // (guid, &obj[slot])
    bq_itf: usize,                  // the buffer-queue interface pointer
    bq_cb: usize,
    bq_ctx: usize,
    bq_count: u32,
    bq_index: u32,
    destroyed: bool,
}

#[cfg(target_os = "macos")]
static SL_OBJS: std::sync::LazyLock<Mutex<HashMap<usize, SlObj>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

#[cfg(target_os = "macos")]
fn sl_vtable(entries: &[usize]) -> usize {
    unsafe {
        let p = malloc_zone_calloc(guest_zone(), entries.len(), 8) as *mut usize;
        // Guests call vtable slots directly (no PLT), so each entry
        // needs the TPIDR swap thunk itself or host code runs under
        // the guest TCB.
        for (i, &e) in entries.iter().enumerate() {
            *p.add(i) = host_thunk(e);
        }
        p as usize
    }
}

#[cfg(target_os = "macos")]
fn sl_obj_vt() -> usize {
    static V: OnceLock<usize> = OnceLock::new();
    *V.get_or_init(|| {
        sl_vtable(&[
            sh_sl_obj_realize as *const () as usize,
            sh_sl_obj_realize as *const () as usize, // Resume
            sh_sl_obj_get_state as *const () as usize,
            sh_sl_obj_get_iface as *const () as usize,
            sh_sl_ok2 as *const () as usize,  // RegisterCallback
            sh_sl_void as *const () as usize, // AbortAsyncOperation
            sh_sl_obj_destroy as *const () as usize,
            sh_sl_ok2 as *const () as usize, // SetPriority
            sh_sl_ok2 as *const () as usize, // GetPriority
            sh_sl_ok2 as *const () as usize, // SetLossOfControlInterfaces
        ])
    })
}

#[cfg(target_os = "macos")]
fn sl_engine_vt() -> usize {
    static V: OnceLock<usize> = OnceLock::new();
    *V.get_or_init(|| {
        sl_vtable(&[
            sh_sl_unsupported as *const () as usize, // CreateLEDDevice
            sh_sl_unsupported as *const () as usize, // CreateVibraDevice
            sh_sl_create_player as *const () as usize,
            sh_sl_create_recorder as *const () as usize,
            sh_sl_unsupported as *const () as usize, // CreateMidiPlayer
            sh_sl_unsupported as *const () as usize, // CreateListener
            sh_sl_unsupported as *const () as usize, // Create3DGroup
            sh_sl_create_mix as *const () as usize,
            sh_sl_unsupported as *const () as usize, // CreateMetadataExtractor
            sh_sl_unsupported as *const () as usize, // CreateExtensionObject
            sh_sl_unsupported as *const () as usize, // QueryNumSupportedInterfaces
            sh_sl_unsupported as *const () as usize, // QuerySupportedInterfaces
            sh_sl_unsupported as *const () as usize, // QueryNumSupportedExtensions
            sh_sl_unsupported as *const () as usize, // QuerySupportedExtension
        ])
    })
}

#[cfg(target_os = "macos")]
fn sl_play_vt() -> usize {
    static V: OnceLock<usize> = OnceLock::new();
    *V.get_or_init(|| {
        sl_vtable(&[
            sh_sl_set_play_state as *const () as usize,
            sh_sl_get_play_state as *const () as usize,
            sh_sl_get_duration as *const () as usize,
            sh_sl_get_position as *const () as usize,
            sh_sl_ok2 as *const () as usize, // RegisterCallback
            sh_sl_ok2 as *const () as usize, // SetCallbackEventsMask
            sh_sl_ok2 as *const () as usize, // GetCallbackEventsMask
            sh_sl_ok2 as *const () as usize, // SetMarkerPosition
            sh_sl_ok2 as *const () as usize, // ClearMarkerPosition
            sh_sl_ok2 as *const () as usize, // GetMarkerPosition
            sh_sl_ok2 as *const () as usize, // SetPositionUpdatePeriod
            sh_sl_ok2 as *const () as usize, // GetPositionUpdatePeriod
        ])
    })
}

#[cfg(target_os = "macos")]
fn sl_bq_vt() -> usize {
    static V: OnceLock<usize> = OnceLock::new();
    *V.get_or_init(|| {
        sl_vtable(&[
            sh_sl_bq_enqueue as *const () as usize,
            sh_sl_bq_clear as *const () as usize,
            sh_sl_bq_get_state as *const () as usize,
            sh_sl_bq_register_cb as *const () as usize,
        ])
    })
}

#[cfg(target_os = "macos")]
fn sl_volume_vt() -> usize {
    static V: OnceLock<usize> = OnceLock::new();
    *V.get_or_init(|| {
        sl_vtable(&[
            sh_sl_ok2 as *const () as usize,      // SetVolumeLevel
            sh_sl_zero_out as *const () as usize, // GetVolumeLevel
            sh_sl_zero_out as *const () as usize, // GetMaxVolumeLevel
            sh_sl_ok2 as *const () as usize,      // SetMute
            sh_sl_zero_out as *const () as usize, // GetMute
            sh_sl_ok2 as *const () as usize,      // EnableStereoPosition
            sh_sl_zero_out as *const () as usize, // IsEnabledStereoPosition
            sh_sl_ok2 as *const () as usize,      // SetStereoPosition
            sh_sl_zero_out as *const () as usize, // GetStereoPosition
        ])
    })
}

#[cfg(target_os = "macos")]
fn sl_aconfig_vt() -> usize {
    static V: OnceLock<usize> = OnceLock::new();
    *V.get_or_init(|| {
        sl_vtable(&[
            sh_sl_ok2 as *const () as usize,         // SetConfiguration
            sh_sl_unsupported as *const () as usize, // GetConfiguration
            sh_sl_unsupported as *const () as usize, // AcquireJavaProxy
            sh_sl_unsupported as *const () as usize, // ReleaseJavaProxy
            sh_sl_unsupported as *const () as usize, // AcquireJavaVm
            sh_sl_unsupported as *const () as usize, // ReleaseJavaVm
            sh_sl_unsupported as *const () as usize, // AcquireBuildInfo
            sh_sl_unsupported as *const () as usize, // ReleaseBuildInfo
        ])
    })
}

#[cfg(target_os = "macos")]
fn sl_effectsend_vt() -> usize {
    static V: OnceLock<usize> = OnceLock::new();
    *V.get_or_init(|| {
        sl_vtable(&[
            sh_sl_ok2 as *const () as usize,      // EnableEffectSend
            sh_sl_zero_out as *const () as usize, // IsEnabledEffectSend
            sh_sl_ok2 as *const () as usize,      // SetDirectLevel
            sh_sl_zero_out as *const () as usize, // GetDirectLevel
        ])
    })
}

/// Allocate a guest object: [state_key, &OBJ_VT, iface vtable ptrs...];
/// returns &base[1] so (*obj) is the ObjectItf vtable.
#[cfg(target_os = "macos")]
fn sl_alloc_obj(iface_vts: &[(&str, usize)]) -> usize {
    let n = iface_vts.len();
    let base = unsafe { malloc_zone_calloc(guest_zone(), n + 2, 8) } as *mut usize;
    let obj = unsafe { base.add(1) };
    unsafe {
        *base = obj as usize; // state key, mirrored by sl_obj()
        *obj = sl_obj_vt();
        for (i, &(_, vt)) in iface_vts.iter().enumerate() {
            *obj.add(1 + i) = vt;
        }
    }
    let mut ifaces = Vec::with_capacity(n);
    for (i, &(name, _)) in iface_vts.iter().enumerate() {
        if let Some(g) = sl_iid_guid(name) {
            ifaces.push((g, unsafe { obj.add(1 + i) } as usize));
        }
    }
    SL_OBJS.lock().unwrap().insert(
        obj as usize,
        SlObj {
            realized: false,
            play_state: SL_PLAYSTATE_STOPPED,
            ifaces,
            bq_itf: 0,
            bq_cb: 0,
            bq_ctx: 0,
            bq_count: 0,
            bq_index: 0,
            destroyed: false,
        },
    );
    obj as usize
}

/// Map an interface pointer (`&obj[i]`, or the object itself for
/// ObjectItf calls) back to its SlObj entry — the state key sits at
/// obj[-1] where `obj` is base+1.
#[cfg(target_os = "macos")]
fn sl_obj_key(iface_self: usize) -> usize {
    // iface_self points into the slot array; the object base is
    // somewhere below it. The slot at base+0 holds the object
    // address itself, so walk back until *(p-1) == base.
    unsafe {
        let mut p = iface_self as *const usize;
        for _ in 0..8 {
            if *p.sub(1) == p as usize {
                return p as usize;
            }
            p = p.sub(1);
        }
        0
    }
}

#[cfg(target_os = "macos")]
extern "C" fn sh_sl_ok2(_a: usize, _b: usize, _c: usize, _d: usize) -> usize {
    SL_RESULT_SUCCESS
}

#[cfg(target_os = "macos")]
extern "C" fn sh_sl_unsupported() -> usize {
    SL_RESULT_FEATURE_UNSUPPORTED
}

#[cfg(target_os = "macos")]
extern "C" fn sh_sl_void() {}

#[cfg(target_os = "macos")]
extern "C" fn sh_sl_zero_out(_s: usize, out: usize) -> usize {
    unsafe { *(out as *mut u64) = 0 };
    SL_RESULT_SUCCESS
}

#[cfg(target_os = "macos")]
extern "C" fn sh_sl_obj_realize(self_: usize, _async: usize) -> usize {
    let k = sl_obj_key(self_);
    if let Some(o) = SL_OBJS.lock().unwrap().get_mut(&k) {
        o.realized = true;
        return SL_RESULT_SUCCESS;
    }
    SL_RESULT_FEATURE_UNSUPPORTED
}

#[cfg(target_os = "macos")]
extern "C" fn sh_sl_obj_get_state(self_: usize, out: usize) -> usize {
    let k = sl_obj_key(self_);
    let st = SL_OBJS
        .lock()
        .unwrap()
        .get(&k)
        .map(|o| if o.realized { SL_OBJECT_REALIZED } else { 1 })
        .unwrap_or(1);
    unsafe { *(out as *mut u32) = st };
    SL_RESULT_SUCCESS
}

#[cfg(target_os = "macos")]
extern "C" fn sh_sl_obj_get_iface(self_: usize, iid: usize, out: usize) -> usize {
    let k = sl_obj_key(self_);
    let guid = unsafe { *(iid as *const [u8; 16]) };
    let m = SL_OBJS.lock().unwrap();
    if let Some(o) = m.get(&k) {
        // SL_IID_OBJECT asks for the object itself.
        if guid == sl_iid_guid("SL_IID_OBJECT").unwrap() {
            unsafe { *(out as *mut usize) = k };
            return SL_RESULT_SUCCESS;
        }
        for &(g, slot) in &o.ifaces {
            if g == guid {
                unsafe { *(out as *mut usize) = slot };
                return SL_RESULT_SUCCESS;
            }
        }
    }
    SL_RESULT_FEATURE_UNSUPPORTED
}

#[cfg(target_os = "macos")]
extern "C" fn sh_sl_obj_destroy(self_: usize) {
    let k = sl_obj_key(self_);
    if let Some(o) = SL_OBJS.lock().unwrap().get_mut(&k) {
        o.destroyed = true;
    }
}

#[cfg(target_os = "macos")]
extern "C" fn sh_sl_create_engine(
    out: usize,
    _n_opts: usize,
    _opts: usize,
    _n_if: usize,
    _iids: usize,
    _req: usize,
) -> usize {
    let e = sl_alloc_obj(&[("SL_IID_ENGINE", sl_engine_vt())]);
    unsafe { *(out as *mut usize) = e };
    SL_RESULT_SUCCESS
}

#[cfg(target_os = "macos")]
extern "C" fn sh_sl_create_mix(
    _eng: usize,
    out: usize,
    _n_if: usize,
    _iids: usize,
    _req: usize,
) -> usize {
    let m = sl_alloc_obj(&[]);
    unsafe { *(out as *mut usize) = m };
    SL_RESULT_SUCCESS
}

#[cfg(target_os = "macos")]
extern "C" fn sh_sl_create_player(
    _eng: usize,
    out: usize,
    _src: usize,
    _snk: usize,
    _n_if: usize,
    _iids: usize,
    _req: usize,
) -> usize {
    let bq_slot_vt = sl_bq_vt();
    let p = sl_alloc_obj(&[
        ("SL_IID_PLAY", sl_play_vt()),
        ("SL_IID_ANDROIDSIMPLEBUFFERQUEUE", bq_slot_vt),
        // Godot asks for the standard (non-Android) BQ interface id.
        ("SL_IID_BUFFERQUEUE", bq_slot_vt),
        ("SL_IID_VOLUME", sl_volume_vt()),
        ("SL_IID_ANDROIDCONFIGURATION", sl_aconfig_vt()),
        ("SL_IID_EFFECTSEND", sl_effectsend_vt()),
    ]);
    if let Some(o) = SL_OBJS.lock().unwrap().get_mut(&p) {
        o.bq_itf = o
            .ifaces
            .iter()
            .find(|(g, _)| *g == sl_iid_guid("SL_IID_ANDROIDSIMPLEBUFFERQUEUE").unwrap())
            .map(|(_, s)| *s)
            .unwrap_or(0);
    }
    unsafe { *(out as *mut usize) = p };
    SL_RESULT_SUCCESS
}

#[cfg(target_os = "macos")]
extern "C" fn sh_sl_create_recorder(
    _eng: usize,
    out: usize,
    _src: usize,
    _snk: usize,
    _n_if: usize,
    _iids: usize,
    _req: usize,
) -> usize {
    let r = sl_alloc_obj(&[("SL_IID_RECORD", sl_bq_vt())]);
    unsafe { *(out as *mut usize) = r };
    SL_RESULT_SUCCESS
}

#[cfg(target_os = "macos")]
extern "C" fn sh_sl_set_play_state(self_: usize, state: u32) -> usize {
    let k = sl_obj_key(self_);
    if let Some(o) = SL_OBJS.lock().unwrap().get_mut(&k) {
        o.play_state = state;
    }
    SL_RESULT_SUCCESS
}

#[cfg(target_os = "macos")]
extern "C" fn sh_sl_get_play_state(self_: usize, out: usize) -> usize {
    let k = sl_obj_key(self_);
    let s = SL_OBJS
        .lock()
        .unwrap()
        .get(&k)
        .map(|o| o.play_state)
        .unwrap_or(SL_PLAYSTATE_STOPPED);
    unsafe { *(out as *mut u32) = s };
    SL_RESULT_SUCCESS
}

#[cfg(target_os = "macos")]
extern "C" fn sh_sl_get_duration(_s: usize, out: usize) -> usize {
    unsafe { *(out as *mut u32) = 0xFFFF_FFFF }; // SL_TIME_UNKNOWN
    SL_RESULT_SUCCESS
}

#[cfg(target_os = "macos")]
extern "C" fn sh_sl_get_position(self_: usize, out: usize) -> usize {
    let k = sl_obj_key(self_);
    let pos = SL_OBJS
        .lock()
        .unwrap()
        .get(&k)
        .map(|o| o.bq_index as u64 * 8)
        .unwrap_or(0);
    unsafe { *(out as *mut u64) = pos };
    SL_RESULT_SUCCESS
}

#[cfg(target_os = "macos")]
extern "C" fn sh_sl_bq_enqueue(self_: usize, _buf: usize, _size: u32) -> usize {
    let k = sl_obj_key(self_);
    if let Some(o) = SL_OBJS.lock().unwrap().get_mut(&k) {
        o.bq_count = o.bq_count.saturating_add(1);
    }
    SL_RESULT_SUCCESS
}

#[cfg(target_os = "macos")]
extern "C" fn sh_sl_bq_clear(self_: usize) -> usize {
    let k = sl_obj_key(self_);
    if let Some(o) = SL_OBJS.lock().unwrap().get_mut(&k) {
        o.bq_count = 0;
    }
    SL_RESULT_SUCCESS
}

#[cfg(target_os = "macos")]
extern "C" fn sh_sl_bq_get_state(self_: usize, out: usize) -> usize {
    let k = sl_obj_key(self_);
    let (c, i) = SL_OBJS
        .lock()
        .unwrap()
        .get(&k)
        .map(|o| (o.bq_count, o.bq_index))
        .unwrap_or((0, 0));
    unsafe {
        *(out as *mut u32) = c;
        *(out as *mut u32).add(1) = i;
    }
    SL_RESULT_SUCCESS
}

#[cfg(target_os = "macos")]
extern "C" fn sh_sl_bq_register_cb(self_: usize, cb: usize, ctx: usize) -> usize {
    let k = sl_obj_key(self_);
    if let Some(o) = SL_OBJS.lock().unwrap().get_mut(&k) {
        o.bq_cb = cb;
        o.bq_ctx = ctx;
    }
    sl_start_pump();
    SL_RESULT_SUCCESS
}

/// Silent sink: while a player is PLAYING, consume one queued buffer
/// per tick and fire the registered callback, which is where Godot
/// mixes and re-enqueues audio.
#[cfg(target_os = "macos")]
fn sl_start_pump() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        std::thread::spawn(|| loop {
            std::thread::sleep(std::time::Duration::from_millis(10));
            let calls: Vec<(usize, usize, usize)> = {
                let mut m = SL_OBJS.lock().unwrap();
                m.values_mut()
                    .filter(|o| {
                        !o.destroyed
                            && o.play_state == SL_PLAYSTATE_PLAYING
                            && o.bq_cb != 0
                            && o.bq_count > 0
                    })
                    .map(|o| {
                        o.bq_count -= 1;
                        o.bq_index = o.bq_index.wrapping_add(1);
                        (o.bq_cb, o.bq_itf, o.bq_ctx)
                    })
                    .collect()
            };
            for (cb, itf, ctx) in calls {
                let f: extern "C" fn(usize, usize) = unsafe { std::mem::transmute(cb) };
                with_guest_tls_at("sl_pump", || f(itf, ctx));
            }
        });
    });
}

// ---- NDK miscellany: AAsset, ANativeWindow, cxa-abi helpers ----

/// Raw pointer to the running app's ApkResources — set when a native
/// library loads for it. The Vm owns it and outlives loaded code.
static APK_RES_PTR: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

pub(crate) fn set_apk_reader(p: usize) {
    APK_RES_PTR.store(p, std::sync::atomic::Ordering::Release);
}

fn apk_read(path: &str) -> Option<Vec<u8>> {
    let p =
        APK_RES_PTR.load(std::sync::atomic::Ordering::Acquire) as *const crate::res::ApkResources;
    if p.is_null() {
        return None;
    }
    unsafe { (*p).read(path) }
}

#[cfg(target_os = "macos")]
type AssetMap = Mutex<HashMap<usize, (Vec<u8>, usize)>>;
static AASSETS: std::sync::LazyLock<AssetMap> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

#[cfg(target_os = "macos")]
extern "C" fn sh_asset_mgr_from_java(_env: usize, _mgr: usize) -> usize {
    1
}

#[cfg(target_os = "macos")]
extern "C" fn sh_asset_open(_mgr: usize, name: usize, _mode: i32) -> usize {
    let n = crate::jni::cstr_ro(name as u64);
    let data = apk_read(&n)
        .or_else(|| apk_read(&format!("assets/{n}")))
        .or_else(|| apk_read(&format!("assets/{}", n.trim_start_matches('/'))));
    let Some(data) = data else { return 0 };
    let mut m = AASSETS.lock().unwrap();
    let token = m.keys().max().unwrap_or(&0) + 1;
    m.insert(token, (data, 0));
    token
}

#[cfg(target_os = "macos")]
extern "C" fn sh_asset_read(asset: usize, buf: usize, count: usize) -> isize {
    let mut m = AASSETS.lock().unwrap();
    let Some((data, off)) = m.get_mut(&asset) else {
        return -1;
    };
    let n = count.min(data.len().saturating_sub(*off));
    unsafe { std::ptr::copy_nonoverlapping(data.as_ptr().add(*off), buf as *mut u8, n) };
    *off += n;
    n as isize
}

#[cfg(target_os = "macos")]
extern "C" fn sh_asset_len(asset: usize) -> i64 {
    AASSETS
        .lock()
        .unwrap()
        .get(&asset)
        .map(|(d, _)| d.len() as i64)
        .unwrap_or(-1)
}

#[cfg(target_os = "macos")]
extern "C" fn sh_asset_seek(asset: usize, offset: i64, whence: i32) -> i64 {
    let mut m = AASSETS.lock().unwrap();
    let Some((data, off)) = m.get_mut(&asset) else {
        return -1;
    };
    let new = match whence {
        0 => offset,                     // SEEK_SET
        1 => *off as i64 + offset,       // SEEK_CUR
        2 => data.len() as i64 + offset, // SEEK_END
        _ => -1,
    };
    if new < 0 || new > data.len() as i64 {
        return -1;
    }
    *off = new as usize;
    new
}

#[cfg(target_os = "macos")]
extern "C" fn sh_asset_close(asset: usize) {
    AASSETS.lock().unwrap().remove(&asset);
}

#[cfg(target_os = "macos")]
extern "C" fn sh_native_window_from_surface(_env: usize, _surf: usize) -> usize {
    // A token — guests only test it for null before handing it to EGL.
    1
}

#[cfg(target_os = "macos")]
extern "C" fn sh_egl_current(_d: usize) -> usize {
    0
}

#[cfg(target_os = "macos")]
extern "C" fn sh_egl_get_proc(name: usize) -> usize {
    let n = crate::jni::cstr_ro(name as u64);
    builtin_shim(&n).or_else(|| host_dlsym(&n)).unwrap_or(0)
}

#[cfg(target_os = "macos")]
extern "C" fn sh_void_n(_a: usize, _b: usize, _c: usize, _d: usize) {}

#[cfg(target_os = "macos")]
extern "C" fn sh_cxa_guard_acquire(g: usize) -> usize {
    (unsafe { *(g as *const u8) } == 0) as usize
}

#[cfg(target_os = "macos")]
extern "C" fn sh_cxa_guard_release(g: usize) {
    unsafe { *(g as *mut u8) = 1 };
}

#[cfg(target_os = "macos")]
extern "C" fn sh_cxa_thread_atexit(_f: usize, _a: usize, _d: usize) -> usize {
    0
}

/// `__dynamic_cast(src, src_type, dst_type, src2dst_offset)` — return
/// the source pointer, right for the identity/upcast cases Godot uses.
#[cfg(target_os = "macos")]
extern "C" fn sh_dynamic_cast(src: usize, _a: usize, _b: usize, _c: usize) -> usize {
    src
}

/// Bionic `__sF` — guests declare `extern FILE* __sF[]` and index it
/// for stdin/stdout/stderr. Three cells holding the real Darwin FILEs.
#[cfg(target_os = "macos")]
fn sf_cells() -> usize {
    static A: OnceLock<usize> = OnceLock::new();
    *A.get_or_init(|| unsafe {
        let p = malloc_zone_calloc(guest_zone(), 3, 8) as *mut usize;
        for (i, n) in ["__stdinp", "__stdoutp", "__stderrp"].iter().enumerate() {
            if let Some(a) = host_dlsym(n) {
                *p.add(i) = *(a as *const usize);
            }
        }
        p as usize
    })
}

/// `__FD_SET_chk(fd, set, set_size)` — FORTIFY-bound-checked FD_SET.
#[cfg(target_os = "macos")]
extern "C" fn sh_fd_set_chk(fd: i32, set: usize, _set_size: usize) {
    if !(0..1024).contains(&fd) {
        sh_chk_fail();
    }
    unsafe { *(set as *mut u64).add(fd as usize / 64) |= 1u64 << (fd as usize % 64) };
}

#[cfg(target_os = "macos")]
extern "C" fn sh_setname(_t: usize, _name: usize) -> usize {
    0
}

// ----------------------------------------------------------------
// Bionic pthread synchronization: Bionic's pthread_mutex_t is a
// 40-byte inline object and Darwin's pthread_mutex_* rejects the
// foreign layout (EINVAL -> libc++ "failed to acquire mutex" aborts).
// Mutex / condvar / rwlock state lives here keyed by guest address;
// a single global lock + condvar drives every wait (correct if
// thundering-herd-y).
// ----------------------------------------------------------------
#[cfg(target_os = "macos")]
mod bsync {
    use std::collections::HashMap;
    use std::sync::{Condvar, LazyLock, Mutex};

    pub enum Obj {
        Mutex {
            ty: u32,
            owner: u64,
            count: u32,
        },
        Cond,
        Rw {
            readers: u32,
            wr_owner: u64,
            wr_count: u32,
            wait_w: u32,
        },
    }

    pub static SYNC: LazyLock<(Mutex<HashMap<usize, Obj>>, Condvar)> =
        LazyLock::new(|| (Mutex::new(HashMap::new()), Condvar::new()));

    pub const EPERM: i32 = 1;
    pub const EBUSY: i32 = 16;
    pub const EINVAL: i32 = 22;
    pub const EDEADLK: i32 = 35;
    pub const ETIMEDOUT: i32 = 110;

    pub fn tid() -> u64 {
        unsafe { super::mach_thread_id() }
    }
}
#[cfg(target_os = "macos")]
use bsync::Obj as Bobj;

#[cfg(target_os = "macos")]
extern "C" fn sh_mutexattr_init(a: usize) -> usize {
    unsafe { *(a as *mut u32) = 0 };
    0
}
#[cfg(target_os = "macos")]
extern "C" fn sh_mutexattr_settype(a: usize, ty: u32) -> usize {
    unsafe { *(a as *mut u32) = ty };
    0
}
#[cfg(target_os = "macos")]
extern "C" fn sh_mutexattr_gettype(a: usize, out: usize) -> usize {
    unsafe { *(out as *mut u32) = *(a as *const u32) };
    0
}
#[cfg(target_os = "macos")]
extern "C" fn sh_attr_noop() -> usize {
    0
}

/// `pthread_mutex_init(m, attr)` — attr is Bionic's 4-byte attr
/// object (`{int type}`); NULL means default.
#[cfg(target_os = "macos")]
extern "C" fn sh_mutex_init(m: usize, attr: usize) -> usize {
    unsafe { std::ptr::write_bytes(m as *mut u8, 0, 40) };
    let ty = if attr == 0 {
        0
    } else {
        unsafe { *(attr as *const u32) & 3 }
    };
    bsync::SYNC.0.lock().unwrap().insert(
        m,
        Bobj::Mutex {
            ty,
            owner: 0,
            count: 0,
        },
    );
    0
}

#[cfg(target_os = "macos")]
extern "C" fn sh_mutex_destroy(m: usize) -> usize {
    bsync::SYNC.0.lock().unwrap().remove(&m);
    0
}

#[cfg(target_os = "macos")]
extern "C" fn sh_mutex_lock(m: usize) -> usize {
    let (map, cv) = &*bsync::SYNC;
    let mut g = map.lock().unwrap();
    let t = bsync::tid();
    loop {
        match g.get_mut(&m) {
            Some(Bobj::Mutex { ty, owner, count }) => {
                if *owner == 0 {
                    *owner = t;
                    *count = 1;
                    return 0;
                }
                if *owner == t {
                    // Bionic: NORMAL=0 RECURSIVE=1 ERRORCHECK=2.
                    if *ty == 1 {
                        *count += 1;
                        return 0;
                    }
                    if *ty == 2 {
                        return bsync::EDEADLK as usize;
                    }
                    // NORMAL self-lock: wait forever (real deadlock).
                }
                g = cv.wait(g).unwrap();
            }
            // An object that was never inited reads its type from the
            // guest word: Bionic static initializers encode it in bits
            // 14-15 (NORMAL=0, RECURSIVE=0x4000, ERRORCHECK=0x8000) — a
            // static recursive mutex must not register as NORMAL or the
            // second lock by the same thread deadlocks.
            None => {
                let ty = unsafe { (*(m as *const u32) >> 14) & 3 };
                g.insert(
                    m,
                    Bobj::Mutex {
                        ty,
                        owner: t,
                        count: 1,
                    },
                );
                return 0;
            }
            Some(_) => return bsync::EINVAL as usize,
        }
    }
}

#[cfg(target_os = "macos")]
extern "C" fn sh_mutex_trylock(m: usize) -> usize {
    let (map, _) = &*bsync::SYNC;
    let mut g = map.lock().unwrap();
    let t = bsync::tid();
    match g.get_mut(&m) {
        Some(Bobj::Mutex { ty, owner, count }) => {
            if *owner == 0 {
                *owner = t;
                *count = 1;
                0
            } else if *owner == t && *ty == 1 {
                *count += 1;
                0
            } else if *owner == t {
                // POSIX/Bionic: trylock on an already-owned mutex
                // reports EBUSY even for ERRORCHECK — EDEADLK is a
                // blocking-lock result only.
                bsync::EBUSY as usize
            } else {
                bsync::EBUSY as usize
            }
        }
        None => {
            let ty = unsafe { (*(m as *const u32) >> 14) & 3 };
            g.insert(
                m,
                Bobj::Mutex {
                    ty,
                    owner: t,
                    count: 1,
                },
            );
            0
        }
        Some(_) => bsync::EINVAL as usize,
    }
}

#[cfg(target_os = "macos")]
extern "C" fn sh_mutex_unlock(m: usize) -> usize {
    let (map, cv) = &*bsync::SYNC;
    let mut g = map.lock().unwrap();
    let t = bsync::tid();
    match g.get_mut(&m) {
        Some(Bobj::Mutex { ty, owner, count }) => {
            if *owner != t {
                return bsync::EPERM as usize;
            }
            let _ = ty;
            *count -= 1;
            if *count == 0 {
                *owner = 0;
                cv.notify_all();
            }
            0
        }
        _ => bsync::EINVAL as usize,
    }
}

#[cfg(target_os = "macos")]
extern "C" fn sh_cond_init(c: usize, _attr: usize) -> usize {
    unsafe { std::ptr::write_bytes(c as *mut u8, 0, 40) };
    bsync::SYNC.0.lock().unwrap().insert(c, Bobj::Cond);
    0
}

#[cfg(target_os = "macos")]
extern "C" fn sh_cond_destroy(c: usize) -> usize {
    bsync::SYNC.0.lock().unwrap().remove(&c);
    0
}

/// Shared cond-wait core: drop the mutex (preserving a recursive
/// count), wait on the global cv, then reacquire. `deadline` is an
/// absolute mach time in nanoseconds; None = untimed.
#[cfg(target_os = "macos")]
fn bcond_wait(c: usize, m: usize, deadline_ns: Option<u64>) -> i32 {
    let (map, cv) = &*bsync::SYNC;
    let t = bsync::tid();
    let mut g = map.lock().unwrap();
    // Release the mutex, saving recursive depth.
    let mut saved = 0u32;
    match g.get_mut(&m) {
        Some(Bobj::Mutex { owner, count, .. }) => {
            if *owner != t {
                return bsync::EPERM;
            }
            saved = *count;
            *owner = 0;
            *count = 0;
        }
        Some(_) => return bsync::EINVAL,
        None => {}
    }
    cv.notify_all();
    if !matches!(g.get(&c), Some(Bobj::Cond)) {
        g.insert(c, Bobj::Cond);
    }
    let mut timed_out = false;
    if let Some(dl) = deadline_ns {
        let dur = dl.saturating_sub(unsafe { mach_absolute_time_ns() });
        let (_g2, to) = cv
            .wait_timeout(g, std::time::Duration::from_nanos(dur))
            .unwrap();
        g = _g2;
        timed_out = to.timed_out();
    } else {
        g = cv.wait(g).unwrap();
    }
    // Reacquire the mutex (blocking).
    loop {
        match g.get_mut(&m) {
            Some(Bobj::Mutex { owner, count, .. }) => {
                if *owner == 0 {
                    *owner = t;
                    *count = if saved == 0 { 1 } else { saved };
                    break;
                }
                g = cv.wait(g).unwrap();
            }
            Some(_) => return bsync::EINVAL,
            None => {
                g.insert(
                    m,
                    Bobj::Mutex {
                        ty: 0,
                        owner: t,
                        count: saved.max(1),
                    },
                );
                break;
            }
        }
    }
    if timed_out {
        bsync::ETIMEDOUT
    } else {
        0
    }
}

/// Current monotonic time in nanoseconds for pthread timeouts
/// (CLOCK_REALTIME abstime comes in as epoch ns — guests pass
/// timespecs in either clock; treat as monotonic-ish by comparing
/// against system uptime).
#[cfg(target_os = "macos")]
unsafe fn mach_absolute_time_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

#[cfg(target_os = "macos")]
extern "C" fn sh_cond_wait(c: usize, m: usize) -> usize {
    bcond_wait(c, m, None) as usize
}

#[cfg(target_os = "macos")]
extern "C" fn sh_cond_timedwait(c: usize, m: usize, ts: usize) -> usize {
    let dl = if ts == 0 {
        None
    } else {
        let (sec, nsec) = unsafe { (*(ts as *const i64), *(ts as *const i64).add(1)) };
        Some(
            (sec as u64)
                .saturating_mul(1_000_000_000)
                .saturating_add(nsec as u64),
        )
    };
    bcond_wait(c, m, dl) as usize
}

#[cfg(target_os = "macos")]
extern "C" fn sh_cond_signal(c: usize) -> usize {
    let (map, cv) = &*bsync::SYNC;
    let _g = map.lock().unwrap();
    cv.notify_one();
    let _ = c;
    0
}

#[cfg(target_os = "macos")]
extern "C" fn sh_cond_broadcast(c: usize) -> usize {
    let (map, cv) = &*bsync::SYNC;
    let _g = map.lock().unwrap();
    cv.notify_all();
    let _ = c;
    0
}

#[cfg(target_os = "macos")]
extern "C" fn sh_rwlock_init(r: usize, _attr: usize) -> usize {
    // pthread_rwlock_t is 56 bytes on arm64 Bionic — zeroing more
    // scribbles over whatever follows the lock in guest memory.
    unsafe { std::ptr::write_bytes(r as *mut u8, 0, 56) };
    bsync::SYNC.0.lock().unwrap().insert(
        r,
        Bobj::Rw {
            readers: 0,
            wr_owner: 0,
            wr_count: 0,
            wait_w: 0,
        },
    );
    0
}

#[cfg(target_os = "macos")]
extern "C" fn sh_rwlock_destroy(r: usize) -> usize {
    bsync::SYNC.0.lock().unwrap().remove(&r);
    0
}

#[cfg(target_os = "macos")]
extern "C" fn sh_rwlock_rdlock(r: usize) -> usize {
    let (map, cv) = &*bsync::SYNC;
    let mut g = map.lock().unwrap();
    loop {
        match g.get_mut(&r) {
            Some(Bobj::Rw {
                readers,
                wr_owner,
                wait_w,
                ..
            }) => {
                if *wr_owner == 0 && *wait_w == 0 {
                    *readers += 1;
                    return 0;
                }
                g = cv.wait(g).unwrap();
            }
            Some(_) => return bsync::EINVAL as usize,
            None => {
                g.insert(
                    r,
                    Bobj::Rw {
                        readers: 1,
                        wr_owner: 0,
                        wr_count: 0,
                        wait_w: 0,
                    },
                );
                return 0;
            }
        }
    }
}

#[cfg(target_os = "macos")]
extern "C" fn sh_rwlock_tryrdlock(r: usize) -> usize {
    let (map, _) = &*bsync::SYNC;
    let mut g = map.lock().unwrap();
    match g.get_mut(&r) {
        Some(Bobj::Rw {
            readers,
            wr_owner,
            wait_w,
            ..
        }) => {
            if *wr_owner == 0 && *wait_w == 0 {
                *readers += 1;
                0
            } else {
                bsync::EBUSY as usize
            }
        }
        Some(_) => bsync::EINVAL as usize,
        None => {
            g.insert(
                r,
                Bobj::Rw {
                    readers: 1,
                    wr_owner: 0,
                    wr_count: 0,
                    wait_w: 0,
                },
            );
            0
        }
    }
}

#[cfg(target_os = "macos")]
extern "C" fn sh_rwlock_wrlock(r: usize) -> usize {
    let (map, cv) = &*bsync::SYNC;
    let mut g = map.lock().unwrap();
    let t = bsync::tid();
    if g.get(&r).is_none() {
        g.insert(
            r,
            Bobj::Rw {
                readers: 0,
                wr_owner: t,
                wr_count: 1,
                wait_w: 0,
            },
        );
        return 0;
    }
    // Recursion: Bionic rwlocks are not recursive; wait like others.
    loop {
        match g.get_mut(&r) {
            Some(Bobj::Rw {
                readers,
                wr_owner,
                wr_count,
                wait_w,
            }) => {
                if *wr_owner == t {
                    *wr_count += 1;
                    return 0;
                }
                if *wr_owner == 0 && *readers == 0 {
                    *wr_owner = t;
                    *wr_count = 1;
                    return 0;
                }
                *wait_w += 1;
                g = cv.wait(g).unwrap();
                if let Some(Bobj::Rw { wait_w, .. }) = g.get_mut(&r) {
                    *wait_w = wait_w.saturating_sub(1);
                }
            }
            Some(_) => return bsync::EINVAL as usize,
            None => return bsync::EINVAL as usize,
        }
    }
}

#[cfg(target_os = "macos")]
extern "C" fn sh_rwlock_trywrlock(r: usize) -> usize {
    let (map, _) = &*bsync::SYNC;
    let mut g = map.lock().unwrap();
    let t = bsync::tid();
    match g.get_mut(&r) {
        Some(Bobj::Rw {
            readers,
            wr_owner,
            wr_count,
            ..
        }) => {
            if *wr_owner == t {
                *wr_count += 1;
                0
            } else if *wr_owner == 0 && *readers == 0 {
                *wr_owner = t;
                *wr_count = 1;
                0
            } else {
                bsync::EBUSY as usize
            }
        }
        Some(_) => bsync::EINVAL as usize,
        None => {
            g.insert(
                r,
                Bobj::Rw {
                    readers: 0,
                    wr_owner: t,
                    wr_count: 1,
                    wait_w: 0,
                },
            );
            0
        }
    }
}

#[cfg(target_os = "macos")]
extern "C" fn sh_rwlock_unlock(r: usize) -> usize {
    let (map, cv) = &*bsync::SYNC;
    let mut g = map.lock().unwrap();
    let t = bsync::tid();
    match g.get_mut(&r) {
        Some(Bobj::Rw {
            readers,
            wr_owner,
            wr_count,
            ..
        }) => {
            if *wr_owner == t {
                *wr_count -= 1;
                if *wr_count == 0 {
                    *wr_owner = 0;
                    cv.notify_all();
                }
                0
            } else if *wr_owner == 0 && *readers > 0 {
                *readers -= 1;
                if *readers == 0 {
                    cv.notify_all();
                }
                0
            } else {
                bsync::EPERM as usize
            }
        }
        _ => bsync::EINVAL as usize,
    }
}

/// Diagnostic wrapper for `vfprintf(FILE*,fmt,ap)`: log the args and
/// the guest caller (lr -> .so offset), then let the real call run —
/// libSystem varargs can't be forwarded portably, so this only logs.
#[cfg(target_os = "macos")]
extern "C" fn sh_vfprintf(fp: usize, fmt: usize, _ap: usize) -> usize {
    let mut lr = 0usize;
    unsafe { core::arch::asm!("mov {}, x30", out(reg) lr) };
    let off = so_off(lr);
    let f = crate::jni::cstr_ro(fmt as u64);
    let shown = if f.len() > 80 { &f[..80] } else { &f };
    eprintln!("[vf] fp={fp:#x} caller={off:#x?} fmt={shown:?}");
    0
}

#[cfg(target_os = "macos")]
extern "C" fn sh_fprintf(fp: usize, fmt: usize, _a3: usize, _a4: usize) -> usize {
    let mut lr = 0usize;
    unsafe { core::arch::asm!("mov {}, x30", out(reg) lr) };
    let off = so_off(lr);
    let f = crate::jni::cstr_ro(fmt as u64);
    let shown = if f.len() > 80 { &f[..80] } else { &f };
    eprintln!("[fprintf] fp={fp:#x} caller={off:#x?} fmt={shown:?}");
    0
}

/// `exit`/`abort` reach libSystem directly through host_dlsym, killing
/// the process with no trace. Log the guest caller (.so offset via lr)
/// and the status first, then honor the request — Godot's setup path
/// calls exit() when an early check fails, which previously looked
/// like a silent clean exit.
#[cfg(target_os = "macos")]
extern "C" fn sh_exit(code: i32) -> ! {
    let mut lr = 0usize;
    unsafe { core::arch::asm!("mov {}, x30", out(reg) lr) };
    eprintln!("[exit] code={code} caller={:#x?}", so_off(lr));
    unsafe { exit(code) }
}

/// `_exit`/`_Exit` bypass atexit handlers and stdio flushing — they
/// must not become regular `exit`.
#[cfg(target_os = "macos")]
extern "C" fn sh_underscore_exit(code: i32) -> ! {
    let mut lr = 0usize;
    unsafe { core::arch::asm!("mov {}, x30", out(reg) lr) };
    eprintln!("[_exit] code={code} caller={:#x?}", so_off(lr));
    unsafe { _exit(code) }
}

#[cfg(target_os = "macos")]
/// `__cxa_throw`: log the exception's typeinfo name before handing to
/// the real runtime — guest `.eh_frame` isn't registered so the unwind
/// terminates, but the type name identifies what failed.
#[cfg(target_os = "macos")]
extern "C" fn sh_cxa_throw(_thrown: *mut c_void, tinfo: *const usize, _dest: *mut c_void) -> ! {
    let name = if tinfo.is_null() {
        0
    } else {
        unsafe { *tinfo.add(1) }
    } as *const i8;
    let s = if name.is_null() || !src_readable(name as *const c_void, 1) {
        "?".to_string()
    } else {
        unsafe { std::ffi::CStr::from_ptr(name) }
            .to_string_lossy()
            .to_string()
    };
    let mut lr = 0usize;
    unsafe { core::arch::asm!("mov {}, x30", out(reg) lr) };
    eprintln!("[cxa-throw] type={} caller={:#x?}", s, so_off(lr));
    // Guest unwinding can't run (its .eh_frame is unregistered), so a
    // throw always lands in terminate — go straight to abort.
    sh_abort()
}

extern "C" fn sh_abort() -> ! {
    let mut lr = 0usize;
    unsafe { core::arch::asm!("mov {}, x30", out(reg) lr) };
    eprintln!("[abort] caller={:#x?}", so_off(lr));
    unsafe { libc_abort() }
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn exit(code: i32) -> !;
    #[link_name = "abort"]
    fn libc_abort() -> !;
}

/// `kill`/`raise` likewise pass straight through — a guest that fires
/// SIGKILL at itself vanishes with no trace. Log target/signal/caller,
/// then deliver.
/// Guest `sigaction`: Godot installs a crash handler on SEGV/BUS/ILL,
/// which would replace our TLS-healing handler with guest code — a
/// fault then delivers into guest code running under a garbage TPIDR
/// and the nested exception kills the task as unrecoverable. Record
/// the guest handler for diagnostics; keep ours installed. Other
/// signals forward to the real sigaction.
#[cfg(target_os = "macos")]
extern "C" fn sh_sigaction(sig: i32, act: usize, oact: usize) -> i32 {
    let mut lr = 0usize;
    unsafe { core::arch::asm!("mov {}, x30", out(reg) lr) };
    // Bionic layout: {sa_flags:i32, pad, sa_handler:u64, sa_mask:u32,
    // sa_sigaction:u64 (old variants)} — flags at +0, handler at +8.
    // Linux flag values differ from Darwin's, so translate.
    let (bflags, handler) = if act != 0 {
        unsafe { (*(act as *const i32), *(act as *const usize).add(1)) }
    } else {
        (0, 0)
    };
    eprintln!(
        "[sigaction] sig={sig} flags={bflags:#x} handler={handler:#x} caller={:#x?}",
        so_off(lr)
    );
    if oact != 0 {
        unsafe { std::ptr::write_bytes(oact as *mut u8, 0, 32) };
    }
    if matches!(sig, 4 | 6 | 10 | 11) {
        if act != 0 && handler > 1 {
            if let Ok(mut g) = GUEST_SIG_HANDLERS.lock() {
                g.get_or_insert_with(HashMap::new).insert(sig, handler);
            }
        }
        return 0;
    }
    // Linux->Darwin flag map: only the bits Bionic actually uses.
    let mut dflags = 0i32;
    if bflags & 0x0800_0000 != 0 {
        dflags |= 0x0001; // SA_ONSTACK
    }
    if bflags & 0x1000_0000 != 0 {
        dflags |= 0x0002; // SA_RESTART
    }
    if bflags & 0x4 != 0 {
        dflags |= 0x0040; // SA_SIGINFO
    }
    if bflags & 0x8000_0000u32 as i32 != 0 {
        dflags |= 0x0004; // SA_RESETHAND
    }
    if bflags & 0x4000_0000 != 0 {
        dflags |= 0x0010; // SA_NODEFER
    }
    if bflags & 0x1 != 0 {
        dflags |= 0x0008; // SA_NOCLDSTOP
    }
    if bflags & 0x2 != 0 {
        dflags |= 0x0020; // SA_NOCLDWAIT
    }
    let dact = if act != 0 {
        Sigaction {
            sa_sigaction: handler,
            sa_mask: 0,
            sa_flags: dflags,
        }
    } else {
        Sigaction {
            sa_sigaction: 0,
            sa_mask: 0,
            sa_flags: 0,
        }
    };
    unsafe {
        sigaction(
            sig,
            if act != 0 { &dact } else { std::ptr::null() },
            std::ptr::null_mut(),
        )
    }
}

/// Guest-requested signal handlers we declined to install (sig ->
/// sa_handler address), kept for diagnostics/chaining.
#[cfg(target_os = "macos")]
static GUEST_SIG_HANDLERS: std::sync::Mutex<Option<std::collections::HashMap<i32, usize>>> =
    std::sync::Mutex::new(None);

/// Task-level Mach exception port: the kernel consults it before BSD
/// signal delivery, so it observes faults that never reach a signal
/// handler (unrecoverable -> SIGKILL). Each request is logged
/// (type/codes/thread-port/pc) and answered with a failure code so
/// delivery continues down the normal BSD chain — nothing is stolen.
#[cfg(target_os = "macos")]
pub(crate) mod excport {
    use std::ffi::c_void;

    extern "C" {
        fn mach_port_allocate(task: u32, right: i32, port: *mut u32) -> i32;
        fn mach_port_insert_right(task: u32, name: u32, poly: u32, ptype: u32) -> i32;
        fn task_set_exception_ports(
            task: u32,
            mask: u32,
            port: u32,
            behavior: u32,
            flavor: u32,
        ) -> i32;
        fn mach_msg(
            msg: *mut u8,
            opt: u32,
            snd: u32,
            rcv: u32,
            port: u32,
            timeout: u32,
            notify: u32,
        ) -> i32;
    }

    const RCV: u32 = 2;
    const SND: u32 = 1;

    /// Mach port name of the thread currently inside a JNI
    /// `call_native` fnptr call (0 = idle). The watchdog samples this
    /// thread at high rate while guest code runs.
    pub static JNI_ACT_PORT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    /// Guest TCB of the thread currently inside a JNI call — the
    /// watchdog reads the call-attribution slots out of it.
    pub static JNI_ACT_TCB: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    /// Watchdog: samples the JNI-calling thread's pc every ~200us
    /// while guest code runs. A thread suspended mid-exception-
    /// delivery still yields its state, so the last samples before
    /// the kernel's SIGKILL name the faulting insn — the record BSD
    /// delivery can never show us.
    pub fn start_watchdog() {
        // Debug-only: task_for_pid/thread_get_state needs a Developer
        // Tools grant and the sampling loop burns a core.
        if std::env::var_os("OVELO_WATCH").is_none() {
            return;
        }
        extern "C" {
            fn thread_get_state(t: u32, flavor: u32, state: *mut u32, cnt: *mut u32) -> i32;
        }
        std::thread::Builder::new()
            .name("exc-watch".into())
            .spawn(|| unsafe {
                let mut last: u64 = u64::MAX;
                let mut last_call = [0u64; 2];
                loop {
                    let t = JNI_ACT_PORT.load(std::sync::atomic::Ordering::Acquire);
                    if t == 0 {
                        std::hint::spin_loop();
                        continue;
                    }
                    let mut st = [0u32; 68];
                    let mut n = 68u32;
                    let gkr = thread_get_state(t, 6, st.as_mut_ptr(), &mut n);
                    if gkr != 0 {
                        eprintln!("[watch] tport={t} get_state kr={gkr}");
                    } else {
                        let pc = (st[64] as u64) | ((st[65] as u64) << 32);
                        if pc != last {
                            last = pc;
                            eprintln!("[watch] pc={pc:#x} so={:#x?}", super::so_off(pc as usize));
                        }
                    }
                    // Call attribution: the thunk wrote the import target
                    // and the guest call site into TCB+0x7fe8/+0x7ff0.
                    let tcb = JNI_ACT_TCB.load(std::sync::atomic::Ordering::Acquire);
                    if tcb != 0 {
                        let mut buf = [0u64; 2];
                        let mut got = 0u64;
                        let kr = super::mach_vm_read_overwrite(
                            super::mach_task_self(),
                            (tcb + 0x7fe8) as u64,
                            16,
                            buf.as_mut_ptr() as u64,
                            &mut got,
                        );
                        if kr == 0 && got == 16 && buf != last_call {
                            last_call = buf;
                            let name = super::target_name(buf[0] as usize)
                                .unwrap_or_else(|| format!("{:#x}", buf[0]));
                            eprintln!("[call] {name} <- so={:#x?}", super::so_off(buf[1] as usize));
                        }
                    }
                    // Spin: a fatal BRK is processed faster than any
                    // sleep granularity — every pc change matters.
                }
            })
            .ok();
    }

    // All mach exception types except EXC_RPC_ALERT / EXC_MACHINE /
    // EXC_CORPSE — includes SYSCALL/MACH_SYSCALL/GUARD/RESOURCE, any
    // of which could be the undeliverable kill we're hunting.
    const MASK: u32 = 0x0000_2FFE;
    // MACH_EXCEPTION_CODES | EXCEPTION_STATE
    const BEHAVIOR: u32 = 0x8000_0001;
    const ARM_THREAD_STATE64: u32 = 6;
    const STATE_CNT: usize = 68; // u32 words; pc = word 64..66 pair

    pub fn start() {
        unsafe {
            let me = super::mach_task_self();
            let mut port = 0u32;
            let kr = mach_port_allocate(me, 1 /* RECEIVE */, &mut port);
            if kr != 0 {
                eprintln!("[mach-exc] port_allocate failed kr={kr}");
                return;
            }
            let kr = mach_port_insert_right(me, port, port, 20 /* MAKE_SEND */);
            if kr != 0 {
                eprintln!("[mach-exc] insert_right failed kr={kr}");
                return;
            }
            let kr = task_set_exception_ports(me, MASK, port, BEHAVIOR, ARM_THREAD_STATE64);
            eprintln!("[mach-exc] task_set_exception_ports kr={kr} port={port}");
            if kr != 0 {
                return;
            }
            std::thread::Builder::new()
                .name("exc-listen".into())
                .spawn(move || loop {
                    let mut req = [0u8; 4096];
                    let kr = mach_msg(
                        req.as_mut_ptr() as *mut c_void as *mut u8,
                        RCV,
                        0,
                        req.len() as u32,
                        port,
                        0,
                        0,
                    );
                    if kr != 0 {
                        continue;
                    }
                    let rd = |o: usize| u32::from_ne_bytes(req[o..o + 4].try_into().unwrap());
                    let bits = rd(0);
                    let remote = rd(8);
                    // MACH_MSGH_BITS_COMPLEX: body descriptor count +
                    // port descriptors precede the NDR record.
                    let arg0 = if bits & 0x8000_0000 != 0 {
                        24 + 4 + rd(24) as usize * 12
                    } else {
                        24
                    };
                    let ndr = arg0;
                    let exc = rd(ndr + 8) as i32;
                    // thread port name is in the first body descriptor.
                    let thread_port = rd(28);
                    let code0 = i64::from_ne_bytes(req[ndr + 16..ndr + 24].try_into().unwrap());
                    let code1 = i64::from_ne_bytes(req[ndr + 24..ndr + 32].try_into().unwrap());
                    let flavor = rd(ndr + 32);
                    let scnt = rd(ndr + 36) as usize;
                    let st_off = ndr + 40;
                    let pc = if scnt >= STATE_CNT {
                        u64::from_ne_bytes(req[st_off + 256..st_off + 264].try_into().unwrap())
                    } else {
                        0
                    };
                    // A thread faulting in a way the BSD handler can't
                    // recover (e.g. branch to a null pc) re-faults the
                    // instant the reply lands — a ~20k/s exception
                    // livelock. After a few identical reports suspend
                    // the thread: it cannot make progress anyway.
                    static SEEN: std::sync::atomic::AtomicU64 =
                        std::sync::atomic::AtomicU64::new(0);
                    static REPEATS: std::sync::atomic::AtomicU64 =
                        std::sync::atomic::AtomicU64::new(0);
                    let key = (thread_port as u64) << 32 ^ code0 as u64 ^ code1 as u64;
                    let n = if key == SEEN.swap(key, std::sync::atomic::Ordering::Relaxed) {
                        REPEATS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1
                    } else {
                        REPEATS.store(0, std::sync::atomic::Ordering::Relaxed);
                        0
                    };
                    if n < 8 {
                        eprintln!(
                            "[mach-exc] type={exc} code=({code0:#x},{code1:#x}) pc={pc:#x} tport={thread_port} so={:#x?}",
                            super::so_off(pc as usize)
                        );
                    }
                    // Only the null-branch signature (fault addr 0)
                    // is unrecoverable by definition; anything else
                    // may still be a healable TLS/data fault the BSD
                    // handler recovers — never suspend those.
                    if n == 8 && code1 == 0 {
                        extern "C" {
                            fn thread_suspend(t: u32) -> i32;
                            fn thread_get_state(
                                t: u32,
                                flavor: u32,
                                state: *mut u32,
                                cnt: *mut u32,
                            ) -> i32;
                            fn mach_vm_region(
                                task: u32,
                                addr: *mut u64,
                                size: *mut u64,
                                flavor: i32,
                                info: *mut i32,
                                count: *mut u32,
                                object: *mut u32,
                            ) -> i32;
                        }
                        // arm64_thread_state64 (flavor 6): x0..x28,
                        // fp, lr, sp, pc — words 60..64 hold
                        // fp/lr/sp/pc pairs.
                        let mut st = [0u32; 68];
                        let mut cnt = 68u32;
                        let gkr =
                            thread_get_state(thread_port, 6, st.as_mut_ptr(), &mut cnt);
                        if gkr == 0 {
                            let w = |i: usize| {
                                u64::from(st[i]) | (u64::from(st[i + 1]) << 32)
                            };
                            // x0..x28 = words 0..57; fp=58, lr=60,
                            // sp=62, pc=64 (each reg = 2 words).
                            let (fp, lr, sp, rpc) = (w(58), w(60), w(62), w(64));
                            let x16 = w(32);
                            let tname = super::target_name(x16 as usize)
                                .unwrap_or_else(|| "?".into());
                            let mut insw = [0u32; 8];
                            {
                                let mut nb = 0u64;
                                super::mach_vm_read_overwrite(
                                    super::mach_task_self(),
                                    rpc.wrapping_sub(16),
                                    32,
                                    insw.as_mut_ptr() as u64,
                                    &mut nb,
                                );
                            }
                            let jb = super::JIT_CODE_BASE
                                .load(std::sync::atomic::Ordering::Relaxed);
                            let je = super::JIT_CODE_END
                                .load(std::sync::atomic::Ordering::Relaxed);
                            let injit = rpc >= jb as u64 && rpc < je as u64;
                            // Region fingerprint: base + head bytes of
                            // the VM region holding pc — distinguishes
                            // guest .so / thunk arena / other maps.
                            let (mut rbase, mut rsize) = (rpc, 0u64);
                            let mut rinfo = [0i32; 9];
                            let mut rc = 9u32;
                            let mut robj = 0u32;
                            let rkr = mach_vm_region(
                                super::mach_task_self(),
                                &mut rbase,
                                &mut rsize,
                                9,
                                rinfo.as_mut_ptr(),
                                &mut rc,
                                &mut robj,
                            );
                            let mut head = [0u32; 4];
                            if rkr == 0 {
                                let mut nb = 0u64;
                                super::mach_vm_read_overwrite(
                                    super::mach_task_self(),
                                    rbase,
                                    16,
                                    head.as_mut_ptr() as u64,
                                    &mut nb,
                                );
                            }
                            eprintln!(
                                "[mach-exc] tport={thread_port} unrecoverable pc={rpc:#x}({:#x?}) jit={injit} insn={:#x?} reg=[{rbase:#x}+{rsize:#x} {:#x?}] lr={lr:#x}({:#x?}) sp={sp:#x} fp={fp:#x} x16={x16:#x}({tname}) l1={:#x} l2={:#x}; suspending",
                                head,
                                insw,
                                super::so_off(rpc as usize),
                                super::so_off(lr as usize),
                                if fp != 0
                                    && super::mem_writable(fp as *const c_void, 24)
                                {
                                    *(fp as *const u64).add(1)
                                } else {
                                    0
                                },
                                if fp != 0
                                    && super::mem_writable(fp as *const c_void, 40)
                                {
                                    *(fp as *const u64).add(3)
                                } else {
                                    0
                                },
                            );
                        } else {
                            eprintln!(
                                "[mach-exc] tport={thread_port} unrecoverable at pc={pc:#x} (get_state kr={gkr}); suspending"
                            );
                        }
                        thread_suspend(thread_port);
                    }
                    // Reply: echo state verbatim, nonzero RetCode ->
                    // "not handled" -> kernel continues to BSD signals.
                    let mut rep = [0u8; 512];
                    rep[0..4]
                        .copy_from_slice(&((bits & 0xFF) | ((bits >> 8) & 0xFF) << 8).to_ne_bytes());
                    let rep_size = 44 + scnt * 4;
                    rep[4..8].copy_from_slice(&(rep_size as u32).to_ne_bytes());
                    rep[8..12].copy_from_slice(&remote.to_ne_bytes());
                    // msgh_local_port = NULL, voucher = NULL
                    rep[20..24].copy_from_slice(&(rd(20) + 100).to_ne_bytes());
                    rep[24..32].copy_from_slice(&req[ndr..ndr + 8]); // NDR
                    rep[32..36].copy_from_slice(&5u32.to_ne_bytes()); // RetCode: EIO -> chain on
                    rep[36..40].copy_from_slice(&flavor.to_ne_bytes());
                    rep[40..44].copy_from_slice(&(scnt as u32).to_ne_bytes());
                    rep[44..44 + scnt * 4].copy_from_slice(&req[st_off..st_off + scnt * 4]);
                    mach_msg(
                        rep.as_mut_ptr() as *mut c_void as *mut u8,
                        SND,
                        rep_size as u32,
                        0,
                        0,
                        0,
                        0,
                    );
                })
                .ok();
        }
    }
}

#[cfg(target_os = "macos")]
extern "C" fn sh_kill(pid: i32, sig: i32) -> i32 {
    let mut lr = 0usize;
    unsafe { core::arch::asm!("mov {}, x30", out(reg) lr) };
    eprintln!("[kill] pid={pid} sig={sig} caller={:#x?}", so_off(lr));
    unsafe { kill(pid, sig) }
}

#[cfg(target_os = "macos")]
extern "C" fn sh_killpg(pgrp: i32, sig: i32) -> i32 {
    let mut lr = 0usize;
    unsafe { core::arch::asm!("mov {}, x30", out(reg) lr) };
    eprintln!("[killpg] pgrp={pgrp} sig={sig} caller={:#x?}", so_off(lr));
    unsafe { killpg(pgrp, sig) }
}

#[cfg(target_os = "macos")]
extern "C" fn sh_tgkill(pid: i32, tid: i32, sig: i32) -> i32 {
    let mut lr = 0usize;
    unsafe { core::arch::asm!("mov {}, x30", out(reg) lr) };
    eprintln!(
        "[tgkill] pid={pid} tid={tid} sig={sig} caller={:#x?}",
        so_off(lr)
    );
    // Darwin has no tgkill; pthread_kill needs a thread handle, which
    // the guest's Linux tid can't give us — fall back to kill() on
    // the process when the caller targets a foreign thread.
    unsafe { kill(pid, sig) }
}

#[cfg(target_os = "macos")]
extern "C" fn sh_pthread_kill(thread: usize, sig: i32) -> i32 {
    let mut lr = 0usize;
    unsafe { core::arch::asm!("mov {}, x30", out(reg) lr) };
    eprintln!(
        "[pthread_kill] thread={thread:#x} sig={sig} caller={:#x?}",
        so_off(lr)
    );
    unsafe { pthread_kill(thread, sig) }
}

#[cfg(target_os = "macos")]
extern "C" fn sh_raise(sig: i32) -> i32 {
    let mut lr = 0usize;
    unsafe { core::arch::asm!("mov {}, x30", out(reg) lr) };
    eprintln!("[raise] sig={sig} caller={:#x?}", so_off(lr));
    unsafe { raise(sig) }
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
    fn killpg(pgrp: i32, sig: i32) -> i32;
    fn pthread_kill(thread: usize, sig: i32) -> i32;
    fn raise(sig: i32) -> i32;
}

/// `__emutls_get_address(struct __emutls_control*)` — Bionic's emulated
/// TLS for `__thread` in libs built without native TLS. One guest-zone
/// cell per (control, thread), lazily allocated + initialized.
#[cfg(target_os = "macos")]
extern "C" fn sh_emutls_addr(ctrl: usize) -> usize {
    static EMU: std::sync::LazyLock<Mutex<HashMap<(usize, u64), usize>>> =
        std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));
    let tid = unsafe { mach_thread_id() };
    let key = (ctrl, tid);
    let mut m = EMU.lock().unwrap();
    if let Some(&a) = m.get(&key) {
        return a;
    }
    let (size, init) = unsafe { (*(ctrl as *const usize), *(ctrl as *const usize).add(3)) };
    let a = unsafe { malloc_zone_calloc(guest_zone(), size.max(1), 1) } as usize;
    if init != 0 {
        unsafe {
            std::ptr::copy_nonoverlapping(init as *const u8, a as *mut u8, size.min(1 << 20))
        };
    }
    m.insert(key, a);
    a
}

/// glClearDepthf/glDepthRangef — ES names for the desktop-GL doubles.
// ----------------------------------------------------------------
// Real GL for native renderers (Godot): guest `gl*` imports resolve
// to macOS OpenGL.framework (dlopen'd RTLD_GLOBAL by
// ensure_gl_framework). A CGL context plus a renderbuffer-backed FBO
// masquerades as framebuffer 0 — the guest draws into it, and
// `glctx_read_rgba` pulls pixels that the dex-side eglSwapBuffers
// feeds to the presenter as a Bitmap op.
// ----------------------------------------------------------------
#[cfg(target_os = "macos")]
pub(crate) mod glctx {
    use std::ffi::c_void;
    use std::sync::Mutex;

    type CGLCtx = *mut c_void;
    type Cglpf = *mut c_void;
    #[link(name = "OpenGL", kind = "framework")]
    extern "C" {
        fn CGLChoosePixelFormat(attribs: *const i32, fmt: *mut Cglpf, n: *mut i32) -> i32;
        fn CGLCreateContext(fmt: Cglpf, share: CGLCtx, ctx: *mut CGLCtx) -> i32;
        fn CGLSetCurrentContext(ctx: CGLCtx) -> i32;
    }

    fn glp<T>(name: &str) -> Option<T> {
        super::host_dlsym(name).map(|p| unsafe { std::mem::transmute_copy::<usize, T>(&p) })
    }

    struct S {
        ctx: usize,
        fbo: u32,
        rb: u32,
        vao: u32,
        w: i32,
        h: i32,
    }
    static G: Mutex<Option<S>> = Mutex::new(None);

    /// Create the context on first use and (re)build the screen FBO
    /// at `w`×`h`; leaves it bound as GL_FRAMEBUFFER.
    pub fn make_current(w: i32, h: i32) {
        super::ensure_gl_framework();
        let mut g = G.lock().unwrap();
        if g.is_none() {
            unsafe {
                // kCglpfAAccelerated(72) + kCglpfAOpenGLProfile(99)=
                // kCGLOGLPVersion_4_1_Core(0x3200), terminator. Core
                // profile gives GLSL 410 (uint, texture(), in/out)
                // which the ES->GLSL rewrite in sh_gl_shader_source
                // targets; the legacy profile caps at GLSL 1.20 and
                // can't express Godot's uint-packing shaders.
                let attrs = [72i32, 99, 0x3200, 0];
                let mut pf = std::ptr::null_mut();
                let mut npix = 0i32;
                CGLChoosePixelFormat(attrs.as_ptr(), &mut pf, &mut npix);
                let mut ctx = std::ptr::null_mut();
                CGLCreateContext(pf, std::ptr::null_mut(), &mut ctx);
                CGLSetCurrentContext(ctx);
                // Core profile draws need a VAO bound; the guest's
                // GLES2 path never creates one, so bind ours forever.
                let mut vao = 0u32;
                let gen_vao: Option<unsafe extern "C" fn(i32, *mut u32)> = glp("glGenVertexArrays");
                let bind_vao: Option<unsafe extern "C" fn(u32)> = glp("glBindVertexArray");
                if let (Some(gv), Some(bv)) = (gen_vao, bind_vao) {
                    gv(1, &mut vao);
                    bv(vao);
                }
                *g = Some(S {
                    ctx: ctx as usize,
                    fbo: 0,
                    rb: 0,
                    vao,
                    w: 0,
                    h: 0,
                });
            }
        }
        let s = g.as_mut().unwrap();
        unsafe { CGLSetCurrentContext(s.ctx as CGLCtx) };
        // Keep our VAO bound — GLES2 guests never call
        // glBindVertexArray, and core profile draws need one.
        if let Some(bv) = glp::<unsafe extern "C" fn(u32)>("glBindVertexArray") {
            unsafe { bv(s.vao) };
        }
        if s.w != w || s.h != h {
            unsafe {
                let gen_fbo: unsafe extern "C" fn(i32, *mut u32) =
                    glp("glGenFramebuffers").unwrap_or(noop_fbo_gen);
                let bind_fbo: unsafe extern "C" fn(i32, u32) =
                    glp("glBindFramebuffer").unwrap_or(noop_fbo_bind);
                let gen_rb: unsafe extern "C" fn(i32, *mut u32) =
                    glp("glGenRenderbuffers").unwrap_or(noop_fbo_gen);
                let bind_rb: unsafe extern "C" fn(i32, u32) =
                    glp("glBindRenderbuffer").unwrap_or(noop_fbo_bind);
                let rb_store: unsafe extern "C" fn(i32, i32, i32, i32) =
                    glp("glRenderbufferStorage").unwrap_or(noop_rbs);
                let fbo_attach: unsafe extern "C" fn(i32, i32, i32, u32) =
                    glp("glFramebufferRenderbuffer").unwrap_or(noop_attach);
                gen_fbo(1, &mut s.fbo);
                bind_fbo(0x8D40, s.fbo); // GL_FRAMEBUFFER
                gen_rb(1, &mut s.rb);
                bind_rb(0x8D41, s.rb); // GL_RENDERBUFFER
                rb_store(0x8D41, 0x8058, w, h); // GL_RGBA8
                fbo_attach(0x8D40, 0x8CE0, 0x8D41, s.rb); // COLOR_ATTACHMENT0
                s.w = w;
                s.h = h;
            }
        }
    }

    unsafe extern "C" fn noop_fbo_gen(_: i32, _: *mut u32) {}
    unsafe extern "C" fn noop_fbo_bind(_: i32, _: u32) {}
    unsafe extern "C" fn noop_rbs(_: i32, _: i32, _: i32, _: i32) {}
    unsafe extern "C" fn noop_attach(_: i32, _: i32, _: i32, _: u32) {}

    /// The FBO id the guest sees as 0.
    pub fn screen_fbo() -> u32 {
        G.lock().unwrap().as_ref().map(|s| s.fbo).unwrap_or(0)
    }

    /// Bottom-up RGBA8 readback of the screen FBO, flipped to
    /// top-down for the presenter.
    pub fn read_rgba() -> Option<(Vec<u8>, i32, i32)> {
        let g = G.lock().unwrap();
        let s = g.as_ref()?;
        if s.fbo == 0 || s.w <= 0 || s.h <= 0 {
            return None;
        }
        unsafe {
            let bind_fbo: unsafe extern "C" fn(i32, u32) = glp("glBindFramebuffer")?;
            let get_iv: unsafe extern "C" fn(i32, *mut i32) = glp("glGetIntegerv")?;
            let read: unsafe extern "C" fn(i32, i32, i32, i32, i32, i32, *mut c_void) =
                glp("glReadPixels")?;
            // Save the app's current binding — leaving our screen FBO
            // bound leaks guest draws into the readback target.
            let mut prev = 0i32;
            get_iv(0x8CA6, &mut prev); // GL_FRAMEBUFFER_BINDING
            bind_fbo(0x8D40, s.fbo);
            let mut buf = vec![0u8; (s.w * s.h * 4) as usize];
            read(
                0,
                0,
                s.w,
                s.h,
                0x1908,
                0x1401,
                buf.as_mut_ptr() as *mut c_void,
            );
            bind_fbo(0x8D40, prev as u32);
            let stride = (s.w * 4) as usize;
            let mut out = vec![0u8; buf.len()];
            for y in 0..s.h as usize {
                let src = y * stride;
                let dst = (s.h as usize - 1 - y) * stride;
                out[dst..dst + stride].copy_from_slice(&buf[src..src + stride]);
            }
            Some((out, s.w, s.h))
        }
    }
}

/// Guest `glBindFramebuffer(target, fbo)` — 0 is the on-screen
/// framebuffer; route it to our offscreen FBO.
#[cfg(target_os = "macos")]
extern "C" fn sh_gl_bind_framebuffer(target: i32, fbo: u32) {
    let real = if fbo == 0 { glctx::screen_fbo() } else { fbo };
    if let Some(f) = host_dlsym("glBindFramebuffer") {
        type F = unsafe extern "C" fn(i32, u32);
        unsafe { std::mem::transmute::<usize, F>(f)(target, real) };
    }
}

#[cfg(target_os = "macos")]
extern "C" fn sh_gl_clear_depthf(d: f32) {
    ensure_gl_framework();
    type F = unsafe extern "C" fn(f64);
    if let Some(p) = host_dlsym("glClearDepth") {
        unsafe { std::mem::transmute::<usize, F>(p)(d as f64) };
    }
}

#[cfg(target_os = "macos")]
extern "C" fn sh_gl_depth_rangef(n: f32, f: f32) {
    ensure_gl_framework();
    type F = unsafe extern "C" fn(f64, f64);
    if let Some(p) = host_dlsym("glDepthRanged") {
        unsafe { std::mem::transmute::<usize, F>(p)(n as f64, f as f64) };
    }
}

/// Shader id -> GL_*_SHADER, from glCreateShader calls.
#[cfg(target_os = "macos")]
static SHADER_TYPES: Mutex<Option<HashMap<u32, i32>>> = Mutex::new(None);

#[cfg(target_os = "macos")]
extern "C" fn sh_gl_create_shader(ty: i32) -> u32 {
    ensure_gl_framework();
    let id = host_dlsym("glCreateShader")
        .map(|f| unsafe { std::mem::transmute::<usize, unsafe extern "C" fn(i32) -> u32>(f)(ty) })
        .unwrap_or(0);
    if let Ok(mut g) = SHADER_TYPES.lock() {
        g.get_or_insert_with(HashMap::new).insert(id, ty);
    }
    id
}

/// Rewrite an ES shader body for GLSL 410 core: drop
/// `#version`/`#extension`/`precision` lines and precision keywords,
/// map `attribute`/`varying` to `in`/`out`, `texture2D`/`textureCube`
/// to `texture`, and `gl_FragColor`/`gl_FragData` to declared outs.
#[cfg(target_os = "macos")]
fn glsl_es_to_gl(src: &str, frag: bool) -> String {
    let mut out = String::with_capacity(src.len() + 96);
    out.push_str("#version 410 core\n");
    if frag {
        if src.contains("gl_FragData") {
            out.push_str("out vec4 ovelo_fragdata[8];\n");
        } else if src.contains("gl_FragColor") {
            out.push_str("layout(location=0) out vec4 ovelo_frag;\n");
        }
    }
    for line in src.split('\n') {
        let t = line.trim_start();
        if t.starts_with("#version") || t.starts_with("#extension") || t.starts_with("precision") {
            continue;
        }
        let mut l = line.to_string();
        for kw in ["highp", "mediump", "lowp"] {
            l = l
                .replace(&format!("{kw} "), "")
                .replace(&format!("{kw}\t"), "")
                .replace(&format!("{kw}\n"), "");
        }
        if frag {
            l = l
                .replace("gl_FragColor", "ovelo_frag")
                .replace("gl_FragData", "ovelo_fragdata")
                .replace("varying ", "in ")
                .replace("varying\t", "in\t");
        } else {
            l = l
                .replace("attribute ", "in ")
                .replace("attribute\t", "in\t")
                .replace("varying ", "out ")
                .replace("varying\t", "out\t");
        }
        l = l
            .replace("texture2DProj", "textureProj")
            .replace("texture2DLodEXT", "textureLod")
            .replace("textureCubeLodEXT", "textureLod")
            .replace("texture2D", "texture")
            .replace("textureCube", "texture")
            .replace("gl_FragDepthEXT", "gl_FragDepth");
        out.push_str(&l);
        out.push('\n');
    }
    out
}

#[cfg(target_os = "macos")]
extern "C" fn sh_gl_shader_source(shader: u32, count: i32, strings: usize, lengths: usize) {
    ensure_gl_framework();
    unsafe {
        let mut src = String::new();
        for i in 0..count.clamp(0, 4096) as usize {
            let p = *(strings as *const usize).add(i) as *const i8;
            if p.is_null() {
                continue;
            }
            let len = if lengths != 0 {
                *(lengths as *const i32).add(i)
            } else {
                -1
            };
            if crate::dbg_flag("OVELO_GLSLSRC") {
                eprintln!(
                    "[glsl-part shader={shader} i={i} p={:#x} len={len}]",
                    p as usize
                );
            }
            if len >= 0 {
                if !src_readable(p as *const c_void, len as usize) {
                    continue;
                }
                let b = std::slice::from_raw_parts(p as *const u8, len as usize);
                src.push_str(&String::from_utf8_lossy(b));
            } else if src_readable(p as *const c_void, 1) {
                src.push_str(&std::ffi::CStr::from_ptr(p).to_string_lossy());
                if crate::dbg_flag("OVELO_GLSLSRC") && src_readable(p as *const c_void, 65536) {
                    // A truncated-looking string may just hold an
                    // embedded NUL: probe past it for more text.
                    let b = std::slice::from_raw_parts(p as *const u8, 65536);
                    if let Some(nul) = b.iter().position(|&c| c == 0) {
                        let tail = &b[nul + 1..(nul + 1 + 512).min(65536)];
                        let show: String = tail
                            .iter()
                            .take_while(|&&c| c.is_ascii() && c != 0)
                            .map(|&c| c as char)
                            .collect();
                        if !show.is_empty() {
                            eprintln!("[glsl-nul shader={shader} at={nul} tail={show:?}]");
                        }
                    }
                }
            }
        }
        if let Some(f) = host_dlsym("glShaderSource") {
            type F = unsafe extern "C" fn(u32, i32, *const usize, usize);
            let frag = SHADER_TYPES
                .lock()
                .ok()
                .and_then(|g| g.as_ref().and_then(|m| m.get(&shader).copied()))
                .map(|t| t == 0x8B30) // GL_FRAGMENT_SHADER
                .unwrap_or_else(|| src.contains("gl_FragColor") || src.contains("gl_FragData"));
            let t_src = glsl_es_to_gl(&src, frag);
            if crate::dbg_flag("OVELO_GLSLSRC") {
                eprintln!("[glsl-src shader={shader}]\n{t_src}");
            }
            let t = std::ffi::CString::new(t_src).unwrap_or_default();
            let arr = [t.as_ptr() as usize];
            std::mem::transmute::<usize, F>(f)(shader, 1, arr.as_ptr(), 0);
        }
    }
}

/// Log the real info log once per shader/program so Godot's
/// "shader failed to compile" diagnostics carry the GL error.
#[cfg(target_os = "macos")]
extern "C" fn sh_gl_get_shader_info_log(shader: u32, bufsize: i32, length: usize, log: usize) {
    ensure_gl_framework();
    if let Some(f) = host_dlsym("glGetShaderInfoLog") {
        type F = unsafe extern "C" fn(u32, i32, *mut i32, *mut i8);
        unsafe {
            std::mem::transmute::<usize, F>(f)(shader, bufsize, length as *mut i32, log as *mut i8)
        };
        if log != 0 {
            let s = unsafe { std::ffi::CStr::from_ptr(log as *const i8).to_string_lossy() };
            if !s.trim().is_empty() {
                eprintln!("[glsl] {s}");
            }
        }
    }
}

fn builtin_shim(name: &str) -> Option<usize> {
    Some(match name {
        "__cxa_pure_virtual" => sh_pure_virtual as *const () as usize,
        "__stack_chk_fail" => sh_stack_chk_fail as *const () as usize,
        "__cxa_finalize" => sh_cxa_finalize as *const () as usize,
        "__cxa_atexit" => sh_cxa_atexit as *const () as usize,
        "atoi" | "strtol" => sh_atoi as *const () as usize,
        #[cfg(target_os = "macos")]
        "malloc" => sh_malloc as *const () as usize,
        #[cfg(target_os = "macos")]
        "_Znwm" | "_Znam" => sh_guest_new as *const () as usize,
        #[cfg(target_os = "macos")]
        "_ZnwmRKSt9nothrow_t" | "_ZnamRKSt9nothrow_t" => sh_guest_new_nothrow as *const () as usize,
        #[cfg(target_os = "macos")]
        "_ZnwmSt11align_val_t" | "_ZnamSt11align_val_t" => {
            sh_guest_new_aligned as *const () as usize
        }
        #[cfg(target_os = "macos")]
        "_ZnwmSt11align_val_tRKSt9nothrow_t" | "_ZnamSt11align_val_tRKSt9nothrow_t" => {
            sh_guest_new_aligned_nothrow as *const () as usize
        }
        #[cfg(target_os = "macos")]
        "free"
        | "_ZdlPv"
        | "_ZdaPv"
        | "_ZdlPvm"
        | "_ZdaPvm"
        | "_ZdlPvRKSt9nothrow_t"
        | "_ZdaPvRKSt9nothrow_t"
        | "_ZdlPvSt11align_val_t"
        | "_ZdaPvSt11align_val_t"
        | "_ZdlPvmSt11align_val_t"
        | "_ZdaPvmSt11align_val_t"
        | "_ZdlPvSt11align_val_tRKSt9nothrow_t"
        | "_ZdaPvSt11align_val_tRKSt9nothrow_t" => sh_guest_delete as *const () as usize,
        #[cfg(target_os = "macos")]
        "calloc" => sh_calloc as *const () as usize,
        #[cfg(target_os = "macos")]
        "realloc" => sh_realloc as *const () as usize,
        #[cfg(target_os = "macos")]
        "memalign" | "aligned_alloc" => sh_memalign as *const () as usize,
        #[cfg(target_os = "macos")]
        "valloc" => sh_valloc as *const () as usize,
        #[cfg(target_os = "macos")]
        "pvalloc" => sh_pvalloc as *const () as usize,
        #[cfg(target_os = "macos")]
        "mremap" => sh_mremap as *const () as usize,
        #[cfg(target_os = "macos")]
        "mmap" => sh_guest_mmap as *const () as usize,
        #[cfg(target_os = "macos")]
        "munmap" => sh_guest_munmap as *const () as usize,
        #[cfg(target_os = "macos")]
        "posix_memalign" => sh_posix_memalign as *const () as usize,
        #[cfg(target_os = "macos")]
        "malloc_usable_size" | "malloc_size" => sh_malloc_usable_size as *const () as usize,
        #[cfg(target_os = "macos")]
        "strdup" => sh_strdup as *const () as usize,
        #[cfg(target_os = "macos")]
        "strndup" => sh_strndup as *const () as usize,
        "sincosf" => sh_sincosf as *const () as usize,
        "sincos" => sh_sincos as *const () as usize,
        #[cfg(target_os = "macos")]
        "memcpy" => sh_memcpy as *const () as usize,
        #[cfg(target_os = "macos")]
        "memmove" => sh_memmove as *const () as usize,
        #[cfg(target_os = "macos")]
        "memset" => sh_memset as *const () as usize,
        #[cfg(target_os = "macos")]
        "strcpy" => sh_strcpy as *const () as usize,
        #[cfg(target_os = "macos")]
        "strncpy" => sh_strncpy as *const () as usize,
        "__strlen_chk" => sh_strlen_chk as *const () as usize,
        "__ctype_get_mb_cur_max" => sh_mb_cur_max as *const () as usize,
        "__strcpy_chk" => sh_strcpy_chk as *const () as usize,
        "__strncpy_chk2" => sh_strncpy_chk2 as *const () as usize,
        "__strchr_chk" => sh_strchr_chk as *const () as usize,
        "__memmove_chk" => sh_memmove_chk as *const () as usize,
        "__strcat_chk" => sh_strcat_chk as *const () as usize,
        "__vsnprintf_chk" => sh_vsnprintf_chk as *const () as usize,
        "__errno" => sh_errno as *const () as usize,
        "__assert2" => sh_assert2 as *const () as usize,
        "getauxval" => sh_zero as *const () as usize,
        "prctl" => sh_zero as *const () as usize,
        "sched_setaffinity" => sh_zero as *const () as usize,
        "syscall" => sh_neg1 as *const () as usize,
        "gettid" => sh_gettid as *const () as usize,
        #[cfg(target_arch = "aarch64")]
        "pthread_create" => sh_pthread_create as *const () as usize,
        "__open_2" => sh_open2 as *const () as usize,
        "open" => sh_open as *const () as usize,
        "__system_property_get" => sh_system_property_get as *const () as usize,
        "sched_getaffinity" => sh_getaffinity as *const () as usize,
        "lseek64" => lseek as *const () as usize,
        "__memcpy_chk" => sh_memcpy_chk as *const () as usize,
        "__memset_chk" => sh_memset_chk as *const () as usize,
        "__read_chk" => sh_read_chk as *const () as usize,
        #[cfg(target_os = "macos")]
        "slCreateEngine" => sh_sl_create_engine as *const () as usize,
        #[cfg(target_os = "macos")]
        "AAssetManager_fromJava" => sh_asset_mgr_from_java as *const () as usize,
        #[cfg(target_os = "macos")]
        "AAssetManager_open" => sh_asset_open as *const () as usize,
        #[cfg(target_os = "macos")]
        "AAsset_read" => sh_asset_read as *const () as usize,
        #[cfg(target_os = "macos")]
        "AAsset_getLength" | "AAsset_getLength64" => sh_asset_len as *const () as usize,
        #[cfg(target_os = "macos")]
        "AAsset_seek" | "AAsset_seek64" => sh_asset_seek as *const () as usize,
        #[cfg(target_os = "macos")]
        "AAsset_close" => sh_asset_close as *const () as usize,
        #[cfg(target_os = "macos")]
        "ANativeWindow_fromSurface" => sh_native_window_from_surface as *const () as usize,
        #[cfg(target_os = "macos")]
        "eglGetCurrentContext" | "eglGetCurrentDisplay" | "eglGetCurrentSurface" => {
            sh_egl_current as *const () as usize
        }
        #[cfg(target_os = "macos")]
        "eglGetProcAddress" => sh_egl_get_proc as *const () as usize,
        #[cfg(target_os = "macos")]
        "glBindFramebuffer" | "glBindFramebufferEXT" | "glBindFramebufferOES" => {
            sh_gl_bind_framebuffer as *const () as usize
        }
        #[cfg(target_os = "macos")]
        "glClearDepthf" => sh_gl_clear_depthf as *const () as usize,
        #[cfg(target_os = "macos")]
        "glDepthRangef" => sh_gl_depth_rangef as *const () as usize,
        #[cfg(target_os = "macos")]
        "glShaderSource" | "glShaderSourceARB" => sh_gl_shader_source as *const () as usize,
        #[cfg(target_os = "macos")]
        "glCreateShader" => sh_gl_create_shader as *const () as usize,
        #[cfg(target_os = "macos")]
        "glGetShaderInfoLog" => sh_gl_get_shader_info_log as *const () as usize,
        #[cfg(target_os = "macos")]
        "ZSTD_trace_compress_begin"
        | "ZSTD_trace_compress_end"
        | "ZSTD_trace_decompress_begin"
        | "ZSTD_trace_decompress_end" => sh_void_n as *const () as usize,
        #[cfg(target_os = "macos")]
        "__cxa_guard_acquire" => sh_cxa_guard_acquire as *const () as usize,
        #[cfg(target_os = "macos")]
        "__cxa_guard_release" => sh_cxa_guard_release as *const () as usize,
        #[cfg(target_os = "macos")]
        "__cxa_thread_atexit" => sh_cxa_thread_atexit as *const () as usize,
        #[cfg(target_os = "macos")]
        "__dynamic_cast" => sh_dynamic_cast as *const () as usize,
        #[cfg(target_os = "macos")]
        "__FD_SET_chk" => sh_fd_set_chk as *const () as usize,
        #[cfg(target_os = "macos")]
        "pthread_gettid_np" => sh_gettid as *const () as usize,
        #[cfg(target_os = "macos")]
        "pthread_once" => sh_pthread_once as *const () as usize,
        // Bionic pthread sync objects have a different layout than
        // Darwin's — keyed guest-side implementation, not host calls.
        #[cfg(target_os = "macos")]
        "pthread_mutex_init" => sh_mutex_init as *const () as usize,
        #[cfg(target_os = "macos")]
        "pthread_mutex_destroy" => sh_mutex_destroy as *const () as usize,
        #[cfg(target_os = "macos")]
        "pthread_mutex_lock" => sh_mutex_lock as *const () as usize,
        #[cfg(target_os = "macos")]
        "pthread_mutex_trylock" => sh_mutex_trylock as *const () as usize,
        #[cfg(target_os = "macos")]
        "pthread_mutex_unlock" => sh_mutex_unlock as *const () as usize,
        #[cfg(target_os = "macos")]
        "pthread_mutexattr_init" => sh_mutexattr_init as *const () as usize,
        #[cfg(target_os = "macos")]
        "pthread_mutexattr_destroy" => sh_attr_noop as *const () as usize,
        #[cfg(target_os = "macos")]
        "pthread_mutexattr_settype" => sh_mutexattr_settype as *const () as usize,
        #[cfg(target_os = "macos")]
        "pthread_mutexattr_gettype" => sh_mutexattr_gettype as *const () as usize,
        #[cfg(target_os = "macos")]
        "pthread_mutexattr_setpshared" => sh_attr_noop as *const () as usize,
        #[cfg(target_os = "macos")]
        "pthread_cond_init" => sh_cond_init as *const () as usize,
        #[cfg(target_os = "macos")]
        "pthread_cond_destroy" => sh_cond_destroy as *const () as usize,
        #[cfg(target_os = "macos")]
        "pthread_cond_wait" => sh_cond_wait as *const () as usize,
        #[cfg(target_os = "macos")]
        "pthread_cond_timedwait" => sh_cond_timedwait as *const () as usize,
        #[cfg(target_os = "macos")]
        "pthread_cond_signal" => sh_cond_signal as *const () as usize,
        #[cfg(target_os = "macos")]
        "pthread_cond_broadcast" => sh_cond_broadcast as *const () as usize,
        #[cfg(target_os = "macos")]
        "pthread_condattr_init" => sh_mutexattr_init as *const () as usize,
        #[cfg(target_os = "macos")]
        "pthread_condattr_destroy" => sh_attr_noop as *const () as usize,
        #[cfg(target_os = "macos")]
        "pthread_rwlock_init" => sh_rwlock_init as *const () as usize,
        #[cfg(target_os = "macos")]
        "pthread_rwlock_destroy" => sh_rwlock_destroy as *const () as usize,
        #[cfg(target_os = "macos")]
        "pthread_rwlock_rdlock" => sh_rwlock_rdlock as *const () as usize,
        #[cfg(target_os = "macos")]
        "pthread_rwlock_tryrdlock" => sh_rwlock_tryrdlock as *const () as usize,
        #[cfg(target_os = "macos")]
        "pthread_rwlock_wrlock" => sh_rwlock_wrlock as *const () as usize,
        #[cfg(target_os = "macos")]
        "pthread_rwlock_trywrlock" => sh_rwlock_trywrlock as *const () as usize,
        #[cfg(target_os = "macos")]
        "pthread_rwlock_unlock" => sh_rwlock_unlock as *const () as usize,
        #[cfg(target_os = "macos")]
        "pthread_rwlockattr_init" => sh_mutexattr_init as *const () as usize,
        #[cfg(target_os = "macos")]
        "pthread_rwlockattr_destroy" => sh_attr_noop as *const () as usize,
        #[cfg(target_os = "macos")]
        "vfprintf" => sh_vfprintf as *const () as usize,
        #[cfg(target_os = "macos")]
        "fprintf" => sh_fprintf as *const () as usize,
        #[cfg(target_os = "macos")]
        "pthread_setname_np" => sh_setname as *const () as usize,
        "dl_iterate_phdr" => sh_dl_iterate_phdr as *const () as usize,
        "android_set_abort_message" => sh_set_abort_message as *const () as usize,
        "AndroidBitmap_getInfo" => sh_ab_get_info as *const () as usize,
        "AndroidBitmap_lockPixels" => sh_ab_lock_pixels as *const () as usize,
        "AndroidBitmap_unlockPixels" => sh_ab_unlock_pixels as *const () as usize,
        "__android_log_print" | "__android_log_vprint" => sh_log_print as *const () as usize,
        "__android_log_write" => sh_log_write as *const () as usize,
        #[cfg(target_os = "macos")]
        "exit" | "exit_group" => sh_exit as *const () as usize,
        #[cfg(target_os = "macos")]
        "_exit" | "_Exit" => sh_underscore_exit as *const () as usize,
        #[cfg(target_os = "macos")]
        "abort" => sh_abort as *const () as usize,
        #[cfg(target_os = "macos")]
        "__cxa_throw" => sh_cxa_throw as *const () as usize,
        #[cfg(target_os = "macos")]
        "sigaction" => sh_sigaction as *const () as usize,
        #[cfg(target_os = "macos")]
        "kill" => sh_kill as *const () as usize,
        #[cfg(target_os = "macos")]
        "killpg" => sh_killpg as *const () as usize,
        #[cfg(target_os = "macos")]
        "tgkill" => sh_tgkill as *const () as usize,
        #[cfg(target_os = "macos")]
        "pthread_kill" => sh_pthread_kill as *const () as usize,
        #[cfg(target_os = "macos")]
        "raise" => sh_raise as *const () as usize,
        #[cfg(target_os = "macos")]
        "__emutls_get_address" => sh_emutls_addr as *const () as usize,
        _ => return None,
    })
    .filter(|&a| a != 0)
}

struct Dyn {
    strtab: usize,
    symtab: usize,
    strsz: usize,
    rela: usize,
    relasz: usize,
    jmprel: usize,
    pltrelsz: usize,
    init: usize,
    init_array: usize,
    init_array_sz: usize,
    hash: usize,
    gnu_hash: usize,
    relr: usize,
    relrsz: usize,
    android_rel: usize,
    android_relsz: usize,
    android_rela: usize,
    android_relasz: usize,
    needed: Vec<usize>,
}

/// Exports of every lib loaded in this process: symbol → candidate
/// addresses in load order — the dynamic linker's "global scope"
/// later relocations resolve against (a lib's DT_NEEDED deps are
/// always loaded before it). The FIRST candidate is the effective
/// binding (first registration wins, matching breadth-first order);
/// shadowed duplicates wait underneath so scrubbing a dead lib's
/// range uncovers the next provider instead of losing the symbol.
static GLOBAL_SYMS: LazyLock<Mutex<HashMap<String, Vec<usize>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
/// Scrub every candidate inside `[map, map+span)` from `g`; empty
/// buckets go away.
fn globals_scrub(g: &mut HashMap<String, Vec<usize>>, lo: usize, hi: usize) {
    g.retain(|_, cands| {
        cands.retain(|a| !(*a >= lo && *a < hi));
        !cands.is_empty()
    });
}
/// Sonames already mapped — a second DT_NEEDED edge or `loadLibrary`
/// call reuses the image instead of double-mapping it.
static LOADED_SONAMES: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// DT_NEEDED sonames of `elf` (dependencies), in declared order.
/// Names live in the file's dynstr; DT_STRTAB is a vaddr converted
/// back to a file offset via the PT_LOAD table.
fn elf_needed(elf: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    if elf.len() < 64 || &elf[0..4] != b"\x7fELF" {
        return out;
    }
    let phoff = rd64(elf, 32) as usize;
    let phentsize = rd16(elf, 54) as usize;
    let phnum = rd16(elf, 56) as usize;
    let mut loads: Vec<(usize, usize, usize)> = Vec::new(); // (vaddr, off, filesz)
    let mut dyn_off = 0usize;
    for i in 0..phnum {
        let p = phoff + i * phentsize;
        if p + 56 > elf.len() {
            break;
        }
        match rd32(elf, p) {
            PT_LOAD => loads.push((
                rd64(elf, p + 16) as usize,
                rd64(elf, p + 8) as usize,
                rd64(elf, p + 32) as usize,
            )),
            PT_DYNAMIC => dyn_off = rd64(elf, p + 8) as usize,
            _ => {}
        }
    }
    if dyn_off == 0 {
        return out;
    }
    let v2o = |va: usize| -> Option<usize> {
        loads
            .iter()
            .find_map(|&(v, o, s)| (va >= v && va < v + s).then_some(o + (va - v)))
    };
    let mut strtab = 0usize;
    let mut strsz = 0usize;
    let mut need_offs = Vec::new();
    let mut e = dyn_off;
    while e + 16 <= elf.len() {
        let tag = rd64(elf, e) as i64;
        let val = rd64(elf, e + 8) as usize;
        if tag == DT_NULL {
            break;
        }
        match tag {
            DT_NEEDED => need_offs.push(val),
            DT_STRTAB => strtab = v2o(val).unwrap_or(0),
            DT_STRSZ => strsz = val,
            _ => {}
        }
        e += 16;
    }
    for off in need_offs {
        if off >= strsz {
            continue;
        }
        let start = strtab + off;
        if start >= elf.len() {
            continue;
        }
        let end = elf[start..]
            .iter()
            .position(|&b| b == 0)
            .map(|n| start + n)
            .unwrap_or(elf.len());
        out.push(String::from_utf8_lossy(&elf[start..end]).into_owned());
    }
    out
}

/// Load `elf` plus, recursively, every DT_NEEDED library `lookup`
/// can supply (APK-bundled dependencies). Returns libs in
/// dependency-first order; the tail is the requested lib. Each
/// dep's exports join `GLOBAL_SYMS` before the dependent relocates,
/// so `libjxlcoder.so` finds `Jxl*` in `libjxl.so` etc.
pub fn load_tree(
    name: &str,
    elf: &[u8],
    lookup: &mut dyn FnMut(&str) -> Option<Vec<u8>>,
) -> Result<Vec<NativeLib>> {
    let mut out = Vec::new();
    match load_tree_into(name, elf, lookup, &mut out, 0) {
        Ok(()) => {
            // Bionic order: the whole tree relocates first, then ctors
            // run dependency-first. A dep's DT_INIT may legitimately
            // call back into its dependent (callback registration) —
            // running inits during the recursive load would hit a
            // still-unrelocated, non-executable parent image.
            check_all_zones("load_tree.reloc");
            for (_, inits) in &out {
                run_ctors(inits);
            }
            check_all_zones("load_tree.done");
            Ok(out.into_iter().map(|(l, _)| l).collect())
        }
        Err(e) => {
            // Unwind the failed tree like bionic's failed dlopen:
            // no ctors ran yet, so nothing outside `out` can have
            // bound to these exports. Unmark the sonames so a later
            // load re-maps fresh copies, then drop — NativeLib::drop
            // scrubs each lib's exports from GLOBAL_SYMS before
            // unmapping, so no dangling addresses survive.
            {
                let mut names = LOADED_SONAMES.lock().unwrap();
                for (l, _) in &out {
                    names.remove(l.name.rsplit('/').next().unwrap_or(&l.name));
                }
            }
            drop(out);
            Err(e)
        }
    }
}

fn load_tree_into(
    name: &str,
    elf: &[u8],
    lookup: &mut dyn FnMut(&str) -> Option<Vec<u8>>,
    out: &mut Vec<(NativeLib, Vec<usize>)>,
    depth: usize,
) -> Result<()> {
    if depth > 16 {
        return Err(err("elf.depdepth"));
    }
    let base = name.rsplit('/').next().unwrap_or(name).to_string();
    if !LOADED_SONAMES.lock().unwrap().insert(base.clone()) {
        // Already loaded, or a dep edge circling back to a lib still
        // loading — its exports are already in GLOBAL_SYMS either way.
        return Ok(());
    }
    let r = (|| {
        // Map + publish this lib's exports BEFORE its deps relocate:
        // a dep that references its dependent's symbols (plugin-style
        // callbacks into the requester) binds them — bionic adds the
        // image to the link map's global scope before relocating deps.
        let mapped = map_so(elf)?;
        {
            let mut g = GLOBAL_SYMS.lock().unwrap();
            for (n, a) in &mapped.exports {
                g.entry(n.clone()).or_default().push(*a);
            }
        }
        for soname in elf_needed(elf) {
            let dep = soname.rsplit('/').next().unwrap_or(&soname).to_string();
            // A bundled bionic would collide with the host/shim libc —
            // its symbols resolve via builtin_shim/host_dlsym anyway.
            if matches!(
                dep.as_str(),
                "libc.so" | "libm.so" | "libdl.so" | "libstdc++.so"
            ) {
                continue;
            }
            if LOADED_SONAMES.lock().unwrap().contains(&dep) {
                continue;
            }
            if let Some(bytes) = lookup(&dep) {
                if let Err(e) = load_tree_into(&dep, &bytes, lookup, out, depth + 1) {
                    eprintln!("[jni] dep {dep}: {e}");
                }
            }
        }
        // Exports were published at map time; a failed finish must
        // take them back — the image stays mapped but unfinished, so
        // binding to its symbols would jump into an unrelocated body.
        let img = (mapped.map as usize, mapped.map as usize + mapped.span);
        match finish_so(mapped, name) {
            Ok(x) => out.push(x),
            Err(e) => {
                globals_scrub(&mut GLOBAL_SYMS.lock().unwrap(), img.0, img.1);
                return Err(e);
            }
        }
        Ok(())
    })();
    if r.is_err() {
        LOADED_SONAMES.lock().unwrap().remove(&base);
    }
    r
}

/// An image copied into memory with its dynamic info parsed and
/// exports computed — relocations and initializers not yet applied.
/// Deliberately has no Drop: once a lib's exports join `GLOBAL_SYMS`
/// its mapping must never go away, so a failed finish leaks the
/// mapping rather than leaving dangling symbol addresses behind.
struct MappedLib {
    map: *mut u8,
    span: usize,
    bias: usize,
    lo: u64,
    d: Dyn,
    segs: Vec<(usize, usize, u32)>,
    relro: Option<(u64, u64)>,
    exports: HashMap<String, usize>,
    /// Raw program-header bytes (for `dl_iterate_phdr`) + count.
    phdrs: Vec<u8>,
    phnum: usize,
}

/// Parse + map an ELF64 ET_DYN aarch64 image: mmap, copy PT_LOADs,
/// parse the dynamic table, compute the export map. Relocations and
/// initializers happen in `finish_so` — split so a dependent's
/// exports can publish before its deps relocate (dep→dependent edges).
fn map_so(elf: &[u8]) -> Result<MappedLib> {
    install_crash_dump();
    if elf.len() < 64 || &elf[0..4] != b"\x7fELF" || elf[4] != 2 || elf[5] != 1 {
        return Err(err("elf.bad"));
    }
    if rd16(elf, 16) != 3 || rd16(elf, 18) != 183 {
        return Err(errd("elf.abi", "need ET_DYN EM_AARCH64"));
    }
    let phoff = rd64(elf, 32) as usize;
    let phentsize = rd16(elf, 54) as usize;
    let phnum = rd16(elf, 56) as usize;

    // Span over PT_LOAD (page-aligned).
    let mut lo = u64::MAX;
    let mut hi = 0u64;
    let mut dyn_off = 0usize;
    let mut relro: Option<(u64, u64)> = None;
    for i in 0..phnum {
        let p = phoff + i * phentsize;
        if p + 56 > elf.len() {
            break;
        }
        match rd32(elf, p) {
            PT_LOAD => {
                let va = rd64(elf, p + 16);
                lo = lo.min(va & !(PAGE as u64 - 1));
                hi = hi.max(va + rd64(elf, p + 40));
            }
            PT_DYNAMIC => dyn_off = rd64(elf, p + 8) as usize,
            // PT_GNU_RELRO: region mprotect'd read-only after
            // relocation on Android too — honoring it also splits the
            // RWX boundary page macOS would reject.
            0x6474_e552 => {
                relro = Some((rd64(elf, p + 16), rd64(elf, p + 40)));
            }
            _ => {}
        }
    }
    if lo == u64::MAX {
        return Err(err("elf.noload"));
    }
    let span = ((hi - lo) as usize + PAGE - 1) & !(PAGE - 1);
    let map = unsafe {
        mmap(
            std::ptr::null_mut(),
            span,
            // RWX mmap is denied even for unsigned arm64 binaries —
            // map RW, relocate, then promote to RX before running code.
            PROT_READ | PROT_WRITE,
            MAP_PRIVATE | MAP_ANON,
            -1,
            0,
        )
    };
    if map == MAP_FAILED {
        let e = std::io::Error::last_os_error();
        return Err(errd("elf.mmap", format!("span={span:#x} lo={lo:#x}: {e}")));
    }
    let bias = map as usize - lo as usize;

    // Copy PT_LOAD segments, remembering each one's declared
    // permissions for the post-relocation mprotect. Anything from the
    // APK is validated against the mapping before a raw pointer sees it.
    let map_end = map as usize + span;
    let mut segs: Vec<(usize, usize, u32)> = Vec::new();
    for i in 0..phnum {
        let p = phoff + i * phentsize;
        if rd32(elf, p) != PT_LOAD {
            continue;
        }
        let flags = rd32(elf, p + 4);
        let off = rd64(elf, p + 8) as usize;
        let va = rd64(elf, p + 16) as usize;
        let filesz = rd64(elf, p + 32) as usize;
        let memsz = rd64(elf, p + 40) as usize;
        if off + filesz > elf.len()
            || filesz > memsz
            || va < lo as usize
            || bias + va + memsz > map_end
        {
            unsafe { munmap(map, span) };
            return Err(err("elf.seg"));
        }
        segs.push((va, memsz, flags));
        unsafe {
            std::ptr::copy_nonoverlapping(elf.as_ptr().add(off), (bias + va) as *mut u8, filesz);
            if memsz > filesz {
                std::ptr::write_bytes((bias + va + filesz) as *mut u8, 0, memsz - filesz);
            }
        }
    }
    // Dynamic table.
    let mut d = Dyn {
        strtab: 0,
        symtab: 0,
        strsz: 0,
        rela: 0,
        relasz: 0,
        jmprel: 0,
        pltrelsz: 0,
        init: 0,
        init_array: 0,
        init_array_sz: 0,
        hash: 0,
        gnu_hash: 0,
        relr: 0,
        relrsz: 0,
        android_rel: 0,
        android_relsz: 0,
        android_rela: 0,
        android_relasz: 0,
        needed: Vec::new(),
    };
    let mut off = dyn_off;
    loop {
        if off + 16 > elf.len() {
            break;
        }
        let tag = rd64(elf, off) as i64;
        let val = rd64(elf, off + 8) as usize;
        match tag {
            DT_NULL => break,
            DT_STRTAB => d.strtab = bias + val,
            DT_SYMTAB => d.symtab = bias + val,
            DT_STRSZ => d.strsz = val,
            DT_RELA | DT_ANDROID_RELA => {
                if tag == DT_RELA {
                    d.rela = bias + val;
                } else {
                    d.android_rela = bias + val;
                }
            }
            DT_RELASZ | DT_ANDROID_RELASZ => {
                if tag == DT_RELASZ {
                    d.relasz = val;
                } else {
                    d.android_relasz = val;
                }
            }
            DT_JMPREL => d.jmprel = bias + val,
            DT_PLTRELSZ => d.pltrelsz = val,
            DT_INIT => d.init = bias + val,
            DT_INIT_ARRAY => d.init_array = bias + val,
            DT_INIT_ARRAYSZ => d.init_array_sz = val,
            DT_HASH => d.hash = bias + val,
            DT_GNU_HASH => d.gnu_hash = bias + val,
            DT_RELR | DT_ANDROID_REL => {
                if tag == DT_RELR {
                    d.relr = bias + val;
                } else {
                    d.android_rel = bias + val;
                }
            }
            DT_RELRSZ | DT_ANDROID_RELSZ => {
                if tag == DT_RELRSZ {
                    d.relrsz = val;
                } else {
                    d.android_relsz = val;
                }
            }
            DT_NEEDED => d.needed.push(val),
            _ => {}
        }
        off += 16;
    }
    // DT_PLTGOT referenced but not tracked (GOT entries relocate via RELA).
    let _ = DT_PLTGOT;

    // Symbol count: DT_HASH nchain, else walk DT_GNU_HASH chains.
    let sym_count = if d.hash != 0 {
        unsafe { ((d.hash + 4) as *const u32).read() as usize }
    } else if d.gnu_hash != 0 {
        gnu_sym_count(d.gnu_hash)
    } else if d.symtab != 0 && d.strtab > d.symtab {
        (d.strtab - d.symtab) / 24
    } else {
        0
    };
    let sym_name = |i: usize| -> String {
        let so = unsafe { (d.symtab as *const u8).add(i * 24) };
        let name_off = unsafe { (so as *const u32).read() };
        cstr_at(bias, d.strtab, name_off)
    };
    let sym_shndx = |i: usize| -> u16 { unsafe { ((d.symtab + i * 24 + 6) as *const u16).read() } };
    let sym_value =
        |i: usize| -> usize { unsafe { ((d.symtab + i * 24 + 8) as *const u64).read() as usize } };
    // Export map (defined GLOBAL/WEAK/DEFAULT symbols).
    let mut exports: HashMap<String, usize> = HashMap::new();
    for i in 0..sym_count {
        if sym_shndx(i) == SHN_UNDEF {
            continue;
        }
        exports.insert(sym_name(i), bias + sym_value(i));
    }

    let phdrs = elf
        .get(phoff..phoff + phentsize * phnum)
        .map(|r| r.to_vec())
        .unwrap_or_default();

    Ok(MappedLib {
        map: map as *mut u8,
        span,
        bias,
        lo,
        d,
        segs,
        relro,
        exports,
        phdrs,
        phnum,
    })
}

/// Relocate + protect a mapped image, wrap it as a `NativeLib`, and
/// collect its DT_INIT/DT_INIT_ARRAY entry points for `run_ctors`.
/// Ctors are NOT run here: `load_tree` runs every tree member's ctors
/// only after every member has relocated. A lib that fails partway
/// stays mapped: tree loads publish exports at map time (so
/// dep→dependent edges resolve), so unmapping it would leave dangling
/// addresses in `GLOBAL_SYMS`.
fn finish_so(m: MappedLib, name: &str) -> Result<(NativeLib, Vec<usize>)> {
    let map = m.map;
    let span = m.span;
    let bias = m.bias;
    let lo = m.lo;
    let d = &m.d;
    let segs = &m.segs;
    let relro = m.relro;
    let exports = &m.exports;
    let map_end = map as usize + span;
    // A raw guest-vaddr-derived pointer is legal only inside the
    // mapping — relocations and init pointers must all pass this.
    let in_img = |a: usize, n: usize| a >= map as usize && a + n <= map_end;
    let sym_name = |i: usize| -> String {
        let so = unsafe { (d.symtab as *const u8).add(i * 24) };
        let name_off = unsafe { (so as *const u32).read() };
        cstr_at(bias, d.strtab, name_off)
    };
    let sym_bind =
        |i: usize| -> u8 { (unsafe { ((d.symtab + i * 24 + 4) as *const u8).read() }) >> 4 };

    // Import resolution: own exports → shim table → host dlsym.
    let resolve = |i: usize, exports: &HashMap<String, usize>| -> Result<usize> {
        let name = sym_name(i);
        // strip ELF version suffix (`name@LIBC` handled by dynstr already
        // carrying plain name; version refs live in .gnu.version).
        if let Some(&a) = exports.get(&name) {
            return Ok(a);
        }
        // Already-loaded libs (DT_NEEDED deps, earlier loadLibrary
        // calls) — the global scope of the process link map.
        if let Some(a) = GLOBAL_SYMS
            .lock()
            .unwrap()
            .get(&name)
            .and_then(|v| v.first().copied())
        {
            return Ok(a);
        }
        if let Some(a) = builtin_shim(&name) {
            name_target(a, &name);
            return Ok(host_thunk(a));
        }
        if let Some(a) = data_shim(&name) {
            return Ok(a);
        }
        // OVELO_NOHOSTDLSYM: debug bisection — refuse to hand guest code
        // real libSystem entry points, so every import lands on a shim.
        if crate::dbg_flag("OVELO_NOHOSTDLSYM") {
            eprintln!("[dlsym-deny] {name}");
        } else {
            if let Some(a) = host_dlsym(&name) {
                if crate::dbg_flag("OVELO_DLSYMLOG") {
                    eprintln!("[dlsym] {name}");
                }
                // Data symbols must stay raw addresses — wrapping them in
                // a code thunk would have the guest read machine code
                // where the variable's value belongs.
                return Ok(if is_data_sym(&name) {
                    a
                } else {
                    name_target(a, &name);
                    host_thunk(a)
                });
            }
        }
        // Weak undefined symbols resolve to 0 (legal — the callee checks).
        if sym_bind(i) == STB_WEAK {
            return Ok(0);
        }
        Err(errd("elf.unresolved", name.to_string()))
    };

    let apply = |_d: &Dyn,
                 table: usize,
                 bytes: usize,
                 entsize: usize,
                 exports: &HashMap<String, usize>|
     -> Result<()> {
        let n = bytes / entsize;
        for k in 0..n {
            let r = table + k * entsize;
            let (r_off, r_info, r_add) = unsafe {
                (
                    (r as *const u64).read() as usize,
                    ((r as *const u64).add(1)).read(),
                    if entsize == 24 {
                        ((r as *const u64).add(2)).read() as i64
                    } else {
                        0
                    },
                )
            };
            let sym_i = (r_info >> 32) as usize;
            let rtype = (r_info & 0xffff_ffff) as u32;
            let loc = (bias + r_off) as *mut u64;
            if !in_img(loc as usize, 8) {
                return Err(errd("elf.reloc", format!("target oob {r_off:#x}")));
            }
            match rtype {
                R_AARCH64_RELATIVE => unsafe { loc.write((bias as i64 + r_add) as u64) },
                R_AARCH64_ABS64 | R_AARCH64_GLOB_DAT | R_AARCH64_JUMP_SLOT => {
                    let s = resolve(sym_i, exports)? as i64;
                    unsafe { loc.write((s + r_add) as u64) };
                }
                0 => {}
                other => return Err(errd("elf.reloc", format!("type {other}"))),
            }
        }
        Ok(())
    };

    apply(d, d.rela, d.relasz, 24, exports)?;
    apply(d, d.jmprel, d.pltrelsz, 24, exports)?;
    apply(d, d.android_rela, d.android_relasz, 24, exports)?;
    // Android packed REL (REL, no addend: *loc = base + *loc).
    let n_rel = d.android_relsz / 8;
    for k in 0..n_rel {
        let r = d.android_rel + k * 8;
        let (r_off, r_info) = unsafe {
            (
                (r as *const u64).read() as usize,
                ((r as *const u64).add(1)).read(),
            )
        };
        let rtype = (r_info & 0xffff_ffff) as u32;
        let loc = (bias + r_off) as *mut u64;
        if !in_img(loc as usize, 8) {
            return Err(errd("elf.reloc", format!("rel target oob {r_off:#x}")));
        }
        match rtype {
            R_AARCH64_RELATIVE => unsafe {
                let cur = loc.read();
                loc.write(bias as u64 + cur);
            },
            R_AARCH64_ABS64 | R_AARCH64_GLOB_DAT | R_AARCH64_JUMP_SLOT => {
                let s = resolve((r_info >> 32) as usize, exports)?;
                unsafe { loc.write(s as u64) };
            }
            0 => {}
            other => return Err(errd("elf.reloc", format!("rel type {other}"))),
        }
    }
    // RELR packed relocation.
    apply_relr(bias, d.relr, d.relrsz, map as usize, map_end)?;

    // Guest TLS access is left unpatched: `mrs xN,TPIDR_EL0` reads
    // TPIDR_EL0, which already holds the guest TCB while guest code
    // runs (thunks swap it to the host TPIDR for the host-call
    // window). An earlier revision rewrote these to a reserved x18
    // carrier because TPIDR_EL0 was believed to be clobbered at
    // kernel boundaries — measured the other way: signal delivery
    // zeroes x18 and sigreturn does NOT restore it (x18 is dead after
    // the first signal, turning every patched access into a fault),
    // while TPIDR_EL0 survives signal delivery intact.

    // Apply each segment's declared permissions — writable data and
    // bss must stay writable after relocation, executable pages go RX.
    // Two segments can share one 16 KiB OS page at the boundary, so
    // permissions are merged per page first: a boundary page touched
    // by both an RX and an RW segment keeps both bits.
    let n_pages = span / PAGE;
    let mut page_prot = vec![0i32; n_pages];
    for &(va, memsz, flags) in segs {
        if memsz == 0 {
            continue;
        }
        let prot = (if flags & 4 != 0 { PROT_READ } else { 0 })
            | (if flags & 2 != 0 { PROT_WRITE } else { 0 })
            | (if flags & 1 != 0 { PROT_EXEC } else { 0 });
        let first = ((va - lo as usize) & !(PAGE - 1)) / PAGE;
        let last = ((va - lo as usize + memsz + PAGE - 1) & !(PAGE - 1)) / PAGE;
        for p in page_prot.iter_mut().take(last.min(n_pages)).skip(first) {
            *p |= prot;
        }
    }
    // PT_GNU_RELRO: pages fully inside the region lose WRITE (the
    // boundary page where an RX tail meets the RW RELRO head then
    // reads R|X instead of the RWX macOS refuses to grant).
    if let Some((rva, rlen)) = relro {
        let rlo = (rva - lo) as usize;
        let rhi = (rva - lo + rlen) as usize;
        for (i, p) in page_prot.iter_mut().enumerate() {
            let plo = i * PAGE;
            let phi = plo + PAGE;
            if plo >= rlo && phi <= rhi {
                *p &= !PROT_WRITE;
            }
        }
    }
    let mut start = 0;
    while start < n_pages {
        let mut end = start + 1;
        while end < n_pages && page_prot[end] == page_prot[start] {
            end += 1;
        }
        if page_prot[start] != 0 {
            let addr = (map as usize + start * PAGE) as *mut c_void;
            let len = (end - start) * PAGE;
            // macOS never grants RWX on non-JIT anonymous maps. Where
            // a 16 KiB OS page merges an RX tail with an RW head, the
            // writable bytes are the data tail already relocated
            // above — drop WRITE and keep the page executable.
            let prot = if page_prot[start] & (PROT_EXEC | PROT_WRITE) == PROT_EXEC | PROT_WRITE {
                page_prot[start] & !PROT_WRITE
            } else {
                page_prot[start]
            };
            if unsafe { mprotect(addr, len, prot) } != 0 {
                let e = std::io::Error::last_os_error();
                return Err(errd(
                    "elf.mprotect",
                    format!(
                        "{name}: off={:#x} len={:#x} prot={:#x} map={:#x} span={:#x}: {e}",
                        start * PAGE,
                        (end - start) * PAGE,
                        page_prot[start],
                        map as usize,
                        span
                    ),
                ));
            }
        }
        start = end;
    }
    unsafe { icache_flush(map.cast(), span) };
    LOADED_RANGES.lock().unwrap().push((map as usize, span));

    // Initializers: DT_INIT then DT_INIT_ARRAY — both guest-vaddr
    // derived, both checked against the mapping before they can branch
    // anywhere outside it.
    if d.init != 0 && !in_img(d.init, 4) {
        return Err(err("elf.init"));
    }
    // Register with dl_iterate_phdr BEFORE the initializers run —
    // unwind-table registration (e.g. glide-webp's) walks the phdr
    // list during DT_INIT and must find this lib. The phdr copy is
    // leaked so the reported pointer stays valid for the process.
    if let Ok(name_c) = CString::new(name) {
        if !m.phdrs.is_empty() {
            PHDR_LIBS.lock().unwrap().push(Arc::new(PhdrLib {
                bias,
                name: name_c,
                phdrs: m.phdrs.into(),
                phnum: m.phnum.try_into().unwrap_or(u16::MAX),
            }));
        }
    }

    // Collect initializers — run later by `run_ctors` once every lib
    // in the tree is relocated. The INIT_ARRAY slots themselves were
    // just relocated (R_AARCH64_RELATIVE), so read the real addresses.
    let mut inits = Vec::new();
    if d.init != 0 {
        inits.push(d.init);
    }
    let n_init = if in_img(d.init_array, d.init_array_sz) {
        d.init_array_sz / 8
    } else {
        0
    };
    for k in 0..n_init {
        inits.push(unsafe { ((d.init_array + k * 8) as *const u64).read() } as usize);
    }

    // Join the process global scope: later libs (and dep edges of
    // siblings) resolve against these exports. First registration
    // wins — keeps breadth-first precedence when names collide.
    {
        let mut g = GLOBAL_SYMS.lock().unwrap();
        for (n, a) in exports {
            g.entry(n.clone()).or_default().push(*a);
        }
    }

    Ok((
        NativeLib {
            map,
            span,
            bias,
            exports: m.exports,
            registered: HashMap::new(),
            name: name.to_string(),
        },
        inits,
    ))
}

/// DT_INIT then DT_INIT_ARRAY, each under the guest TLS window.
fn run_ctors(inits: &[usize]) {
    for &a in inits {
        if crate::dbg_flag("OVELO_ZONECHECK") {
            eprintln!("[zonecheck] ctor {a:#x}");
        }
        check_all_zones("pre-ctor");
        let f: extern "C" fn() = unsafe { std::mem::transmute(a) };
        with_guest_tls_at("ctors", || f());
        check_all_zones("post-ctor");
    }
}

/// Single-image load: map, relocate, initialize. `load_tree` is the
/// entry point for libs with DT_NEEDED edges.
#[allow(dead_code)]
pub fn load_so(name: &str, elf: &[u8]) -> Result<NativeLib> {
    let (lib, inits) = finish_so(map_so(elf)?, name)?;
    run_ctors(&inits);
    Ok(lib)
}

fn gnu_sym_count(gh: usize) -> usize {
    unsafe {
        let nbuckets = (gh as *const u32).read() as usize;
        let symoffset = ((gh as *const u32).add(1)).read() as usize;
        let bloom_size = ((gh as *const u32).add(2)).read() as usize;
        let buckets = (gh + 16 + bloom_size * 8) as *const u32;
        let mut max = 0usize;
        for b in 0..nbuckets {
            let mut si = buckets.add(b).read() as usize;
            if si == 0 {
                continue;
            }
            let mut chain =
                (gh + 16 + bloom_size * 8 + nbuckets * 4 + (si - symoffset) * 4) as *const u32;
            loop {
                let h = chain.read();
                si += 1;
                chain = chain.add(1);
                if h & 1 == 1 {
                    break;
                }
            }
            max = max.max(si);
        }
        max
    }
}

fn apply_relr(
    bias: usize,
    table: usize,
    bytes: usize,
    map_lo: usize,
    map_end: usize,
) -> Result<()> {
    if table == 0 {
        return Ok(());
    }
    let mut addr = 0usize;
    let mut i = 0;
    while i * 8 < bytes {
        let e = unsafe { ((table + i * 8) as *const u64).read() };
        i += 1;
        if e & 1 == 0 {
            addr = e as usize;
            let loc = (bias + addr) as *mut u64;
            if (loc as usize) < map_lo || (loc as usize) + 8 > map_end {
                return Err(errd("elf.reloc", format!("relr target oob {addr:#x}")));
            }
            unsafe { loc.write(bias as u64 + loc.read()) };
            addr += 8;
        } else {
            let mut bits = e >> 1;
            let mut bit = 0;
            while bits != 0 {
                if bits & 1 == 1 {
                    let off = addr + bit * 8;
                    let loc = (bias + off) as *mut u64;
                    if (loc as usize) < map_lo || (loc as usize) + 8 > map_end {
                        return Err(errd("elf.reloc", format!("relr target oob {off:#x}")));
                    }
                    unsafe { loc.write(bias as u64 + loc.read()) };
                }
                bits >>= 1;
                bit += 1;
            }
            addr += 63 * 8;
        }
    }
    Ok(())
}

impl NativeLib {
    pub fn sym(&self, name: &str) -> Option<usize> {
        self.exports.get(name).copied()
    }
}

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
    #[cfg(target_os = "macos")]
    fn sys_icache_invalidate(start: *mut c_void, len: usize);
    #[cfg(target_os = "macos")]
    fn sigaction(sig: i32, act: *const Sigaction, old: *mut Sigaction) -> i32;
    #[cfg(target_os = "macos")]
    fn _exit(code: i32) -> !;
    #[cfg(target_os = "macos")]
    fn write(fd: i32, buf: *const c_void, n: usize) -> isize;
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
fn guest_tcb() -> *mut u64 {
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
    // Lock-free parallel list the signal handler can scan: tcb stored
    // before tid so a torn read never pairs a tid with a stale tcb.
    for i in (0..TCB_LIST.len()).step_by(2) {
        if TCB_LIST[i + 1].load(std::sync::atomic::Ordering::Acquire) == 0 {
            TCB_LIST[i].store(
                tcb.as_ptr() as usize as u64,
                std::sync::atomic::Ordering::Release,
            );
            TCB_LIST[i + 1].store(tid, std::sync::atomic::Ordering::Release);
            break;
        }
    }
    tcb.as_ptr() as *mut u64
}

/// (tcb, tid) pairs appended at TCB creation; never removed.
#[cfg(target_arch = "aarch64")]
static TCB_LIST: [std::sync::atomic::AtomicU64; 256] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 256];

/// TCB for `tid`, scannable from the async-signal handler.
#[cfg(target_arch = "aarch64")]
fn tcb_for(tid: u64) -> usize {
    for i in (0..TCB_LIST.len()).step_by(2) {
        if TCB_LIST[i + 1].load(std::sync::atomic::Ordering::Acquire) == tid {
            return TCB_LIST[i].load(std::sync::atomic::Ordering::Acquire) as usize;
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
    if std::env::var_os("OVELO_NOTLS").is_some() {
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

        *tcb.add(SLOT_HOST_TPIDR) = old as u64;
        core::arch::asm!("msr tpidr_el0, {}", in(reg) tcb as usize);
        TLS_TRACE[8].store(tcb as usize as u64, std::sync::atomic::Ordering::Relaxed);
        // OVELO_TLSPROBE: allocation inside the swapped window — if the
        // crash is malloc's TPIDR-keyed per-thread magazine state, a
        // plain Vec alloc+drop dies here with the same signature.
        if std::env::var_os("OVELO_TLSPROBE").is_some() {
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
            // Per-thread guest TCB; env thunks on this thread swap to
            // whatever TPIDR the thread started with (host garbage).
            let tcb = guest_tcb();
            let mut old: usize;
            core::arch::asm!("mrs {}, tpidr_el0", out(reg) old);
            *tcb.add(SLOT_HOST_TPIDR) = old as u64;
            core::arch::asm!("msr tpidr_el0, {}", in(reg) tcb as usize);
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
    const SLOT: usize = 160;
    const PAGE: usize = 0x10000;

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
                0xD53B_D05D, // 24: mrs x29, tpidr_el0 — this thread's TCB
                0xF97F_FFB0, // 25: ldr x16, [x29, #0x7ff8] — saved host TPIDR
                0xD51B_D050, // 26: msr tpidr_el0, x16
            ]);
            ins.extend_from_slice(&trace_stores(at, 108, 6, 16)); // 27-28: TRACE[6]=swap val
            ins.extend_from_slice(&[
                0x5800_0130, // 29: ldr x16, +36 -> target literal at byte 152
                0xD63F_0200, // 30: blr x16
                0xD51B_D05D, // 31: msr tpidr_el0, x29 — back to guest TCB
            ]);
            ins.extend_from_slice(&trace_stores(at, 128, 7, 29)); // 32-33: TRACE[7]=restore val
            ins.extend_from_slice(&[
                0xA94A_7BFD, // 34: ldp x29,x30,[sp,#168]
                0x9103_03FF, // 35: add sp, sp, #192
                0xD65F_03C0, // 36: ret
                0xD503_201F, // 37: pad before literal
            ]);
            debug_assert_eq!(ins.len(), 38);
            for (i, ins) in ins.iter().enumerate() {
                w.add(i).write_volatile(*ins);
            }
            // Target literal at byte 152, reached by `ldr x16, +36` from
            // insn 29 (byte 116).
            let lit = (at + 152) as *mut usize;
            lit.write_volatile(target);
            pthread_jit_write_protect_np(1);
            sys_icache_invalidate(at as *mut c_void, SLOT);
        }
        at
    }
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
    if base >= 0x1000 || si != base.wrapping_add(imm) {
        return false;
    }
    // A smashed TPIDR is only a plausible diagnosis when the base
    // register provably held the thread pointer: require an
    // `mrs xN,TPIDR_EL0` to have loaded it, with no clobber and no
    // control flow in between. Otherwise this is a genuine near-null
    // deref and must fault normally — healing a store here would
    // scribble into the guest TCB and keep running corrupted.
    let pc = unsafe { *(mc.add(272) as *const usize) };
    let want_mrs = 0xD53B_D040 | rn as u32;
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
        return false;
    }
    let tcb = tcb_for(unsafe { mach_thread_id() });
    if tcb == 0 {
        return false;
    }
    unsafe {
        *(mc.add(16 + rn * 8) as *mut usize) = tcb;
        for (rb, off) in spills.iter().take(nspill) {
            // mc+248 = saved fp (x29), mc+264 = saved sp.
            let slot = *(mc.add(if *rb == 31 { 264 } else { 248 }) as *const usize)
                .wrapping_add(*off) as *mut usize;
            *slot = tcb;
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
                let insn = *(pc as *const u32);
                if heal_tls_fault(si, insn, mc as *mut u8) {
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
            let sa = Sigaction {
                sa_sigaction: on_segv as *const () as usize,
                sa_mask: 0,
                sa_flags: 0x40, // SA_SIGINFO
            };
            sigaction(11, &sa, std::ptr::null_mut());
            sigaction(10, &sa, std::ptr::null_mut()); // SIGBUS — writes to non-writable guest pages
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
    let p = unsafe { dlsym(RTLD_DEFAULT, c.as_ptr()) };
    (!p.is_null()).then_some(p as usize)
}

/// Bionic/libc++-abi imports libSystem doesn't export under these
/// names. Addresses are filled per-process where they need storage.
extern "C" fn sh_pure_virtual() -> ! {
    eprintln!("[native] __cxa_pure_virtual called");
    std::process::abort()
}
extern "C" fn sh_stack_chk_fail() -> ! {
    eprintln!("[native] __stack_chk_fail");
    std::process::abort()
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
    unsafe { strncpy(d, s, n) }
}
extern "C" fn sh_system_property_get(_name: *const i8, value: *mut i8) -> i32 {
    unsafe { *value = 0 };
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
extern "C" fn sh_memcpy(d: *mut c_void, s: *const c_void, n: usize) -> *mut c_void {
    if !guest_dst_ok(d as usize, n) {
        eprintln!("[guestptr] memcpy dst {d:p}+{n:#x} not guest/stack/image — skipped");
        return d;
    }
    unsafe { memcpy(d, s, n) }
}
#[cfg(target_os = "macos")]
extern "C" fn sh_memmove(d: *mut c_void, s: *const c_void, n: usize) -> *mut c_void {
    if !guest_dst_ok(d as usize, n) {
        eprintln!("[guestptr] memmove dst {d:p}+{n:#x} not guest/stack/image — skipped");
        return d;
    }
    unsafe { memmove(d, s, n) }
}
#[cfg(target_os = "macos")]
extern "C" fn sh_memset(d: *mut c_void, c: i32, n: usize) -> *mut c_void {
    if !guest_dst_ok(d as usize, n) {
        eprintln!("[guestptr] memset dst {d:p}+{n:#x} not guest/stack/image — skipped");
        return d;
    }
    unsafe { memset(d, c, n) }
}
#[cfg(target_os = "macos")]
extern "C" fn sh_strcpy(d: *mut c_void, s: *const c_void) -> *mut c_void {
    unsafe {
        let n = strlen(s as *const i8) + 1;
        if !guest_dst_ok(d as usize, n) {
            eprintln!("[guestptr] strcpy dst {d:p}+{n:#x} not guest/stack/image — skipped");
            return d;
        }
        memcpy(d, s, n)
    }
}
#[cfg(target_os = "macos")]
extern "C" fn sh_strncpy(d: *mut c_void, s: *const c_void, n: usize) -> *mut c_void {
    if !guest_dst_ok(d as usize, n) {
        eprintln!("[guestptr] strncpy dst {d:p}+{n:#x} not guest/stack/image — skipped");
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
    _a3: usize,
    _a4: usize,
    _a5: usize,
    _a6: usize,
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
        eprintln!("[alog:{prio}] {t}: {f}");
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
        p >= top.saturating_sub(size) && p.saturating_add(len) <= top
    }
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
    if std::env::var_os("OVELO_ZONECHECK").is_none() {
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
    let extra = if std::env::var_os("OVELO_FREEDBG").is_some() {
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
    let extra = if std::env::var_os("OVELO_FREEDBG").is_some() {
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
    let extra = if std::env::var_os("OVELO_FREEDBG").is_some() {
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
    if std::env::var_os("OVELO_FREEDBG").is_some() {
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
    let extra = if std::env::var_os("OVELO_FREEDBG").is_some() {
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
        let n = strlen(s) + 1;
        let extra = if std::env::var_os("OVELO_FREEDBG").is_some() {
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
        while len < n && *s.add(len) != 0 {
            len += 1;
        }
        let extra = if std::env::var_os("OVELO_FREEDBG").is_some() {
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
    let extra = if std::env::var_os("OVELO_FREEDBG").is_some() {
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
    let extra = if std::env::var_os("OVELO_FREEDBG").is_some() {
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
    let extra = if std::env::var_os("OVELO_FREEDBG").is_some() {
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
    let extra = if std::env::var_os("OVELO_FREEDBG").is_some() {
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
        "dl_iterate_phdr" => sh_dl_iterate_phdr as *const () as usize,
        "android_set_abort_message" => sh_set_abort_message as *const () as usize,
        "AndroidBitmap_getInfo" => sh_ab_get_info as *const () as usize,
        "AndroidBitmap_lockPixels" => sh_ab_lock_pixels as *const () as usize,
        "AndroidBitmap_unlockPixels" => sh_ab_unlock_pixels as *const () as usize,
        "__android_log_print" | "__android_log_vprint" => sh_log_print as *const () as usize,
        "__android_log_write" => sh_log_write as *const () as usize,
        "__emutls_get_address" => 0, // TLS unimplemented — signal caller
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
            return Ok(host_thunk(a));
        }
        if let Some(a) = data_shim(&name) {
            return Ok(a);
        }
        // OVELO_NOHOSTDLSYM: debug bisection — refuse to hand guest code
        // real libSystem entry points, so every import lands on a shim.
        if std::env::var_os("OVELO_NOHOSTDLSYM").is_some() {
            eprintln!("[dlsym-deny] {name}");
        } else {
            if let Some(a) = host_dlsym(&name) {
                if std::env::var_os("OVELO_DLSYMLOG").is_some() {
                    eprintln!("[dlsym] {name}");
                }
                // Data symbols must stay raw addresses — wrapping them in
                // a code thunk would have the guest read machine code
                // where the variable's value belongs.
                return Ok(if is_data_sym(&name) { a } else { host_thunk(a) });
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
    let mut start = 0;
    while start < n_pages {
        let mut end = start + 1;
        while end < n_pages && page_prot[end] == page_prot[start] {
            end += 1;
        }
        if page_prot[start] != 0
            && unsafe {
                mprotect(
                    (map as usize + start * PAGE) as *mut c_void,
                    (end - start) * PAGE,
                    page_prot[start],
                )
            } != 0
        {
            let e = std::io::Error::last_os_error();
            return Err(errd("elf.mprotect", format!("{e}")));
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
        if std::env::var_os("OVELO_ZONECHECK").is_some() {
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

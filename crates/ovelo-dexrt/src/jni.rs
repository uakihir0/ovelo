//! Real JNI bridge: a `JNIEnv` function table of real code pointers the
//! guest `.so` calls back into the VM with, `RegisterNatives` capture, and
//! a small asm trampoline that calls native functions with AAPCS64 arg
//! placement (x0-x7 integer, d0-d7 float).

// Inherently unsafe: raw pointer interop with guest native code and
// read-only-after-init static function tables shared with it.
#![allow(unsafe_code, static_mut_refs)]
//!
//! Handles crossing the boundary:
//! * `jobject`/`jclass`/`jstring`/`jarray`/`jthrowable` = heap handle (u32)
//!   widened to a pointer-sized value; 0 is null.
//! * `jmethodID`/`jfieldID` = index into `Vm.jni_mids`/`jni_fids` + 1.
//! * Java calls into native: dex `native` method → symbol lookup →
//!   `jni_call` trampoline.

use std::ffi::c_void;

use crate::jdk;
use crate::proto_sig;
use crate::value::{HostState, Obj, V};
use crate::{gl, GIdx, Rt, Vm};

// Guest `.so`s can spawn real pthreads (pthread_create resolves to
// libsystem); JNI entry points reached from those threads have no
// thread-local binding, so they read the last Vm bound anywhere.
// No thread_local here: while `with_guest_tls` swaps TPIDR_EL0 to the
// guest TCB, every #[thread_local] read resolves into *guest* memory —
// a nonzero CUR_VM there would build `&mut Vm` at a wild address.
static VM_DEPTH: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
static FALLBACK_VM: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

// All Vm access from JNI entry points serializes on a recursive mutex:
// a guest thread that AttachCurrentThread's and calls FindClass/etc.
// would otherwise hold a second live &mut Vm and corrupt the heap
// (observed as allocator faults during Vec/String growth).
// Recursive because call_native re-enters JNI on the same thread.
static VM_MTX: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static MTX_ONCE: std::sync::Once = std::sync::Once::new();

extern "C" {
    fn pthread_mutex_init(m: *mut c_void, attr: *const c_void) -> i32;
    fn pthread_mutex_lock(m: *mut c_void) -> i32;
    fn pthread_mutex_unlock(m: *mut c_void) -> i32;
    fn pthread_mutexattr_init(a: *mut c_void) -> i32;
    fn pthread_mutexattr_settype(a: *mut c_void, t: i32) -> i32;
}

// pthread_mutex_t is 64 bytes on macOS; 128 leaves room for other Unix.
// The backing store must be aligned: glibc's lock path issues futex()
// on a word inside the mutex and aborts with __libc_fatal if the
// address is unaligned — a bare [u8] only promises 1-byte alignment.
#[repr(align(16))]
struct Aligned<const N: usize>([u8; N]);
static mut MUTEX_STORAGE: Aligned<128> = Aligned([0; 128]);
static mut MUTEX_ATTR_STORAGE: Aligned<128> = Aligned([0; 128]);

// PTHREAD_MUTEX_RECURSIVE is 2 on macOS but 1 on glibc/Bionic
// (where 2 is PTHREAD_MUTEX_ERRORCHECK — relocking would fail with
// EDEADLK and Drop would then release a lock this guard never took).
#[cfg(target_os = "macos")]
const MUTEX_TYPE_RECURSIVE: i32 = 2;
#[cfg(not(target_os = "macos"))]
const MUTEX_TYPE_RECURSIVE: i32 = 1;

fn vm_mtx() -> *mut c_void {
    MTX_ONCE.call_once(|| unsafe {
        let a = std::ptr::addr_of_mut!(MUTEX_ATTR_STORAGE) as *mut c_void;
        pthread_mutexattr_init(a);
        pthread_mutexattr_settype(a, MUTEX_TYPE_RECURSIVE);
        let m = std::ptr::addr_of_mut!(MUTEX_STORAGE) as *mut c_void;
        pthread_mutex_init(m, a);
        VM_MTX.store(m as usize, std::sync::atomic::Ordering::Release);
    });
    VM_MTX.load(std::sync::atomic::Ordering::Acquire) as *mut c_void
}

/// One held level of the VM recursive mutex. The interpreter itself
/// takes this (every `invoke`), so the lock is what makes the JNI-side
/// serialization real: a guest-pthread JNI call can only touch the Vm
/// while the interpreter is inside `call_native` — where
/// `without_vm_lock` releases all levels — or between dispatches.
pub(crate) struct VmLock;
impl Drop for VmLock {
    fn drop(&mut self) {
        VM_DEPTH.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        unsafe {
            pthread_mutex_unlock(vm_mtx());
        }
    }
}
pub(crate) fn vm_lock() -> VmLock {
    unsafe {
        if pthread_mutex_lock(vm_mtx()) != 0 {
            // Continuing without the lock is UB; aborting is safer.
            std::process::abort();
        }
    }
    VM_DEPTH.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    VmLock
}

/// Release every level of the VM lock this thread holds while `f`
/// runs, then re-acquire them. `call_native` uses this: the
/// interpreter cannot touch `Vm` while blocked inside guest machine
/// code, and threads the `.so` spawns need JNI — and so the lock —
/// during exactly that window.
#[cfg_attr(not(target_arch = "aarch64"), allow(dead_code))]
pub(crate) fn without_vm_lock<R>(f: impl FnOnce() -> R) -> R {
    // VM_DEPTH is a process-global counter, not per-thread — sound
    // because the recursive pthread mutex serializes holders anyway:
    // only one thread can have a nonzero depth at a time.
    let n = VM_DEPTH.swap(0, std::sync::atomic::Ordering::Relaxed);
    let m = vm_mtx();
    for _ in 0..n {
        unsafe {
            pthread_mutex_unlock(m);
        }
    }
    let r = f();
    for _ in 0..n {
        unsafe {
            if pthread_mutex_lock(m) != 0 {
                std::process::abort();
            }
        }
    }
    VM_DEPTH.fetch_add(n, std::sync::atomic::Ordering::Relaxed);
    r
}

/// Guard holding the VM lock; derefs to `&mut Vm` so every existing
/// `vm().heap…` call site works unchanged.
pub(crate) struct VmGuard {
    vm: &'static mut Vm,
    _lock: VmLock,
}
impl std::ops::Deref for VmGuard {
    type Target = Vm;
    fn deref(&self) -> &Vm {
        self.vm
    }
}
impl std::ops::DerefMut for VmGuard {
    fn deref_mut(&mut self) -> &mut Vm {
        self.vm
    }
}

// Track the last env-fn entry (set by `env_enter`); used to name the
// caller whose preceding work corrupted the malloc zone.
// Mutex not thread_local: env fns run under the guest TCB where
// thread_local storage resolves to guest memory.
static LAST_ENV: std::sync::Mutex<&'static str> = std::sync::Mutex::new("?");
fn last_env_get() -> &'static str {
    LAST_ENV.lock().map(|g| *g).unwrap_or("?")
}
fn last_env_set(name: &'static str) {
    if let Ok(mut g) = LAST_ENV.lock() {
        *g = name;
    }
}

/// Env-fn entry hook: `env_enter("FindClass")` at the top of each fn.
/// Under OVELO_ZONECHECK it verifies the default malloc zone every
/// entry — the LAST_ENV name then identifies whichever env call ran
/// just before the corruption was first observed.
pub(crate) fn env_enter(name: &'static str) {
    if std::env::var_os("OVELO_ENVCALL").is_some() {
        eprintln!("[env>] {name}");
    }
    // malloc_zone_check is a Darwin-only API; keep the whole probe macOS-gated
    // so the Linux build neither links nor runs it.
    #[cfg(target_os = "macos")]
    if std::env::var_os("OVELO_ZONECHECK").is_some() {
        unsafe {
            extern "C" {
                fn malloc_default_zone() -> *mut std::ffi::c_void;
                fn malloc_zone_check(zone: *mut std::ffi::c_void) -> bool;
                fn malloc_get_all_zones(
                    task: u32,
                    reader: usize,
                    zones: *mut *mut std::ffi::c_void,
                    count: *mut u32,
                ) -> i32;
                fn mach_task_self() -> u32;
            }
            // Check EVERY zone the process knows about — Rust's own
            // allocations live in a private zone that neither
            // malloc_default_zone nor guest_zone names.
            let mut zs: *mut std::ffi::c_void = std::ptr::null_mut();
            let mut n: u32 = 0;
            let nr = malloc_get_all_zones(mach_task_self(), 0, &mut zs, &mut n);
            if nr != 0 {
                eprintln!("[zonecheck] malloc_get_all_zones -> {nr}");
            }
            for i in 0..n as usize {
                let z = *(zs as *mut *mut std::ffi::c_void).add(i);
                if !malloc_zone_check(z) {
                    eprintln!(
                        "[zonecheck] BAD ZONE {z:?} on entry to {name}; previous env call: {}",
                        last_env_get()
                    );
                    std::process::abort();
                }
            }
            let _ = malloc_default_zone;
        }
    }
    check_buf_canaries(name);
    #[cfg(target_os = "macos")]
    crate::native::dbg_check_all(name);
    last_env_set(name);
}

pub(crate) fn vm() -> VmGuard {
    let lock = vm_lock();
    // FALLBACK_VM only: under the guest-TLS window any thread_local
    // read is a read of guest-controlled memory.
    let p = FALLBACK_VM.load(std::sync::atomic::Ordering::Acquire);
    VmGuard {
        vm: unsafe { &mut *(p as *mut Vm) },
        _lock: lock,
    }
}

/// Run `f` with the JNIEnv bound to `vm` — every native call holds this
/// for the duration so env functions can reach the heap.
pub(crate) fn with_vm<R>(vm: &mut Vm, f: impl FnOnce(&mut Vm) -> R) -> R {
    FALLBACK_VM.store(vm as *mut Vm as usize, std::sync::atomic::Ordering::Release);
    f(vm)
}

// ---------- trampoline ----------

#[cfg(target_arch = "aarch64")]
core::arch::global_asm!(
    ".global _ovelo_jni_call",
    ".p2align 2",
    "_ovelo_jni_call:",
    // x0=fnptr, x1=iargs[8], x2=fargs[8], x3=out[2],
    // x4=sargs[n] (AAPCS64 stack args, 8B slots), x5=n.
    // x19-x21 keep `out`/sargs/n across the call (callee-saved); lr
    // is clobbered by blr so it rides the stack beside them.
    "stp x19, x20, [sp, #-32]!",
    "stp x21, lr, [sp, #16]",
    "mov x19, x3",
    "mov x20, x4",
    "mov x21, x5",
    // Stack-arg area: round_up(n*8, 16) to keep sp 16-aligned.
    "lsl x9, x5, #3",
    "add x9, x9, #15",
    "and x9, x9, #-16",
    "sub sp, sp, x9",
    // Copy surplus args onto the stack in order.
    "mov x10, #0",
    "1:",
    "cmp x10, x21",
    "b.hs 2f",
    "ldr x11, [x20, x10, lsl #3]",
    "str x11, [sp, x10, lsl #3]",
    "add x10, x10, #1",
    "b 1b",
    "2:",
    "mov x9, x0",
    "ldp d0, d1, [x2]",
    "ldp d2, d3, [x2, #16]",
    "ldp d4, d5, [x2, #32]",
    "ldp d6, d7, [x2, #48]",
    "ldp x2, x3, [x1, #16]",
    "ldp x4, x5, [x1, #32]",
    "ldp x6, x7, [x1, #48]",
    "ldp x0, x1, [x1]",
    "blr x9",
    "str x0, [x19]",
    "str d0, [x19, #8]",
    // Pop the stack-arg area and restore.
    "lsl x9, x21, #3",
    "add x9, x9, #15",
    "and x9, x9, #-16",
    "add sp, sp, x9",
    "ldp x21, lr, [sp, #16]",
    "ldp x19, x20, [sp], #32",
    "ret",
);

#[cfg(target_arch = "aarch64")]
extern "C" {
    /// fnptr(iargs as x0-x7, fargs as d0-d7, sargs onto the stack)
    /// → out[0]=x0, out[1]=d0 bits.
    fn ovelo_jni_call(
        fnptr: usize,
        iargs: *const u64,
        fargs: *const f64,
        out: *mut u64,
        sargs: *const u64,
        snargs: u64,
    );
}

/// Call a native function: int-class args in `iargs` (≤8), float-class
/// args in `fargs` (≤8), surplus args (either class, in AAPCS64 order)
/// as 8-byte slots in `sargs`. Returns (x0, d0's bit pattern).
#[cfg(target_arch = "aarch64")]
pub(crate) fn call_native(
    fnptr: usize,
    iargs: &[u64; 8],
    fargs: &[f64; 8],
    sargs: &[u64],
) -> (u64, u64) {
    let mut out = [0u64; 2];
    if std::env::var_os("OVELO_ENVCALL").is_some() {
        if let Some(m) = ENV_NAMES.get() {
            let name = m.get(&fnptr).copied().unwrap_or("?");
            eprintln!("[envcall] {name}({iargs:p})");
        }
    }
    let ncall = std::env::var_os("OVELO_NCALL").is_some();
    if ncall {
        eprintln!(
            "[ncall] -> {fnptr:#x} off={:#x?}",
            crate::native::so_off(fnptr)
        );
    }
    without_vm_lock(|| {
        crate::native::with_guest_tls_at("jni_call", || unsafe {
            // Publish our thread's mach port so the watchdog can sample
            // this thread's pc at high rate while guest code runs.
            #[cfg(target_os = "macos")]
            {
                extern "C" {
                    fn mach_thread_self() -> u32;
                }
                crate::native::excport::JNI_ACT_PORT
                    .store(mach_thread_self(), std::sync::atomic::Ordering::Release);
                crate::native::excport::JNI_ACT_TCB.store(
                    crate::native::guest_tcb() as usize,
                    std::sync::atomic::Ordering::Release,
                );
            }
            ovelo_jni_call(
                fnptr,
                iargs.as_ptr(),
                fargs.as_ptr(),
                out.as_mut_ptr(),
                sargs.as_ptr(),
                sargs.len() as u64,
            );
            #[cfg(target_os = "macos")]
            crate::native::excport::JNI_ACT_PORT.store(0, std::sync::atomic::Ordering::Release);
        });
    });
    if ncall {
        eprintln!("[ncall] <- {fnptr:#x} ok");
    }
    (out[0], out[1])
}

/// Non-aarch64 host: no trampoline exists — guest native code cannot
/// run anyway (all guest `.so`s are aarch64).
#[cfg(not(target_arch = "aarch64"))]
pub(crate) fn call_native(
    _fnptr: usize,
    _iargs: &[u64; 8],
    _fargs: &[f64; 8],
    _sargs: &[u64],
) -> (u64, u64) {
    (0, 0)
}

// ---------- env/vm tables ----------

static mut JNI_TABLE: [u64; 240] = [0; 240];
static mut JAVAVM_TABLE: [u64; 8] = [0; 8];
/// fnptr → env-fn name for OVELO_ENVCALL tracing.
static ENV_NAMES: std::sync::OnceLock<std::collections::HashMap<usize, &'static str>> =
    std::sync::OnceLock::new();
// JNIEnv*/JavaVM* are one more level of indirection: the guest
// dereferences the handle to get the function table address, so the
// handle must point at a cell HOLDING the table pointer, not at the
// table itself.
static mut JNI_ENV: usize = 0;
static mut JAVA_VM: usize = 0;

pub(crate) fn env_ptr() -> usize {
    std::ptr::addr_of!(JNI_ENV) as usize
}
pub(crate) fn vm_ptr() -> usize {
    std::ptr::addr_of!(JAVA_VM) as usize
}

extern "C" fn unimpl() -> u64 {
    // Read the guest return address before env_enter's own call
    // clobbers `lr`, so we can name the call site in the `.so`.
    #[cfg(target_arch = "aarch64")]
    let caller = {
        let lr: usize;
        unsafe { std::arch::asm!("mov {}, lr", out(reg) lr) };
        lr
    };
    #[cfg(not(target_arch = "aarch64"))]
    let caller = 0usize;
    env_enter("unimpl");
    eprintln!(
        "[jni] unimplemented env fn caller={caller:#x} so_off={:?}",
        crate::native::so_off(caller)
    );
    0
}

macro_rules! ef {
    ($f:expr) => {
        $f as usize as u64
    };
}

/// Fill both tables once at first use.
pub(crate) fn init_tables() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| unsafe {
        JNI_ENV = JNI_TABLE.as_ptr() as usize;
        JAVA_VM = JAVAVM_TABLE.as_ptr() as usize;
        for s in JNI_TABLE.iter_mut() {
            *s = ef!(unimpl as *const ());
        }
        let t = &mut JNI_TABLE;
        t[4] = ef!(get_version as *const ());
        t[6] = ef!(find_class as *const ());
        t[13] = ef!(throw as *const ());
        t[14] = ef!(throw_new as *const ());
        t[15] = ef!(exception_occurred as *const ());
        t[16] = ef!(exception_describe as *const ());
        t[17] = ef!(exception_clear as *const ());
        t[19] = ef!(ret0 as *const ());
        t[20] = ef!(pop_local_frame as *const ());
        t[21] = ef!(new_ref as *const ());
        t[22] = ef!(ret0 as *const ());
        t[23] = ef!(ret0 as *const ());
        t[24] = ef!(is_same_object as *const ());
        t[25] = ef!(new_ref as *const ());
        t[26] = ef!(ret0 as *const ());
        t[27] = ef!(alloc_object as *const ());
        t[31] = ef!(get_object_class as *const ());
        t[32] = ef!(is_instance_of as *const ());
        t[33] = ef!(get_method_id as *const ());
        t[36] = ef!(call_object_method_a as *const ());
        t[39] = ef!(call_boolean_method_a as *const ());
        t[42] = ef!(call_byte_method_a as *const ());
        t[45] = ef!(call_char_method_a as *const ());
        t[48] = ef!(call_short_method_a as *const ());
        t[51] = ef!(call_int_method_a as *const ());
        t[54] = ef!(call_long_method_a as *const ());
        t[57] = ef!(call_float_method_a as *const ());
        t[60] = ef!(call_double_method_a as *const ());
        t[63] = ef!(call_void_method_a as *const ());
        t[94] = ef!(get_field_id as *const ());
        t[95] = ef!(get_object_field as *const ());
        t[96] = ef!(get_boolean_field as *const ());
        t[97] = ef!(get_byte_field as *const ());
        t[98] = ef!(get_char_field as *const ());
        t[99] = ef!(get_short_field as *const ());
        t[100] = ef!(get_int_field as *const ());
        t[101] = ef!(get_long_field as *const ());
        t[102] = ef!(get_float_field as *const ());
        t[103] = ef!(get_double_field as *const ());
        t[104] = ef!(set_object_field as *const ());
        t[105] = ef!(set_boolean_field as *const ());
        t[106] = ef!(set_byte_field as *const ());
        t[107] = ef!(set_char_field as *const ());
        t[108] = ef!(set_short_field as *const ());
        t[109] = ef!(set_int_field as *const ());
        t[110] = ef!(set_long_field as *const ());
        t[111] = ef!(set_float_field as *const ());
        t[112] = ef!(set_double_field as *const ());
        t[113] = ef!(get_static_method_id as *const ());
        t[116] = ef!(call_static_object_method_a as *const ());
        t[119] = ef!(call_static_boolean_method_a as *const ());
        t[122] = ef!(call_static_byte_method_a as *const ());
        t[125] = ef!(call_static_char_method_a as *const ());
        t[128] = ef!(call_static_short_method_a as *const ());
        t[131] = ef!(call_static_int_method_a as *const ());
        t[134] = ef!(call_static_long_method_a as *const ());
        t[137] = ef!(call_static_float_method_a as *const ());
        t[140] = ef!(call_static_double_method_a as *const ());
        t[143] = ef!(call_static_void_method_a as *const ());
        t[144] = ef!(get_static_field_id as *const ());
        t[145] = ef!(get_static_object_field as *const ());
        t[146] = ef!(get_static_boolean_field as *const ());
        t[147] = ef!(get_static_byte_field as *const ());
        t[148] = ef!(get_static_char_field as *const ());
        t[149] = ef!(get_static_short_field as *const ());
        t[150] = ef!(get_static_int_field as *const ());
        t[151] = ef!(get_static_long_field as *const ());
        t[152] = ef!(get_static_float_field as *const ());
        t[153] = ef!(get_static_double_field as *const ());
        t[154] = ef!(set_static_object_field as *const ());
        t[155] = ef!(set_static_boolean_field as *const ());
        t[156] = ef!(set_static_byte_field as *const ());
        t[157] = ef!(set_static_char_field as *const ());
        t[158] = ef!(set_static_short_field as *const ());
        t[159] = ef!(set_static_int_field as *const ());
        t[160] = ef!(set_static_long_field as *const ());
        t[161] = ef!(set_static_float_field as *const ());
        t[162] = ef!(set_static_double_field as *const ());
        t[163] = ef!(new_string as *const ());
        t[164] = ef!(get_string_length as *const ());
        t[165] = ef!(get_string_units as *const ());
        t[166] = ef!(release_string_units as *const ());
        t[167] = ef!(new_string_utf as *const ());
        t[168] = ef!(get_string_utf_length as *const ());
        t[169] = ef!(get_string_utf_chars as *const ());
        t[170] = ef!(release_string_utf_chars as *const ());
        t[171] = ef!(get_array_length as *const ());
        t[172] = ef!(new_object_array as *const ());
        t[173] = ef!(get_object_array_element as *const ());
        t[174] = ef!(set_object_array_element as *const ());
        t[175] = ef!(new_boolean_array as *const ());
        t[176] = ef!(new_byte_array as *const ());
        t[177] = ef!(new_char_array as *const ());
        t[178] = ef!(new_short_array as *const ());
        t[179] = ef!(new_int_array as *const ());
        t[180] = ef!(new_long_array as *const ());
        t[181] = ef!(new_float_array as *const ());
        t[182] = ef!(new_double_array as *const ());
        t[183] = ef!(get_boolean_array_elements as *const ());
        t[184] = ef!(get_byte_array_elements as *const ());
        t[185] = ef!(get_char_array_elements as *const ());
        t[186] = ef!(get_short_array_elements as *const ());
        t[187] = ef!(get_int_array_elements as *const ());
        t[188] = ef!(get_long_array_elements as *const ());
        t[189] = ef!(get_float_array_elements as *const ());
        t[190] = ef!(get_double_array_elements as *const ());
        t[191] = ef!(release_boolean_array_elements as *const ());
        t[192] = ef!(release_byte_array_elements as *const ());
        t[193] = ef!(release_char_array_elements as *const ());
        t[194] = ef!(release_short_array_elements as *const ());
        t[195] = ef!(release_int_array_elements as *const ());
        t[196] = ef!(release_long_array_elements as *const ());
        t[197] = ef!(release_float_array_elements as *const ());
        t[198] = ef!(release_double_array_elements as *const ());
        t[199] = ef!(get_boolean_array_region as *const ());
        t[200] = ef!(get_byte_array_region as *const ());
        t[201] = ef!(get_char_array_region as *const ());
        t[202] = ef!(get_short_array_region as *const ());
        t[203] = ef!(get_int_array_region as *const ());
        t[204] = ef!(get_long_array_region as *const ());
        t[205] = ef!(get_float_array_region as *const ());
        t[206] = ef!(get_double_array_region as *const ());
        t[207] = ef!(set_boolean_array_region as *const ());
        t[208] = ef!(set_byte_array_region as *const ());
        t[209] = ef!(set_char_array_region as *const ());
        t[210] = ef!(set_short_array_region as *const ());
        t[211] = ef!(set_int_array_region as *const ());
        t[212] = ef!(set_long_array_region as *const ());
        t[213] = ef!(set_float_array_region as *const ());
        t[214] = ef!(set_double_array_region as *const ());
        t[215] = ef!(register_natives as *const ());
        t[216] = ef!(ret0 as *const ());
        t[217] = ef!(ret0 as *const ());
        t[218] = ef!(ret0 as *const ());
        t[219] = ef!(get_java_vm as *const ());
        t[220] = ef!(get_string_region as *const ());
        t[221] = ef!(get_string_utf_region as *const ());
        t[222] = ef!(get_primitive_array_critical as *const ());
        t[223] = ef!(release_primitive_array_critical as *const ());
        t[224] = ef!(get_string_units as *const ());
        t[225] = ef!(release_string_units as *const ());
        t[226] = ef!(new_ref as *const ());
        t[227] = ef!(ret0 as *const ());
        t[228] = ef!(exception_check as *const ());
        t[229] = ef!(new_direct_byte_buffer as *const ());
        t[230] = ef!(get_direct_buffer_address as *const ());
        t[231] = ef!(get_direct_buffer_capacity as *const ());
        t[232] = ef!(ret1 as *const ());

        JAVAVM_TABLE[3] = ef!(ret0 as *const ()); // DestroyJavaVM
        JAVAVM_TABLE[4] = ef!(vm_attach as *const ());
        JAVAVM_TABLE[5] = ef!(ret0 as *const ()); // DetachCurrentThread
        JAVAVM_TABLE[6] = ef!(vm_get_env as *const ());
        JAVAVM_TABLE[7] = ef!(vm_attach as *const ());

        let mut names = std::collections::HashMap::new();
        for (i, n) in [
            (4, "GetVersion"),
            (6, "FindClass"),
            (13, "Throw"),
            (14, "ThrowNew"),
            (15, "ExceptionOccurred"),
            (16, "ExceptionDescribe"),
            (17, "ExceptionClear"),
            (20, "PopLocalFrame"),
            (21, "NewLocalRef"),
            (24, "IsSameObject"),
            (25, "NewGlobalRef"),
            (27, "AllocObject"),
            (31, "GetObjectClass"),
            (32, "IsInstanceOf"),
            (33, "GetMethodID"),
            (36, "CallObjectMethodA"),
            (39, "CallBooleanMethodA"),
            (42, "CallByteMethodA"),
            (45, "CallCharMethodA"),
            (48, "CallShortMethodA"),
            (51, "CallIntMethodA"),
            (54, "CallLongMethodA"),
            (57, "CallFloatMethodA"),
            (60, "CallDoubleMethodA"),
            (63, "CallVoidMethodA"),
            (94, "GetFieldID"),
            (95, "GetObjectField"),
            (100, "GetIntField"),
            (104, "SetObjectField"),
            (113, "GetStaticMethodID"),
            (116, "CallStaticObjectMethodA"),
            (143, "CallStaticVoidMethodA"),
            (144, "GetStaticFieldID"),
            (145, "GetStaticObjectField"),
            (154, "SetStaticObjectField"),
            (163, "NewString"),
            (164, "GetStringLength"),
            (167, "NewStringUTF"),
            (168, "GetStringUTFLength"),
            (169, "GetStringUTFChars"),
            (170, "ReleaseStringUTFChars"),
            (171, "GetArrayLength"),
            (172, "NewObjectArray"),
            (173, "GetObjectArrayElement"),
            (174, "SetObjectArrayElement"),
            (184, "GetByteArrayElements"),
            (187, "GetIntArrayElements"),
            (192, "ReleaseByteArrayElements"),
            (195, "ReleaseIntArrayElements"),
            (200, "GetByteArrayRegion"),
            (203, "GetIntArrayRegion"),
            (208, "SetByteArrayRegion"),
            (211, "SetIntArrayRegion"),
            (215, "RegisterNatives"),
            (219, "GetJavaVM"),
            (228, "ExceptionCheck"),
            (229, "NewDirectByteBuffer"),
            (230, "GetDirectBufferAddress"),
            (231, "GetDirectBufferCapacity"),
        ] {
            let p = JNI_TABLE[i] as usize;
            if p != ef!(unimpl as *const ()) as usize {
                names.insert(p, n);
            }
        }
        let _ = ENV_NAMES.set(names);

        // Every table entry is a host fn that guest code calls while
        // TPIDR_EL0 points at the guest TCB — wrap each in a thunk
        // that restores the host pthread block for the call's
        // duration (libSystem's TPIDR-keyed per-thread state —
        // malloc magazine, TSD, stack_chk_guard — faults on the
        // guest TCB; observed as wild derefs inside the first call).
        for s in JNI_TABLE.iter_mut().chain(JAVAVM_TABLE.iter_mut()) {
            if *s != 0 {
                *s = crate::native::host_thunk(*s as usize) as u64;
            }
        }

        // Varargs entries (Call*Method, CallNonvirtual*Method,
        // NewObject): each is a thunk → register-spill stub → Rust
        // dispatcher. `va_thunk` already includes the outer TLS-swap
        // thunk, so these are stored after the wrap loop.
        let t = &mut JNI_TABLE;
        for i in [34, 37, 40, 43, 46, 49, 52, 55, 58, 61] {
            t[i] = crate::native::va_thunk(callv_dispatch as *const () as usize, 0) as u64;
        }
        for i in [35, 38, 41, 44, 47, 50, 53, 56, 59, 62] {
            t[i] = crate::native::host_thunk(callv_v as *const () as usize) as u64;
        }
        for i in [64, 67, 70, 73, 76, 79, 82, 85, 88, 91] {
            t[i] = crate::native::va_thunk(callv_dispatch as *const () as usize, 1) as u64;
        }
        for i in [65, 68, 71, 74, 77, 80, 83, 86, 89, 92] {
            t[i] = crate::native::host_thunk(callnv_v as *const () as usize) as u64;
        }
        for i in [66, 69, 72, 75, 78, 81, 84, 87, 90, 93] {
            t[i] = crate::native::host_thunk(callnv_a as *const () as usize) as u64;
        }
        t[28] = crate::native::va_thunk(callv_dispatch as *const () as usize, 2) as u64;
        t[29] = crate::native::host_thunk(new_object_v as *const () as usize) as u64;
        t[30] = crate::native::host_thunk(new_object_a as *const () as usize) as u64;
        t[7] = crate::native::host_thunk(get_superclass as *const () as usize) as u64;
        t[8] = crate::native::host_thunk(is_assignable_from as *const () as usize) as u64;
        for i in [5, 9, 10, 11, 12, 18] {
            t[i] = crate::native::host_thunk(ret0 as *const () as usize) as u64;
        }
    });
}

// ---------- small helpers ----------

extern "C" fn ret0() -> u64 {
    env_enter("ret0");
    0
}
extern "C" fn ret1() -> u64 {
    env_enter("ret1");
    1
}

pub(crate) fn cstr_ro(p: u64) -> String {
    if p == 0 {
        return String::new();
    }
    let mut s = Vec::new();
    let mut q = p as *const u8;
    unsafe {
        while *q != 0 {
            s.push(*q);
            q = q.add(1);
            // JNI strings are NUL-terminated and can legitimately be large
            // (JSON payloads, base64 blobs); the cap only guards against a
            // bad pointer walking forever through mapped memory.
            if s.len() > 16 * 1024 * 1024 {
                eprintln!("[jni] cstr_ro runaway at {p:#x} ({}+ bytes)", s.len());
                break;
            }
        }
    }
    String::from_utf8_lossy(&s).into_owned()
}

/// decode a jmethodID / jfieldID
fn mid_of(id: u64) -> Option<GIdx> {
    vm().jni_mids.get(id.checked_sub(1)? as usize).copied()
}

/// `jni_mids` entries with this tag index `jni_named` instead of the
/// dex method table — `GetMethodID` on host/JDK classes whose methods
/// only exist as builtins (Class.getDeclaredMethods, Field.get, …).
pub(crate) const GX_NAMED: GIdx = 1 << 63;

fn named_of(midx: GIdx) -> Option<(String, String, String)> {
    (midx & GX_NAMED != 0)
        .then(|| vm().jni_named.get((midx & !GX_NAMED) as usize).cloned())
        .flatten()
}
fn fid_of(id: u64) -> Option<GIdx> {
    vm().jni_fids.get(id.checked_sub(1)? as usize).copied()
}

/// jvalue → V by param descriptor char.
fn jval(slot: u64, desc: &str) -> V {
    match desc {
        "Z" | "B" | "C" | "S" | "I" => V::I32(slot as u32 as i32),
        "J" => V::I64(slot as i64),
        "F" => V::F32(f32::from_bits(slot as u32)),
        "D" => V::F64(f64::from_bits(slot)),
        _ => V::Ref(slot as u32),
    }
}

fn v_to_u64(v: V) -> u64 {
    match v {
        V::Ref(h) => h as u64,
        V::I32(i) => i as u32 as u64,
        V::I64(i) => i as u64,
        V::F32(f) => f.to_bits() as u64,
        V::F64(f) => f.to_bits(),
    }
}

/// Method proto params of a dex method id.
fn method_params(midx: GIdx) -> (String, Vec<String>, String) {
    if let Some((owner, _, sig)) = named_of(midx) {
        return (owner, split_params(&sig), sig_return(&sig));
    }
    let v = vm();
    let Some(d) = v.dex_of(midx) else {
        return (String::new(), Vec::new(), String::new());
    };
    let Ok(m) = d.method(gl(midx)) else {
        return (String::new(), Vec::new(), String::new());
    };
    let class = d.type_descriptor(m.class_idx as u32).unwrap_or_default();
    let sig = proto_sig(d, m.proto_idx as u32).unwrap_or_default();
    (class, split_params(&sig), sig_return(&sig))
}

fn split_params(sig: &str) -> Vec<String> {
    let mut out = Vec::new();
    let Some(p) = sig.strip_prefix('(').and_then(|s| s.split(')').next()) else {
        return out;
    };
    let mut cur = String::new();
    let mut in_ref = false;
    for c in p.chars() {
        cur.push(c);
        match c {
            'L' if !in_ref => in_ref = true,
            ';' if in_ref => {
                in_ref = false;
                out.push(std::mem::take(&mut cur));
            }
            '[' => {}
            _ if !in_ref => {
                out.push(std::mem::take(&mut cur));
            }
            _ => {}
        }
    }
    out
}

fn sig_return(sig: &str) -> String {
    sig.split(')').nth(1).unwrap_or("V").to_string()
}

/// Invoke a dex/host method through the interpreter.
fn call_obj(obj: u64, mid: u64, args: &[V], is_static: bool) -> Option<V> {
    let g = mid_of(mid)?;
    let mut v = vm();
    if let Some((owner, name, _)) = named_of(g) {
        let res = if is_static {
            v.call_static_named(&owner, &name, args.to_vec())
        } else {
            v.call_virtual(obj as u32, &name, args)
        };
        return res.ok().flatten();
    }
    let kind = if is_static {
        crate::InvokeKind::Static
    } else {
        crate::InvokeKind::Virtual
    };
    let mut a = Vec::with_capacity(args.len() + 1);
    if !is_static {
        a.push(V::Ref(obj as u32));
    }
    a.extend_from_slice(args);
    v.invoke_pub(g, a, kind).ok().flatten()
}

/// `CallNonvirtual*Method`: invoke the exact method `mid` resolves to
/// (found on the caller-supplied `clazz`) — the object's override must
/// NOT win, or a `super.foo()` through JNI recurses forever.
fn call_obj_direct(obj: u64, mid: u64, args: &[V]) -> Option<V> {
    let g = mid_of(mid)?;
    let mut v = vm();
    if let Some((_, name, _)) = named_of(g) {
        return v.call_virtual(obj as u32, &name, args).ok().flatten();
    }
    let mut a = Vec::with_capacity(args.len() + 1);
    a.push(V::Ref(obj as u32));
    a.extend_from_slice(args);
    v.invoke_pub(g, a, crate::InvokeKind::Direct).ok().flatten()
}

/// Method-id registration: name+sig on `clazz` (a jclass handle) → id.
fn find_method_id(clazz: u64, name_p: u64, sig_p: u64) -> u64 {
    let name = cstr_ro(name_p);
    let sig = cstr_ro(sig_p);
    if std::env::var("OVELO_JNIDBG").is_ok() {
        eprintln!("[jni] GetMethodID {name}{sig} clazz={clazz:#x}");
    }
    let params = split_params(&sig);
    let pref: Vec<&str> = params.iter().map(String::as_str).collect();
    let mut v = vm();
    let Some(ci) = (match clazz {
        h if h != 0 => v.class_for_jclass(h as u32),
        _ => None,
    }) else {
        return 0;
    };
    match v.find_method_on_chain_sig(ci, &name, &pref) {
        Ok(Some(g)) => {
            v.jni_mids.push(g);
            v.jni_mids.len() as u64
        }
        _ => {
            // Host/JDK class (java.lang.Class reflection and friends):
            // the method isn't on a dex chain but the builtin surface
            // can still answer it — register a by-name mid so the
            // cached jmethodID stays usable (Godot's JNISingleton
            // resolves its whole Java surface this way).
            let owner = v.type_desc_of(ci).unwrap_or_default();
            v.jni_named.push((owner, name.clone(), sig.clone()));
            let idx = (v.jni_named.len() - 1) as u64;
            v.jni_mids.push(GX_NAMED | idx);
            if std::env::var("OVELO_JNIDBG").is_ok() {
                eprintln!("[jni] GetMethodID named {name}{sig}");
            }
            v.jni_mids.len() as u64
        }
    }
}

fn find_field_id(clazz: u64, name_p: u64, sig_p: u64, is_static: bool) -> u64 {
    let name = cstr_ro(name_p);
    let sig = cstr_ro(sig_p);
    if std::env::var("OVELO_JNIDBG").is_ok() {
        eprintln!("[jni] GetFieldID {name} clazz={clazz:#x}");
    }
    let mut v = vm();
    let Some(ci) = (match clazz {
        h if h != 0 => v.class_for_jclass(h as u32),
        _ => None,
    }) else {
        return 0;
    };
    if let Some(g) = v.find_field_by_name(ci, &name, is_static, Some(&sig)) {
        v.jni_fids.push(g);
        return v.jni_fids.len() as u64;
    }
    eprintln!("[jni] GetFieldID miss {name}");
    0
}

// ---------- env function impls ----------

extern "C" fn get_version(_e: u64) -> u64 {
    env_enter("get_version");
    0x0001_0008
}

extern "C" fn find_class(_e: u64, name_p: u64) -> u64 {
    env_enter("find_class");
    let name = cstr_ro(name_p);
    let dbg = std::env::var("OVELO_JNIDBG").is_ok();
    if dbg {
        eprintln!("[jni] FindClass {name}");
    }
    let desc = format!("L{};", name.replace('.', "/"));
    let mut v = vm();
    if dbg {
        eprintln!("[jni]   type_index_of..");
    }
    let ci = v.type_index_of(&desc);
    if dbg {
        eprintln!("[jni]   ci={ci:?} -> mirror..");
    }
    match ci {
        Some(ci) => {
            let h = v.class_mirror(ci) as u64;
            if dbg {
                eprintln!("[jni]   mirror={h:#x}");
            }
            h
        }
        None => {
            eprintln!("[jni] FindClass miss {desc}");
            0
        }
    }
}

extern "C" fn throw(_e: u64, obj: u64) -> u64 {
    env_enter("throw");
    vm().jni_pending_exc = obj as u32;
    0
}
extern "C" fn throw_new(_e: u64, _clazz: u64, msg: u64) -> u64 {
    env_enter("throw_new");
    let m = cstr_ro(msg);
    eprintln!("[jni] ThrowNew {m}");
    vm().jni_pending_exc = u32::MAX;
    0
}
extern "C" fn exception_occurred(_e: u64) -> u64 {
    env_enter("exception_occurred");
    let p = vm().jni_pending_exc;
    if p == u32::MAX {
        0
    } else {
        p as u64
    }
}
extern "C" fn exception_describe(_e: u64) -> u64 {
    env_enter("exception_describe");
    eprintln!(
        "[jni] ExceptionDescribe: pending={:#x}",
        vm().jni_pending_exc
    );
    0
}
extern "C" fn exception_clear(_e: u64) -> u64 {
    env_enter("exception_clear");
    vm().jni_pending_exc = 0;
    0
}
extern "C" fn exception_check(_e: u64) -> u64 {
    env_enter("exception_check");
    u64::from(vm().jni_pending_exc != 0)
}

extern "C" fn pop_local_frame(_e: u64, obj: u64) -> u64 {
    env_enter("pop_local_frame");
    obj
}
extern "C" fn new_ref(_e: u64, obj: u64) -> u64 {
    env_enter("new_ref");
    obj
}
extern "C" fn is_same_object(_e: u64, a: u64, b: u64) -> u64 {
    env_enter("is_same_object");
    u64::from(a == b)
}

extern "C" fn alloc_object(_e: u64, clazz: u64) -> u64 {
    env_enter("alloc_object");
    let mut v = vm();
    let ci = v.heap.class_of(clazz as u32).unwrap_or(crate::GX_NONE);
    let h = v.heap.alloc(Obj::Instance {
        class_idx: ci,
        fields: std::collections::HashMap::new(),
    });
    h as u64
}
extern "C" fn get_object_class(_e: u64, obj: u64) -> u64 {
    env_enter("get_object_class");
    let mut v = vm();
    match v.heap.class_of(obj as u32) {
        Some(ci) => v.class_mirror(ci) as u64,
        None => 0,
    }
}
extern "C" fn is_instance_of(_e: u64, obj: u64, clazz: u64) -> u64 {
    env_enter("is_instance_of");
    let v = vm();
    match (v.heap.class_of(obj as u32), v.heap.class_of(clazz as u32)) {
        (Some(a), Some(b)) => u64::from(v.assignable(a, b)),
        _ => 0,
    }
}

extern "C" fn get_method_id(_e: u64, clazz: u64, name_p: u64, sig_p: u64) -> u64 {
    env_enter("get_method_id");
    find_method_id(clazz, name_p, sig_p)
}
extern "C" fn get_static_method_id(_e: u64, clazz: u64, name_p: u64, sig_p: u64) -> u64 {
    env_enter("get_static_method_id");
    find_method_id(clazz, name_p, sig_p)
}
extern "C" fn get_field_id(_e: u64, clazz: u64, name_p: u64, sig_p: u64) -> u64 {
    env_enter("get_field_id");
    find_field_id(clazz, name_p, sig_p, false)
}
extern "C" fn get_static_field_id(_e: u64, clazz: u64, name_p: u64, sig_p: u64) -> u64 {
    env_enter("get_static_field_id");
    find_field_id(clazz, name_p, sig_p, true)
}

fn jargs(params: &[String], jvalues: u64) -> Vec<V> {
    params
        .iter()
        .enumerate()
        .map(|(i, d)| {
            jval(
                unsafe { ((jvalues + 8 * i as u64) as *const u64).read() },
                d,
            )
        })
        .collect()
}

macro_rules! call_a {
    ($name:ident, $wrap:expr) => {
        extern "C" fn $name(_e: u64, obj: u64, mid: u64, jvalues: u64) -> u64 {
            let Some(g) = mid_of(mid) else { return 0 };
            let (_, params, _) = method_params(g);
            let args = jargs(&params, jvalues);
            call_obj(obj, mid, &args, false).map($wrap).unwrap_or(0)
        }
    };
}
call_a!(call_object_method_a, |v: V| v_to_u64(v));
call_a!(call_boolean_method_a, |v: V| v_to_u64(v));
call_a!(call_byte_method_a, |v: V| v_to_u64(v));
call_a!(call_char_method_a, |v: V| v_to_u64(v));
call_a!(call_short_method_a, |v: V| v_to_u64(v));
call_a!(call_int_method_a, |v: V| v_to_u64(v));
call_a!(call_long_method_a, |v: V| v_to_u64(v));
call_a!(call_float_method_a, |v: V| v_to_u64(v));
call_a!(call_double_method_a, |v: V| v_to_u64(v));
extern "C" fn call_void_method_a(_e: u64, obj: u64, mid_: u64, jvalues: u64) -> u64 {
    env_enter("call_void_method_a");
    let Some(g) = mid_of(mid_) else { return 0 };
    let (_, params, _) = method_params(g);
    let args = jargs(&params, jvalues);
    call_obj(obj, mid_, &args, false);
    0
}

macro_rules! call_static_a {
    ($name:ident) => {
        extern "C" fn $name(_e: u64, _clazz: u64, mid: u64, jvalues: u64) -> u64 {
            let Some(g) = mid_of(mid) else { return 0 };
            let (_, params, _) = method_params(g);
            let args = jargs(&params, jvalues);
            call_obj(0, mid, &args, true).map(v_to_u64).unwrap_or(0)
        }
    };
}
call_static_a!(call_static_object_method_a);
call_static_a!(call_static_boolean_method_a);
call_static_a!(call_static_byte_method_a);
call_static_a!(call_static_char_method_a);
call_static_a!(call_static_short_method_a);
call_static_a!(call_static_int_method_a);
call_static_a!(call_static_long_method_a);
call_static_a!(call_static_float_method_a);
call_static_a!(call_static_double_method_a);
call_static_a!(call_static_void_method_a);

// ---------- varargs Call*Method / NewObject ----------

/// Arg streams for the register-spill stub: GP varargs land in saved
/// x3-x7 then the caller stack; FP varargs in saved d0-d7, then the
/// same stack stream (interleaved by argument order once registers
/// run out — stack index is shared).
struct VaRegs {
    gp: [u64; 5],
    gn: usize,
    fp: [u64; 8],
    stack: u64,
    gi: usize,
    fi: usize,
    si: usize,
}
impl VaRegs {
    unsafe fn next_gp(&mut self) -> u64 {
        if self.gi < self.gn {
            let v = self.gp[self.gi];
            self.gi += 1;
            v
        } else {
            let v = ((self.stack as *const u64).add(self.si)).read();
            self.si += 1;
            v
        }
    }
    unsafe fn next_fp(&mut self) -> u64 {
        if self.fi < 8 {
            let v = self.fp[self.fi];
            self.fi += 1;
            v
        } else {
            let v = ((self.stack as *const u64).add(self.si)).read();
            self.si += 1;
            v
        }
    }
}

fn va_jargs(params: &[String], va: &mut VaRegs) -> Vec<V> {
    params
        .iter()
        .map(|d| match d.as_str() {
            "F" | "D" => jval(unsafe { va.next_fp() }, d),
            _ => jval(unsafe { va.next_gp() }, d),
        })
        .collect()
}

/// Shared body for the varargs/`va_list`/`jvalue[]` call entries.
/// kind: 0 = Call*Method(env,obj,mid,..), 1 = CallNonvirtual*Method
/// (env,obj,clazz,mid,..), 2 = NewObject(env,clazz,mid,..).
fn callv_run(obj: u64, mid: u64, args: &[V], new_obj: u64, nv: bool) -> u64 {
    if mid_of(mid).is_none() {
        eprintln!("[jni] callv: bad mid {mid:#x}");
        return new_obj;
    }
    if new_obj != 0 {
        // NewObject: <init> is a void instance method on the fresh object.
        call_obj(new_obj, mid, args, false);
        return new_obj;
    }
    if nv {
        call_obj_direct(obj, mid, args).map(v_to_u64).unwrap_or(0)
    } else {
        call_obj(obj, mid, args, false).map(v_to_u64).unwrap_or(0)
    }
}

/// Register-dump varargs dispatcher — called by the `emit_va` stub with
/// a saved x0-x7 + d0-d7 buffer and the caller's stack-args pointer.
extern "C" fn callv_dispatch(regs: u64, stack: u64, kind: u64) -> u64 {
    env_enter("callv");
    unsafe {
        let r = regs as *const u64;
        let (obj_or_clazz, mid, gn) = match kind {
            1 => (r.add(1).read(), r.add(3).read(), 4usize), // env,obj,clazz,mid,...
            _ => (r.add(1).read(), r.add(2).read(), 5usize), // env,obj/clazz,mid,...
        };
        let gp_start = 8 - gn;
        let mut va = VaRegs {
            gp: [0; 5],
            gn,
            fp: [0; 8],
            stack,
            gi: 0,
            fi: 0,
            si: 0,
        };
        for (i, slot) in va.gp.iter_mut().enumerate().take(gn) {
            *slot = r.add(gp_start + i).read();
        }
        for (i, slot) in va.fp.iter_mut().enumerate() {
            *slot = r.add(8 + i).read();
        }
        let Some(g) = mid_of(mid) else {
            eprintln!("[jni] callv: bad mid {mid:#x}");
            return 0;
        };
        let (_, params, _) = method_params(g);
        let args = va_jargs(&params, &mut va);
        if kind == 2 {
            // NewObject: allocate, run <init>, return the object.
            let mut v = vm();
            let Some(ci) = v.class_for_jclass(obj_or_clazz as u32) else {
                return 0;
            };
            let h = v.heap.alloc(Obj::Instance {
                class_idx: ci,
                fields: std::collections::HashMap::new(),
            }) as u64;
            drop(v);
            callv_run(0, mid, &args, h, false)
        } else {
            callv_run(obj_or_clazz, mid, &args, 0, kind == 1)
        }
    }
}

/// Bionic aarch64 `va_list`: {stack, gr_top, vr_top, gr_offs, vr_offs}
/// — 32 bytes, passed by reference. GP slots are 8B at gr_top+gr_offs
/// (offs ≤ 0, +8 each); VR slots are 16B at vr_top+vr_offs (+16 each);
/// past the end both streams continue on `stack` (8B each, sequential).
struct VaList {
    stack: u64,
    gr_top: u64,
    vr_top: u64,
    gr_offs: i64,
    vr_offs: i64,
}
impl VaList {
    unsafe fn load(p: u64) -> Self {
        let q = p as *const u64;
        // gr_offs/vr_offs are 4-byte ints at byte offsets 24 and 28.
        Self {
            stack: q.read(),
            gr_top: q.add(1).read(),
            vr_top: q.add(2).read(),
            gr_offs: ((p + 24) as *const i32).read() as i64,
            vr_offs: ((p + 28) as *const i32).read() as i64,
        }
    }
    unsafe fn next_gp(&mut self) -> u64 {
        if self.gr_offs < 0 {
            let v = ((self.gr_top as i64 + self.gr_offs) as *const u64).read();
            self.gr_offs += 8;
            v
        } else {
            let v = (self.stack as *const u64).read();
            self.stack += 8;
            v
        }
    }
    unsafe fn next_fp(&mut self) -> u64 {
        if self.vr_offs < 0 {
            let v = ((self.vr_top as i64 + self.vr_offs) as *const u64).read();
            self.vr_offs += 16;
            v
        } else {
            let v = (self.stack as *const u64).read();
            self.stack += 8;
            v
        }
    }
}

fn valist_jargs(params: &[String], va: &mut VaList) -> Vec<V> {
    params
        .iter()
        .map(|d| match d.as_str() {
            "F" | "D" => jval(unsafe { va.next_fp() }, d),
            _ => jval(unsafe { va.next_gp() }, d),
        })
        .collect()
}

extern "C" fn callv_v(_e: u64, obj: u64, mid: u64, va_p: u64) -> u64 {
    env_enter("callv_v");
    let Some(g) = mid_of(mid) else {
        return 0;
    };
    let (_, params, _) = method_params(g);
    let mut va = unsafe { VaList::load(va_p) };
    let args = valist_jargs(&params, &mut va);
    call_obj(obj, mid, &args, false).map(v_to_u64).unwrap_or(0)
}
extern "C" fn callnv_v(_e: u64, obj: u64, _clazz: u64, mid: u64, va_p: u64) -> u64 {
    env_enter("callnv_v");
    let Some(g) = mid_of(mid) else {
        return 0;
    };
    let (_, params, _) = method_params(g);
    let mut va = unsafe { VaList::load(va_p) };
    let args = valist_jargs(&params, &mut va);
    call_obj_direct(obj, mid, &args).map(v_to_u64).unwrap_or(0)
}
extern "C" fn callnv_a(_e: u64, obj: u64, _clazz: u64, mid: u64, jvalues: u64) -> u64 {
    env_enter("callnv_a");
    let Some(g) = mid_of(mid) else {
        return 0;
    };
    let (_, params, _) = method_params(g);
    let args = jargs(&params, jvalues);
    call_obj_direct(obj, mid, &args).map(v_to_u64).unwrap_or(0)
}

fn new_object_body(clazz: u64, mid: u64, args: &[V]) -> u64 {
    let mut v = vm();
    let Some(ci) = v.class_for_jclass(clazz as u32) else {
        return 0;
    };
    let h = v.heap.alloc(Obj::Instance {
        class_idx: ci,
        fields: std::collections::HashMap::new(),
    }) as u64;
    drop(v);
    call_obj(h, mid, args, false);
    h
}
extern "C" fn new_object_v(_e: u64, clazz: u64, mid: u64, va_p: u64) -> u64 {
    env_enter("new_object_v");
    let Some(g) = mid_of(mid) else {
        return 0;
    };
    let (_, params, _) = method_params(g);
    let mut va = unsafe { VaList::load(va_p) };
    let args = valist_jargs(&params, &mut va);
    new_object_body(clazz, mid, &args)
}
extern "C" fn new_object_a(_e: u64, clazz: u64, mid: u64, jvalues: u64) -> u64 {
    env_enter("new_object_a");
    let Some(g) = mid_of(mid) else {
        return 0;
    };
    let (_, params, _) = method_params(g);
    let args = jargs(&params, jvalues);
    new_object_body(clazz, mid, &args)
}

extern "C" fn get_superclass(_e: u64, clazz: u64) -> u64 {
    env_enter("get_superclass");
    let mut v = vm();
    let Some(ci) = v.class_for_jclass(clazz as u32) else {
        return 0;
    };
    let Ok(own) = v.type_desc_of(ci) else {
        return 0;
    };
    let Some(sd) = v.super_desc(&own) else {
        return 0;
    };
    match v.type_index_of(&sd) {
        Some(s) => v.class_mirror(s) as u64,
        None => 0,
    }
}
extern "C" fn is_assignable_from(_e: u64, sub: u64, sup: u64) -> u64 {
    env_enter("is_assignable_from");
    let v = vm();
    match (
        v.class_for_jclass(sub as u32),
        v.class_for_jclass(sup as u32),
    ) {
        (Some(a), Some(b)) => u64::from(v.assignable(a, b)),
        _ => 0,
    }
}

fn field_get_v(obj: u64, id: u64) -> V {
    let Some(g) = fid_of(id) else {
        return V::I32(0);
    };
    let v = vm();
    let f = v.canon_field(g);
    match v.heap.get(obj as u32) {
        Some(Obj::Instance { fields, .. }) | Some(Obj::Host { fields, .. }) => {
            fields.get(&f).copied().unwrap_or(V::I32(0))
        }
        _ => V::I32(0),
    }
}
fn field_set_v(obj: u64, id: u64, val: u64) {
    let Some(g) = fid_of(id) else { return };
    let mut v = vm();
    let f = v.canon_field(g);
    if let Some(Obj::Instance { fields, .. }) | Some(Obj::Host { fields, .. }) =
        v.heap.get_mut(obj as u32)
    {
        fields.insert(f, V::I64(val as i64));
    }
}
fn static_get_v(_clazz: u64, id: u64) -> V {
    let Some(g) = fid_of(id) else {
        return V::I32(0);
    };
    vm().sget_pub(g).unwrap_or(V::I32(0))
}
fn static_set_v(_clazz: u64, id: u64, val: u64) {
    let Some(g) = fid_of(id) else { return };
    vm().sput_pub(g, V::I64(val as i64));
}

macro_rules! getf {
    ($n:ident) => {
        extern "C" fn $n(_e: u64, obj: u64, id: u64) -> u64 {
            v_to_u64(field_get_v(obj, id))
        }
    };
}
getf!(get_object_field);
getf!(get_boolean_field);
getf!(get_byte_field);
getf!(get_char_field);
getf!(get_short_field);
getf!(get_int_field);
getf!(get_long_field);
getf!(get_float_field);
getf!(get_double_field);

macro_rules! setf {
    ($n:ident) => {
        extern "C" fn $n(_e: u64, obj: u64, id: u64, val: u64) {
            field_set_v(obj, id, val)
        }
    };
}
setf!(set_object_field);
setf!(set_boolean_field);
setf!(set_byte_field);
setf!(set_char_field);
setf!(set_short_field);
setf!(set_int_field);
setf!(set_long_field);
setf!(set_float_field);
setf!(set_double_field);

macro_rules! gets {
    ($n:ident) => {
        extern "C" fn $n(_e: u64, clazz: u64, id: u64) -> u64 {
            v_to_u64(static_get_v(clazz, id))
        }
    };
}
gets!(get_static_object_field);
gets!(get_static_boolean_field);
gets!(get_static_byte_field);
gets!(get_static_char_field);
gets!(get_static_short_field);
gets!(get_static_int_field);
gets!(get_static_long_field);
gets!(get_static_float_field);
gets!(get_static_double_field);

macro_rules! sets {
    ($n:ident) => {
        extern "C" fn $n(_e: u64, clazz: u64, id: u64, val: u64) {
            static_set_v(clazz, id, val)
        }
    };
}
sets!(set_static_object_field);
sets!(set_static_boolean_field);
sets!(set_static_byte_field);
sets!(set_static_char_field);
sets!(set_static_short_field);
sets!(set_static_int_field);
sets!(set_static_long_field);
sets!(set_static_float_field);
sets!(set_static_double_field);

// ---------- strings ----------

extern "C" fn new_string_utf(_e: u64, p: u64) -> u64 {
    env_enter("new_string_utf");
    let s = cstr_ro(p);
    vm().heap.intern_str(&s) as u64
}
extern "C" fn new_string(_e: u64, _p: u64, _len: u64) -> u64 {
    env_enter("new_string");
    0
}
extern "C" fn get_string_length(_e: u64, s: u64) -> u64 {
    env_enter("get_string_length");
    vm().heap
        .str_val(s as u32)
        .map(|x| x.chars().count() as u64)
        .unwrap_or(0)
}
extern "C" fn get_string_utf_length(_e: u64, s: u64) -> u64 {
    env_enter("get_string_utf_length");
    let Some(x) = vm().heap.str_val(s as u32).map(|x| x.to_string()) else {
        return 0;
    };
    // Modified UTF-8 byte count: NUL is 2 bytes and supplementary
    // characters are two 3-byte units, so this is not `x.len()`.
    x.encode_utf16()
        .map(|u| match u {
            0 => 2,
            1..=0x7F => 1,
            0x80..=0x7FF => 2,
            _ => 3,
        })
        .sum()
}
extern "C" fn get_string_utf_chars(_e: u64, s: u64, _copy: u64) -> u64 {
    env_enter("get_string_utf_chars");
    let Some(x) = vm().heap.str_val(s as u32).map(|x| x.to_string()) else {
        return 0;
    };
    let mut out = Vec::with_capacity(x.len() + 1);
    for u in x.encode_utf16() {
        mutf8_unit(&mut out, u);
    }
    let buf = libc_malloc(out.len() + 1) as *mut u8;
    if buf.is_null() {
        return 0;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(out.as_ptr(), buf, out.len());
        *buf.add(out.len()) = 0;
    }
    buf as u64
}
extern "C" fn release_string_utf_chars(_e: u64, _s: u64, p: u64) -> u64 {
    env_enter("release_string_utf_chars");
    if p != 0 {
        libc_free(p as usize);
    }
    0
}

/// `GetStringChars`/`GetStringCritical`: UTF-16 buffer, NUL-terminated.
extern "C" fn get_string_units(_e: u64, s: u64, _copy: u64) -> u64 {
    env_enter("get_string_units");
    let Some(x) = vm().heap.str_val(s as u32).map(|x| x.to_string()) else {
        return 0;
    };
    let u16s: Vec<u16> = x.encode_utf16().collect();
    let buf = libc_malloc((u16s.len() + 1) * 2) as *mut u16;
    if buf.is_null() {
        return 0;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(u16s.as_ptr(), buf, u16s.len());
        *buf.add(u16s.len()) = 0;
    }
    buf as u64
}
extern "C" fn release_string_units(_e: u64, _s: u64, p: u64) {
    env_enter("release_string_units");
    if p != 0 {
        libc_free(p as usize);
    }
}
extern "C" fn get_string_region(_e: u64, s: u64, start: u64, len: u64, buf: u64) {
    env_enter("get_string_region");
    let Some(x) = vm().heap.str_val(s as u32).map(|x| x.to_string()) else {
        return;
    };
    let u16s: Vec<u16> = x.encode_utf16().collect();
    if !crate::native::guest_dst_ok(buf as usize, len as usize * 2) {
        eprintln!("[guestptr] get_string_region: write {buf:#x}+{len}*2 not guest/stack/image");
        return;
    }
    for i in 0..len as usize {
        let v = u16s.get(start as usize + i).copied().unwrap_or(0);
        unsafe { *((buf + i as u64 * 2) as *mut u16) = v };
    }
}
/// One UTF-16 code unit in modified UTF-8 — each unit encodes
/// independently (surrogate pairs become two 3-byte sequences, never
/// merged), and NUL is 0xC0 0x80.
fn mutf8_unit(out: &mut Vec<u8>, u: u16) {
    match u {
        0 => out.extend_from_slice(&[0xC0, 0x80]),
        1..=0x7F => out.push(u as u8),
        0x80..=0x7FF => out.extend_from_slice(&[0xC0 | (u >> 6) as u8, 0x80 | (u & 0x3F) as u8]),
        _ => out.extend_from_slice(&[
            0xE0 | (u >> 12) as u8,
            0x80 | ((u >> 6) & 0x3F) as u8,
            0x80 | (u & 0x3F) as u8,
        ]),
    }
}

extern "C" fn get_string_utf_region(_e: u64, s: u64, start: u64, len: u64, buf: u64) {
    env_enter("get_string_utf_region");
    let Some(x) = vm().heap.str_val(s as u32).map(|x| x.to_string()) else {
        return;
    };
    let u16s: Vec<u16> = x.encode_utf16().collect();
    let lo = (start as usize).min(u16s.len());
    let hi = (start as usize + len as usize).min(u16s.len());
    let mut out = Vec::with_capacity((hi - lo) * 3);
    for &u in &u16s[lo..hi] {
        mutf8_unit(&mut out, u);
    }
    if !crate::native::guest_dst_ok(buf as usize, out.len()) {
        eprintln!(
            "[guestptr] get_string_utf_region: write {buf:#x}+{} not guest/stack/image",
            out.len()
        );
        return;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(out.as_ptr(), buf as *mut u8, out.len());
    }
}

// ---------- arrays ----------

fn arr_data(h: u32) -> Option<Vec<V>> {
    match vm().heap.get(h) {
        Some(Obj::Array { data, .. }) => Some(data.clone()),
        _ => None,
    }
}

extern "C" fn get_array_length(_e: u64, a: u64) -> u64 {
    env_enter("get_array_length");
    arr_data(a as u32).map(|d| d.len() as u64).unwrap_or(0)
}
extern "C" fn new_object_array(_e: u64, len: u64, _clazz: u64, init: u64) -> u64 {
    env_enter("new_object_array");
    let mut v = vm();
    let h = v.heap.alloc(Obj::Array {
        elem: "Ljava/lang/Object;".into(),
        data: vec![V::Ref(init as u32); len as usize],
    });
    h as u64
}
extern "C" fn get_object_array_element(_e: u64, a: u64, i: u64) -> u64 {
    env_enter("get_object_array_element");
    arr_data(a as u32)
        .and_then(|d| d.get(i as usize).copied())
        .map(v_to_u64)
        .unwrap_or(0)
}
extern "C" fn set_object_array_element(_e: u64, a: u64, i: u64, v: u64) {
    env_enter("set_object_array_element");
    if let Some(Obj::Array { data, .. }) = vm().heap.get_mut(a as u32) {
        if let Some(slot) = data.get_mut(i as usize) {
            *slot = V::Ref(v as u32);
        }
    }
}

macro_rules! new_arr {
    ($n:ident, $elem:expr, $v:expr) => {
        extern "C" fn $n(_e: u64, len: u64) -> u64 {
            let mut v = vm();
            let h = v.heap.alloc(Obj::Array {
                elem: $elem.into(),
                data: vec![$v; len as usize],
            });
            h as u64
        }
    };
}
new_arr!(new_boolean_array, "Z", V::I32(0));
new_arr!(new_byte_array, "B", V::I32(0));
new_arr!(new_char_array, "C", V::I32(0));
new_arr!(new_short_array, "S", V::I32(0));
new_arr!(new_int_array, "I", V::I32(0));
new_arr!(new_long_array, "J", V::I64(0));
new_arr!(new_float_array, "F", V::F32(0.0));
new_arr!(new_double_array, "D", V::F64(0.0));

/// element byte width for the typed array accessors
fn elem_size(h: u32) -> usize {
    match vm().heap.get(h) {
        Some(Obj::Array { elem, .. }) => match elem.as_str() {
            "J" | "D" => 8,
            "I" | "F" | "C" | "S" => match elem.as_str() {
                "C" | "S" => 2,
                _ => 4,
            },
            "Z" | "B" => 1,
            _ => 8,
        },
        _ => 8,
    }
}
fn elem_bits(v: V, size: usize) -> u64 {
    match (v, size) {
        (V::Ref(h), _) => h as u64,
        (V::I64(i), _) => i as u64,
        (V::F64(f), _) => f.to_bits(),
        (V::F32(f), _) => f.to_bits() as u64,
        (V::I32(i), 8) => i as u32 as u64,
        (V::I32(i), _) => i as u32 as u64 & ((1u64 << (size * 8)) - 1),
    }
}
fn bits_elem(bits: u64, elem: &str) -> V {
    match elem {
        "J" => V::I64(bits as i64),
        "D" => V::F64(f64::from_bits(bits)),
        "F" => V::F32(f32::from_bits(bits as u32)),
        _ => V::I32(bits as u32 as i32),
    }
}

// Elements are packed at their native width — a jint array hands the
// native a contiguous `int32_t*`, not an array of 8-byte slots.
fn get_elems(a: u64, _copy: u64) -> u64 {
    let h = a as u32;
    let Some(d) = arr_data(h) else { return 0 };
    let sz = elem_size(h);
    let buf = libc_malloc(d.len() * sz) as u64;
    for (i, v) in d.iter().enumerate() {
        // Exactly `sz` bytes per element — a u64 store would run past
        // the malloc'd block.
        unsafe {
            let bytes = elem_bits(*v, sz).to_le_bytes();
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), (buf + (i * sz) as u64) as *mut u8, sz);
        }
    }
    buf
}
fn release_elems(a: u64, buf: u64, mode: u64) {
    let h = a as u32;
    if mode != 2 {
        let sz = elem_size(h);
        if let Some(Obj::Array { data, elem }) = vm().heap.get_mut(h) {
            let e = elem.clone();
            for (i, slot) in data.iter_mut().enumerate() {
                // Read exactly `sz` bytes — a u64 read at a narrow
                // stride would touch memory past the buffer's end.
                let bits = unsafe {
                    let mut b = [0u8; 8];
                    std::ptr::copy_nonoverlapping(
                        (buf + (i * sz) as u64) as *const u8,
                        b.as_mut_ptr(),
                        sz,
                    );
                    u64::from_le_bytes(b)
                };
                *slot = bits_elem(bits & (u64::MAX >> (64 - sz * 8).min(63)), &e);
            }
        }
    }
    if mode != 1 && buf != 0 {
        libc_free(buf as usize);
    }
}

/// Buffers handed to guest code live in the guest zone — a guest-side
/// overrun then damages guest metadata, not the Rust heap.
/// Under OVELO_BUFDBG every allocation gets a magic tail that is
/// verified on free and on every env entry, naming the env call that
/// first saw an overrun.
fn libc_malloc(n: usize) -> usize {
    let dbg = std::env::var_os("OVELO_BUFDBG").is_some();
    if !dbg {
        return crate::native::guest_malloc(n);
    }
    let p = crate::native::guest_malloc(n + 8);
    if p == 0 {
        return 0;
    }
    // p+n isn't necessarily 8-aligned — use unaligned access.
    unsafe { ((p + n) as *mut u64).write_unaligned(0xdead_beef_cafe_f00d) };
    if let Ok(mut b) = bufs().lock() {
        b.insert(p, n);
    }
    p
}
fn libc_free(p: usize) {
    if p == 0 {
        return;
    }
    let n = bufs().lock().ok().and_then(|mut b| b.remove(&p));
    if let Some(n) = n {
        let m = unsafe { ((p + n) as *const u64).read_unaligned() };
        if m != 0xdead_beef_cafe_f00d {
            eprintln!(
                "[bufdbg] canary dead at {p:#x}+{n} on free; last env {}",
                last_env_get()
            );
        }
    }
    crate::native::guest_free(p)
}
fn bufs() -> &'static std::sync::Mutex<std::collections::HashMap<usize, usize>> {
    static B: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<usize, usize>>> =
        std::sync::OnceLock::new();
    B.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}
fn check_buf_canaries(name: &'static str) {
    if std::env::var_os("OVELO_BUFDBG").is_none() {
        return;
    }
    if let Ok(b) = bufs().lock() {
        for (&p, &n) in b.iter() {
            let m = unsafe { ((p + n) as *const u64).read_unaligned() };
            if m != 0xdead_beef_cafe_f00d {
                eprintln!(
                    "[bufdbg] overrun: buf {p:#x}+{n} bad on entry to {name}; previous env {}",
                    last_env_get()
                );
            }
        }
    }
}

macro_rules! ga {
    ($n:ident) => {
        extern "C" fn $n(_e: u64, a: u64, copy: u64) -> u64 {
            get_elems(a, copy)
        }
    };
}
ga!(get_boolean_array_elements);
ga!(get_byte_array_elements);
ga!(get_char_array_elements);
ga!(get_short_array_elements);
ga!(get_int_array_elements);
ga!(get_long_array_elements);
ga!(get_float_array_elements);
ga!(get_double_array_elements);

macro_rules! ra {
    ($n:ident) => {
        extern "C" fn $n(_e: u64, a: u64, buf: u64, mode: u64) {
            release_elems(a, buf, mode)
        }
    };
}
ra!(release_boolean_array_elements);
ra!(release_byte_array_elements);
ra!(release_char_array_elements);
ra!(release_short_array_elements);
ra!(release_int_array_elements);
ra!(release_long_array_elements);
ra!(release_float_array_elements);
ra!(release_double_array_elements);

/// `GetPrimitiveArrayCritical` — same buffer contract as
/// `Get*ArrayElements`; libgdx uses it for its scratch float arrays.
extern "C" fn get_primitive_array_critical(_e: u64, a: u64, copy: u64) -> u64 {
    env_enter("get_primitive_array_critical");
    get_elems(a, copy)
}
extern "C" fn release_primitive_array_critical(_e: u64, a: u64, buf: u64, mode: u64) {
    env_enter("release_primitive_array_critical");
    release_elems(a, buf, mode)
}

fn get_region(a: u64, start: u64, len: u64, out: u64) {
    let h = a as u32;
    let Some(d) = arr_data(h) else { return };
    let sz = elem_size(h);
    if !crate::native::guest_dst_ok(out as usize, len as usize * sz) {
        eprintln!("[guestptr] get_region: write {out:#x}+{len}*{sz} not guest/stack/image");
        return;
    }
    for i in 0..len as usize {
        let v = d.get(start as usize + i).copied().unwrap_or(V::I32(0));
        // Write exactly `sz` bytes — a full u64 store at a narrower
        // stride spills into the caller's buffer past the last element.
        unsafe {
            let bytes = elem_bits(v, sz).to_le_bytes();
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), (out + (i * sz) as u64) as *mut u8, sz);
        }
    }
}
fn set_region(a: u64, start: u64, len: u64, inp: u64) {
    let h = a as u32;
    let sz = elem_size(h);
    if let Some(Obj::Array { data, elem }) = vm().heap.get_mut(h) {
        let e = elem.clone();
        for i in 0..len as usize {
            let bits = unsafe {
                let mut b = [0u8; 8];
                std::ptr::copy_nonoverlapping(
                    (inp + (i * sz) as u64) as *const u8,
                    b.as_mut_ptr(),
                    sz,
                );
                u64::from_le_bytes(b)
            };
            if let Some(slot) = data.get_mut(start as usize + i) {
                *slot = bits_elem(bits & (u64::MAX >> (64 - sz * 8).min(63)), &e);
            }
        }
    }
}

macro_rules! gr {
    ($n:ident) => {
        extern "C" fn $n(_e: u64, a: u64, start: u64, len: u64, out: u64) {
            get_region(a, start, len, out)
        }
    };
}
gr!(get_boolean_array_region);
gr!(get_byte_array_region);
gr!(get_char_array_region);
gr!(get_short_array_region);
gr!(get_int_array_region);
gr!(get_long_array_region);
gr!(get_float_array_region);
gr!(get_double_array_region);

macro_rules! sr {
    ($n:ident) => {
        extern "C" fn $n(_e: u64, a: u64, start: u64, len: u64, inp: u64) {
            set_region(a, start, len, inp)
        }
    };
}
sr!(set_boolean_array_region);
sr!(set_byte_array_region);
sr!(set_char_array_region);
sr!(set_short_array_region);
sr!(set_int_array_region);
sr!(set_long_array_region);
sr!(set_float_array_region);
sr!(set_double_array_region);

// ---------- misc ----------

extern "C" fn register_natives(_e: u64, clazz: u64, methods: u64, n: u64) -> u64 {
    env_enter("register_natives");
    if std::env::var("OVELO_JNIDBG").is_ok() {
        eprintln!("[jni] RegisterNatives clazz={clazz:#x} n={n}");
    }
    let mut v = vm();
    let desc = v
        .class_for_jclass(clazz as u32)
        .and_then(|ci| v.type_desc_of(ci).ok())
        .unwrap_or_default();
    // RegisterNatives applies to the most recently loaded lib — the one
    // whose JNI_OnLoad is running.
    let Some(lib) = v.natives.last_mut() else {
        return 0;
    };
    for i in 0..n as usize {
        let (name_p, sig_p, fn_p) = unsafe {
            (
                ((methods + 24 * i as u64) as *const u64).read(),
                ((methods + 24 * i as u64 + 8) as *const u64).read(),
                ((methods + 24 * i as u64 + 16) as *const u64).read(),
            )
        };
        let name = cstr_ro(name_p);
        let sig = cstr_ro(sig_p);
        lib.registered
            .insert((desc.clone(), name, sig), fn_p as usize);
    }
    0
}

extern "C" fn get_java_vm(_e: u64, out: u64) -> u64 {
    env_enter("get_java_vm");
    if out != 0 {
        unsafe { *(out as *mut usize) = vm_ptr() };
    }
    0
}
extern "C" fn vm_get_env(_vm: u64, out: u64, _ver: u64) -> u64 {
    env_enter("vm_get_env");
    // (*penv) = the JNIEnv handle (cell holding the function table).
    if out != 0 {
        unsafe { *(out as *mut usize) = env_ptr() };
    }
    0
}
extern "C" fn vm_attach(_vm: u64, a: u64, _b: u64) -> u64 {
    env_enter("vm_attach");
    // AttachCurrentThread(vm, &env, args) — single-threaded VM:
    // every thread is the main thread, so just hand back JNIEnv*.
    if a != 0 {
        unsafe { *(a as *mut usize) = env_ptr() };
    }
    0
}

extern "C" fn new_direct_byte_buffer(_e: u64, addr: u64, cap: u64) -> u64 {
    env_enter("new_direct_byte_buffer");
    if !crate::native::guest_dst_ok(addr as usize, cap as usize) {
        eprintln!("[guestptr] new_direct_byte_buffer: {addr:#x}+{cap} not guest/stack/image");
    }
    let mut v = vm();
    let ci = v
        .type_index_of("Ljava/nio/ByteBuffer;")
        .unwrap_or(crate::GX_NONE);
    let mut fields = std::collections::HashMap::new();
    fields.insert(jdk::P_DATA as u64, V::I64(addr as i64));
    fields.insert(jdk::P_FLAGS as u64, V::I64(cap as i64));
    let h = v.heap.alloc(Obj::Host {
        class_idx: ci,
        desc: "Ljava/nio/ByteBuffer;".into(),
        fields,
        state: HostState::Empty,
    });
    h as u64
}
extern "C" fn get_direct_buffer_address(_e: u64, buf: u64) -> u64 {
    env_enter("get_direct_buffer_address");
    jdk::pseudo_get(&vm(), buf as u32, jdk::P_DATA)
        .map(v_to_u64)
        .unwrap_or(0)
}
extern "C" fn get_direct_buffer_capacity(_e: u64, buf: u64) -> u64 {
    env_enter("get_direct_buffer_capacity");
    jdk::pseudo_get(&vm(), buf as u32, jdk::P_FLAGS)
        .map(v_to_u64)
        .unwrap_or(0)
}

// ---------- Java → native dispatch ----------

/// JNI name mangling per spec (short and long forms).
fn mangle(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        match c {
            '/' | '.' => out.push('_'),
            '_' => out.push_str("_1"),
            ';' => out.push_str("_2"),
            '[' => out.push_str("_3"),
            c if c.is_alphanumeric() => out.push(c),
            c => out.push_str(&format!("_0{:04x}", c as u32)),
        }
    }
    out
}

/// Find the native implementation of a dex method in loaded libs:
/// `Java_` symbol first, then the RegisterNatives table.
pub(crate) fn native_addr(vm: &Vm, class: &str, name: &str, sig: &str) -> Option<usize> {
    let cls = class.trim_start_matches('L').trim_end_matches(';');
    let short = format!("Java_{}_{}", mangle(cls), mangle(name));
    let params = split_params(sig).concat();
    let long = format!("{short}__{}", mangle(&params));
    for lib in &vm.natives {
        if let Some(&a) = lib.exports.get(&short).or_else(|| lib.exports.get(&long)) {
            return Some(a);
        }
        if let Some(&a) =
            lib.registered
                .get(&(class.to_string(), name.to_string(), sig.to_string()))
        {
            return Some(a);
        }
    }
    None
}

/// `System.loadLibrary(name)` / `Runtime.load(path)`: pull the arm64
/// `.so` out of the APK, map it, run initializers, then `JNI_OnLoad`.
/// libGDX's loader tries `lib<name>arm64.so` before `lib<name>.so`, so
/// both spellings are candidates; `load()`'s absolute path reduces to
/// its basename inside `lib/arm64-v8a/`.
pub(crate) fn load_native_lib(vm: &mut Vm, arg: &str, absolute: bool) {
    init_tables();
    let cands: Vec<String> = if absolute {
        let base = arg.rsplit('/').next().unwrap_or(arg);
        vec![format!("lib/arm64-v8a/{base}")]
    } else {
        let stripped = arg.strip_suffix("arm64").unwrap_or(arg);
        vec![
            format!("lib/arm64-v8a/lib{arg}.so"),
            format!("lib/arm64-v8a/lib{stripped}.so"),
        ]
    };
    let found = vm
        .apk_res
        .as_ref()
        .and_then(|r| cands.iter().find_map(|p| r.read(p).map(|b| (p.clone(), b))));
    let Some((path, bytes)) = found else {
        eprintln!("[jni] loadLibrary {arg}: no arm64 .so in apk");
        return;
    };
    // libglide-webp's JNI_OnLoad corrupts the host heap (native code
    // shares our address space; crash surfaces in a later host malloc
    // ~50% of runs). PNG thumbnails never reach the webp decoder, so
    // skipping loses nothing; OVELO_SKIP_LIBS adds more lib names
    // (comma-separated substring match), OVELO_NO_SKIP_LIBS=1 disables.
    let skip: Vec<String> = std::env::var("OVELO_SKIP_LIBS")
        .map(|s| {
            s.split(',')
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let denied = if std::env::var_os("OVELO_NO_SKIP_LIBS").is_some() {
        skip.iter().any(|n| path.contains(n.as_str()))
    } else {
        path.contains("glide-webp") || skip.iter().any(|n| path.contains(n.as_str()))
    };
    if denied {
        eprintln!("[jni] loadLibrary {arg}: {path} skipped (denylist)");
        return;
    }
    // DT_NEEDED deps live beside the requested lib in the APK; each
    // dep is mapped before the dependent so its exports are in the
    // global scope when the dependent relocates.
    let libs = {
        let res = vm.apk_res.as_ref();
        if let Some(r) = res {
            crate::native::set_apk_reader(&**r as *const _ as usize);
        }
        let mut lookup = |soname: &str| -> Option<Vec<u8>> {
            res.and_then(|r| r.read(&format!("lib/arm64-v8a/{soname}")))
        };
        match crate::native::load_tree(&path, &bytes, &mut lookup) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("[jni] load_so {path}: {e}");
                return;
            }
        }
    };
    let mut libs = libs;
    let Some(lib) = libs.pop() else {
        // Soname already mapped (e.g. as a dep of an earlier lib) —
        // JNI_OnLoad runs once per image, so nothing to do.
        eprintln!("[jni] loadLibrary {arg}: already loaded");
        return;
    };
    if !libs.is_empty() {
        eprintln!("[jni] deps loaded: {}", libs.len());
        vm.natives.extend(libs);
    }
    let onload = lib.exports.get("JNI_OnLoad").copied();
    eprintln!(
        "[jni] loaded {path} exports={} onload={onload:?}",
        lib.exports.len()
    );
    vm.natives.push(lib);
    if let Some(addr) = onload {
        let iargs = [vm_ptr() as u64, 0, 0, 0, 0, 0, 0, 0];
        let fargs = [0f64; 8];
        let (rc, _) = with_vm(vm, |_| call_native(addr, &iargs, &fargs, &[]));
        eprintln!("[jni] JNI_OnLoad -> {rc:#x}");
    }
}

/// Marshal dex args and call the native function. `iargs[0..2]` are the
/// env + receiver/class — params occupy int regs x2-x7 and float regs
/// d0-d7; anything beyond rides the AAPCS64 stack area (`sargs`, in
/// argument order). Long/double are single 64-bit slots each; a float
/// stack arg keeps its value in the low half of its 8-byte slot.
pub(crate) fn dispatch_native(
    vm: &mut Vm,
    addr: usize,
    class: &str,
    ret: &str,
    is_static: bool,
    args: &[V],
) -> Rt<Option<V>> {
    #[cfg(target_os = "macos")]
    crate::native::check_all_zones("dispatch_native");
    let mut iargs = [0u64; 8];
    let mut fargs = [0f64; 8];
    let mut sargs: Vec<u64> = Vec::new();
    iargs[0] = env_ptr() as u64;
    iargs[1] = if is_static {
        vm.type_index_of(class)
            .map(|ci| vm.class_mirror(ci) as u64)
            .unwrap_or(0)
    } else {
        match args.first() {
            Some(V::Ref(h)) => *h as u64,
            _ => 0,
        }
    };
    let mut ii = 2usize;
    let mut fi = 0usize;
    let params = if is_static {
        args
    } else {
        &args[1.min(args.len())..]
    };
    for v in params {
        match v {
            V::F32(f) => {
                if fi < 8 {
                    fargs[fi] = *f as f64;
                    fi += 1;
                } else {
                    sargs.push(f.to_bits() as u64);
                }
            }
            V::F64(f) => {
                if fi < 8 {
                    fargs[fi] = *f;
                    fi += 1;
                } else {
                    sargs.push(f.to_bits());
                }
            }
            other => {
                if ii < 8 {
                    iargs[ii] = v_to_u64(*other);
                    ii += 1;
                } else {
                    sargs.push(v_to_u64(*other));
                }
            }
        }
    }
    let (x0, d0) = with_vm(vm, |_| call_native(addr, &iargs, &fargs, &sargs));
    Ok(Some(match ret {
        "V" => return Ok(None),
        "Z" | "B" | "C" | "S" | "I" => V::I32(x0 as u32 as i32),
        "J" => V::I64(x0 as i64),
        "F" => V::F32(f64::from_bits(d0) as f32),
        "D" => V::F64(f64::from_bits(d0)),
        _ => V::Ref(x0 as u32),
    }))
}

//! Real ARM64 `.so` loading: mmap the APK's bundled shared library into
//! executable memory, apply dynamic relocations binding imports to host
//! symbols (`dlsym` on libSystem covers libc/libc++/libm basics; a small
//! shim table covers Bionic-only names), run initializers, then
//! `JNI_OnLoad` runs with a real JavaVM (see `jni`).

// This module is inherently unsafe: it maps and executes foreign machine
// code. Safety invariants are documented at each site.
#![allow(unsafe_code)]

use std::collections::HashMap;
use std::ffi::{c_void, CString};
use std::sync::{Arc, Mutex};

use ovelo_error::{ErrorCategory, OveloError, Phase, Result};

fn err(cause: &str) -> OveloError {
    OveloError::new(ErrorCategory::AbiSymbol, Phase::Native, cause)
}
fn errd(cause: &str, detail: impl std::fmt::Display) -> OveloError {
    err(cause).with_detail(detail.to_string())
}

extern "C" {
    fn mmap(
        addr: *mut c_void,
        len: usize,
        prot: i32,
        flags: i32,
        fd: i32,
        off: i64,
    ) -> *mut c_void;
    fn mprotect(addr: *mut c_void, len: usize, prot: i32) -> i32;
    fn munmap(addr: *mut c_void, len: usize) -> i32;
    fn dlsym(handle: *mut c_void, symbol: *const i8) -> *mut c_void;
    fn strlen(s: *const i8) -> usize;
    fn memcpy(d: *mut c_void, s: *const c_void, n: usize) -> *mut c_void;
    fn memset(d: *mut c_void, c: i32, n: usize) -> *mut c_void;
    fn strcpy(d: *mut i8, s: *const i8) -> *mut i8;
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

/// Fake Bionic TCB. Guest `.so`s read Bionic TLS slots off TPIDR_EL0 —
/// most visibly the `__stack_chk_guard` slot at +0x28 that
/// `-fstack-protector` emits into every function prologue/epilogue.
/// Darwin leaves TPIDR_EL0 = 0 (it keeps its own TLS in TPIDRRO_EL0),
/// so without a swap every protected guest function faults on its
/// first TLS read. The block is zeroed apart from a self pointer
/// (slot 0) and a fixed stack-guard cookie (slot 5, +0x28) — the same
/// value reads back at function exit, so the check passes.
#[cfg(target_arch = "aarch64")]
static mut GUEST_TCB: [u64; 64] = {
    let mut t = [0u64; 64];
    t[5] = 0x9e3779b97f4a7c15; // arbitrary but nonzero stack guard
    t
};

/// Run `f` with TPIDR_EL0 pointing at [`GUEST_TCB`], then restore the
/// host thread pointer. Must wrap every entry into guest-native code
/// (JNI_OnLoad, DT_INIT*, exported natives).
#[cfg(target_arch = "aarch64")]
pub(crate) fn with_guest_tls<R>(f: impl FnOnce() -> R) -> R {
    unsafe {
        let tcb = std::ptr::addr_of_mut!(GUEST_TCB);
        (*tcb)[0] = tcb as usize as u64; // TLS_SLOT_SELF
        let mut old: usize;
        core::arch::asm!("mrs {}, tpidr_el0", out(reg) old);
        core::arch::asm!("msr tpidr_el0, {}", in(reg) tcb as usize);
        let r = f();
        core::arch::asm!("msr tpidr_el0, {}", in(reg) old);
        r
    }
}

#[cfg(not(target_arch = "aarch64"))]
pub(crate) fn with_guest_tls<R>(f: impl FnOnce() -> R) -> R {
    f()
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
                let insn = if inlib { *(pc as *const u32) } else { 0 };
                let mut msg = [0u8; 512];
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
                // Guest frame chain: fp → [prev_fp, saved_lr] — best-effort
                // backtrace of the guest/native call path. Each lr is
                // dladdr-resolved to <image>+<off> so the owning module
                // (our binary vs a guest .so) is unambiguous.
                let mut f = fp;
                for _ in 0..6 {
                    if f == 0 || !f.is_multiple_of(8) {
                        break;
                    }
                    let nf = *(f as *const usize);
                    let lr = *((f + 8) as *const usize);
                    let mut di: DlInfo = std::mem::zeroed();
                    if dladdr(lr as *const c_void, &mut di) != 0 && !di.dli_fname.is_null() {
                        let off = lr - di.dli_fbase as usize;
                        let mut p = di.dli_fname;
                        let mut end = p;
                        while *end != 0 { end = end.add(1) }
                        let mut last = p;
                        while p != end { if *p == b'/' as i8 { last = p.add(1) } p = p.add(1) }
                        msg[n] = b'<'; n += 1;
                        let l = (end as usize - last as usize).min(40);
                        std::ptr::copy_nonoverlapping(last as *const u8, msg.as_mut_ptr().add(n), l);
                        n += l;
                        msg[n] = b'+'; n += 1;
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
                msg[n] = b'\n';
                write(2, msg.as_ptr() as *const c_void, n + 1);
                _exit(139);
            }
        }
        let sa = Sigaction {
            sa_sigaction: on_segv as *const () as usize,
            sa_mask: 0,
            sa_flags: 0x40, // SA_SIGINFO
        };
        sigaction(11, &sa, std::ptr::null_mut());
        sigaction(10, &sa, std::ptr::null_mut()); // SIGBUS — writes to non-writable guest pages
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
        // report this lib's phdrs after the image is unmapped.
        PHDR_LIBS.lock().unwrap().retain(|l| l.bias != self.bias);
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
extern "C" fn sh_cxa_atexit(
    _f: *mut c_void,
    _a: *mut c_void,
    _d: *mut c_void,
) -> i32 {
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
extern "C" fn sh_strcpy_chk(d: *mut i8, s: *const i8, dlen: usize) -> *mut i8 {
    unsafe {
        if strlen(s) + 1 > dlen {
            sh_chk_fail();
        }
        strcpy(d, s)
    }
}
extern "C" fn sh_memcpy_chk(d: *mut c_void, s: *const c_void, n: usize, dlen: usize) -> *mut c_void {
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

extern "C" fn sh_dl_iterate_phdr(cb: *const c_void, data: *mut c_void) -> i32 {
    if cb.is_null() {
        return -1;
    }
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
        let r = unsafe {
            cb(
                &mut info as *mut _ as *mut c_void,
                std::mem::size_of::<DlPhdrInfo>(),
                data,
            )
        };
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
        eprintln!("[native] abort-msg: {}",
            std::ffi::CStr::from_ptr(s).to_string_lossy());
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
            0
        }
        _ => -1,
    }
}

extern "C" fn sh_ab_unlock_pixels(_env: usize, _bitmap: usize) -> i32 {
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

fn builtin_shim(name: &str) -> Option<usize> {
    Some(match name {
        "__cxa_pure_virtual" => sh_pure_virtual as *const () as usize,
        "__stack_chk_fail" => sh_stack_chk_fail as *const () as usize,
        "__cxa_finalize" => sh_cxa_finalize as *const () as usize,
        "__cxa_atexit" => sh_cxa_atexit as *const () as usize,
        "atoi" | "strtol" => sh_atoi as *const () as usize,
        "sincosf" => sh_sincosf as *const () as usize,
        "sincos" => sh_sincos as *const () as usize,
        "__strlen_chk" => sh_strlen_chk as *const () as usize,
        "__strcpy_chk" => sh_strcpy_chk as *const () as usize,
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

/// Parse + map + relocate an ELF64 ET_DYN aarch64 image.
pub fn load_so(name: &str, elf: &[u8]) -> Result<NativeLib> {
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
    // A raw guest-vaddr-derived pointer is legal only inside the
    // mapping — relocations and init pointers must all pass this.
    let in_img = |a: usize, n: usize| a >= map as usize && a + n <= map_end;

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
    let sym_value = |i: usize| -> usize { unsafe { ((d.symtab + i * 24 + 8) as *const u64).read() as usize } };
    let sym_bind = |i: usize| -> u8 { (unsafe { ((d.symtab + i * 24 + 4) as *const u8).read() }) >> 4 };

    // Export map (defined GLOBAL/WEAK/DEFAULT symbols).
    let mut exports: HashMap<String, usize> = HashMap::new();
    for i in 0..sym_count {
        if sym_shndx(i) == SHN_UNDEF {
            continue;
        }
        exports.insert(sym_name(i), bias + sym_value(i));
    }

    // Import resolution: own exports → shim table → host dlsym.
    let resolve = |i: usize, exports: &HashMap<String, usize>| -> Result<usize> {
        let name = sym_name(i);
        // strip ELF version suffix (`name@LIBC` handled by dynstr already
        // carrying plain name; version refs live in .gnu.version).
        if let Some(&a) = exports.get(&name) {
            return Ok(a);
        }
        if let Some(a) = builtin_shim(&name) {
            return Ok(a);
        }
        if let Some(a) = host_dlsym(&name) {
            return Ok(a);
        }
        // Weak undefined symbols resolve to 0 (legal — the callee checks).
        if sym_bind(i) == STB_WEAK {
            return Ok(0);
        }
        Err(errd("elf.unresolved", name.to_string()))
    };

    let apply = |_d: &Dyn, table: usize, bytes: usize, entsize: usize, exports: &HashMap<String, usize>| -> Result<()> {
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
                R_AARCH64_RELATIVE => unsafe {
                    loc.write((bias as i64 + r_add) as u64)
                },
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

    apply(&d, d.rela, d.relasz, 24, &exports)?;
    apply(&d, d.jmprel, d.pltrelsz, 24, &exports)?;
    apply(&d, d.android_rela, d.android_relasz, 24, &exports)?;
    // Android packed REL (REL, no addend: *loc = base + *loc).
    let n_rel = d.android_relsz / 8;
    for k in 0..n_rel {
        let r = d.android_rel + k * 8;
        let (r_off, r_info) = unsafe { ((r as *const u64).read() as usize, ((r as *const u64).add(1)).read()) };
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
                let s = resolve((r_info >> 32) as usize, &exports)?;
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
    for &(va, memsz, flags) in &segs {
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
            unsafe { munmap(map, span) };
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
        unsafe { munmap(map, span) };
        return Err(err("elf.init"));
    }
    // Register with dl_iterate_phdr BEFORE the initializers run —
    // unwind-table registration (e.g. glide-webp's) walks the phdr
    // list during DT_INIT and must find this lib. The phdr copy is
    // leaked so the reported pointer stays valid for the process.
    if let Ok(name_c) = CString::new(name) {
        if let Some(raw) = elf.get(phoff..phoff + phentsize * phnum) {
            PHDR_LIBS.lock().unwrap().push(Arc::new(PhdrLib {
                bias,
                name: name_c,
                phdrs: raw.into(),
                phnum: phnum.try_into().unwrap_or(u16::MAX),
            }));
        }
    }

    if d.init != 0 {
        let f: extern "C" fn() = unsafe { std::mem::transmute(d.init) };
        with_guest_tls(|| f());
    }
    let n_init = if in_img(d.init_array, d.init_array_sz) {
        d.init_array_sz / 8
    } else {
        0
    };
    for k in 0..n_init {
        let f: extern "C" fn() = unsafe { std::mem::transmute(((d.init_array + k * 8) as *const u64).read()) };
        with_guest_tls(|| f());
    }

    Ok(NativeLib {
        map: map as *mut u8,
        span,
        bias,
        exports,
        registered: HashMap::new(),
        name: name.to_string(),
    })
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
            let mut chain = (gh + 16 + bloom_size * 8 + nbuckets * 4 + (si - symoffset) * 4) as *const u32;
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

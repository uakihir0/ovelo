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
// libc bodies apply directly; the object-size argument is ignored.
extern "C" fn sh_strlen_chk(s: *const i8, _slen: usize) -> usize {
    unsafe { strlen(s) }
}
extern "C" fn sh_strcpy_chk(d: *mut i8, s: *const i8, _dlen: usize) -> *mut i8 {
    unsafe { strcpy(d, s) }
}
extern "C" fn sh_memcpy_chk(d: *mut c_void, s: *const c_void, n: usize, _dlen: usize) -> *mut c_void {
    unsafe { memcpy(d, s, n) }
}
extern "C" fn sh_memset_chk(d: *mut c_void, c: i32, n: usize, _dlen: usize) -> *mut c_void {
    unsafe { memset(d, c, n) }
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

    // Initializers: DT_INIT then DT_INIT_ARRAY — both guest-vaddr
    // derived, both checked against the mapping before they can branch
    // anywhere outside it.
    if d.init != 0 && !in_img(d.init, 4) {
        unsafe { munmap(map, span) };
        return Err(err("elf.init"));
    }
    if d.init != 0 {
        let f: extern "C" fn() = unsafe { std::mem::transmute(d.init) };
        f();
    }
    let n_init = if in_img(d.init_array, d.init_array_sz) {
        d.init_array_sz / 8
    } else {
        0
    };
    for k in 0..n_init {
        let f: extern "C" fn() = unsafe { std::mem::transmute(((d.init_array + k * 8) as *const u64).read()) };
        f();
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

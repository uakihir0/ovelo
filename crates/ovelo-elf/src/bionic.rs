//! Bionic shim skeleton (design doc §05 phase 2).
//!
//! What this provides now:
//! * Per-thread `errno` and TLS slots — Android's Bionic keeps `errno` in the
//!   TLS area (`TLS_SLOT_ERRNO`); Apple keeps `x18` reserved and uses a
//!   different `tpidrro_el0` layout, so we cannot adopt Bionic's register
//!   conventions directly. The shim instead owns an explicit per-thread state
//!   object the runtime threads through calls.
//! * The fixed-argument integer memory/string functions (`memcpy`, `memset`,
//!   `strlen`, `strcmp`, `memmove`) implemented against the guest [`Image`].
//!
//! Deliberate TODOs (documented ABI differences, not bugs):
//! * Variadic functions (`printf` family, `open`, ...) — ARM64 variadic ABI
//!   spills named arguments differently; needs a trampoline that repacks the
//!   va_list. Not started.
//! * `x18` ShadowCallStack vs Apple-reserved — on macOS `x18` must not be
//!   used by host code, but Bionic-compiled objects treat it as the SCS
//!   pointer. The future JIT/JNI trampoline must save/restore or mask it.
//! * TLS layout — Bionic's TLS slot array (keys, stack guard, sanitizer)
//!   lives at a fixed negative offset from `tpidr_el0`; we model slots as a
//!   map for now and will need the real layout once pthread/emutls arrive.
//! * Syscalls — Bionic inlines raw syscalls (`svc #0`) in many paths; those
//!   must trap into the broker, not reach the host kernel.

use crate::{err, Image};
use ovelo_error::Result;
use std::collections::HashMap;

/// `TLS_SLOT_ERRNO` in Bionic's TLS array.
pub const TLS_SLOT_ERRNO: u32 = 0;

/// Synthetic shim function table: allowlisted `libc.so` imports resolve to
/// these vaddrs until real trampolines exist (§05 phase 2).
const SHIM_BASE: u64 = 0x7f00_0000_0000;
const SHIM_FNS: &[&str] = &[
    "memcpy", "memmove", "memset", "strlen", "strcmp", "__errno",
];

/// Shim entry address for an exported Bionic symbol, if implemented.
pub fn shim_addr(name: &str) -> Option<u64> {
    SHIM_FNS
        .iter()
        .position(|&f| f == name)
        .map(|i| SHIM_BASE + i as u64 * 0x10)
}

/// Per-thread Bionic state owned by the shim.
#[derive(Default)]
pub struct Bionic {
    errno: i32,
    tls: HashMap<u32, u64>,
}

impl Bionic {
    /// Fresh per-thread state (errno = 0, empty TLS slots).
    pub fn new() -> Self {
        Self::default()
    }

    /// Current `errno` value.
    pub fn errno(&self) -> i32 {
        self.errno
    }
    /// Set `errno` and mirror it into `TLS_SLOT_ERRNO`.
    pub fn set_errno(&mut self, v: i32) {
        self.errno = v;
        self.tls.insert(TLS_SLOT_ERRNO, v as u64);
    }
    /// Read a TLS slot (0 when unset).
    pub fn tls_slot(&self, slot: u32) -> u64 {
        self.tls.get(&slot).copied().unwrap_or(0)
    }
    /// Write a TLS slot.
    pub fn set_tls_slot(&mut self, slot: u32, v: u64) {
        self.tls.insert(slot, v);
    }

    /// `void *memcpy(void *dst, const void *src, size_t n) -> dst`.
    pub fn memcpy(&mut self, image: &mut Image, dst: u64, src: u64, n: u64) -> Result<u64> {
        let mut buf = vec![0u8; n as usize];
        image.read(src, &mut buf)?;
        image.write(dst, &buf)?;
        Ok(dst)
    }

    /// `void *memmove(...)` — overlap-safe.
    pub fn memmove(&mut self, image: &mut Image, dst: u64, src: u64, n: u64) -> Result<u64> {
        self.memcpy(image, dst, src, n) // snapshot into buf makes it overlap-safe
    }

    /// `void *memset(void *dst, int c, size_t n) -> dst`.
    pub fn memset(&mut self, image: &mut Image, dst: u64, c: u64, n: u64) -> Result<u64> {
        image.write(dst, &vec![c as u8; n as usize])?;
        Ok(dst)
    }

    /// `size_t strlen(const char *s)`.
    pub fn strlen(&self, image: &Image, s: u64) -> Result<u64> {
        let mut n = 0u64;
        loop {
            let mut b = [0u8; 1];
            image.read(s + n, &mut b)?;
            if b[0] == 0 {
                return Ok(n);
            }
            n += 1;
            if n > (1 << 26) {
                return Err(err("bionic.strlen.unbounded"));
            }
        }
    }

    /// `int strcmp(const char *a, const char *b)`.
    pub fn strcmp(&self, image: &Image, a: u64, b: u64) -> Result<i32> {
        let mut n = 0u64;
        loop {
            let (mut x, mut y) = ([0u8; 1], [0u8; 1]);
            image.read(a + n, &mut x)?;
            image.read(b + n, &mut y)?;
            if x[0] != y[0] {
                return Ok(x[0] as i32 - y[0] as i32);
            }
            if x[0] == 0 {
                return Ok(0);
            }
            n += 1;
            if n > (1 << 26) {
                return Err(err("bionic.strcmp.unbounded"));
            }
        }
    }

    /// Look up an implemented shim by export name (for `libc.so` resolution).
    /// Unknown names return `None` so the caller can raise `AbiSymbol` for
    /// imports Bionic does not yet cover.
    pub fn provides(name: &str) -> bool {
        SHIM_FNS.contains(&name)
    }
}

//! AArch64 dynamic relocations.
//!
//! Supported: `R_AARCH64_RELATIVE` (1027), `R_AARCH64_GLOB_DAT` (1025),
//! `R_AARCH64_JUMP_SLOT` (1026) and RELR packed relocations (decoded as
//! RELATIVE). `R_AARCH64_NONE`/`COPY`/`ABS64`... — anything else is rejected
//! with `AbiSymbol` before any initializer runs (design doc §05).

use crate::{bionic, errd, ElfFile, Image, PT_LOAD};
use ovelo_error::Result;

const R_NONE: u32 = 0;
const R_AARCH64_GLOB_DAT: u32 = 1025;
const R_AARCH64_JUMP_SLOT: u32 = 1026;
const R_AARCH64_RELATIVE: u32 = 1027;

/// Relocation flavor after decoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelocKind {
    /// `R_AARCH64_NONE` — skip.
    None,
    /// `B + A` (RELA addend) — also used for decoded RELR slots.
    Relative,
    /// `S + A`: resolve symbol in loaded objects.
    GlobDat,
    /// `S`: resolve symbol in loaded objects (lazy PLT entry).
    JumpSlot,
    /// Implicit addend (`*slot + B`) — RELR entries.
    Relr,
}

/// One decoded relocation site.
#[derive(Debug, Clone)]
pub struct Reloc {
    /// Virtual offset of the 8-byte slot.
    pub offset: u64,
    /// Symbol table index (0 for RELATIVE/RELR).
    pub sym: u32,
    /// RELA addend (0 for RELR).
    pub addend: i64,
    rtype: u32,
    relr: bool,
}

impl Reloc {
    pub(crate) fn rela(offset: u64, info: u64, addend: i64) -> Self {
        Self {
            offset,
            sym: (info >> 32) as u32,
            addend,
            rtype: info as u32,
            relr: false,
        }
    }
    pub(crate) fn relr(offset: u64) -> Self {
        Self {
            offset,
            sym: 0,
            addend: 0,
            rtype: R_AARCH64_RELATIVE,
            relr: true,
        }
    }

    /// Map the raw type to a supported kind, rejecting unknown types.
    pub fn kind(&self) -> Result<RelocKind> {
        if self.relr {
            return Ok(RelocKind::Relr);
        }
        match self.rtype {
            R_NONE => Ok(RelocKind::None),
            R_AARCH64_RELATIVE => Ok(RelocKind::Relative),
            R_AARCH64_GLOB_DAT => Ok(RelocKind::GlobDat),
            R_AARCH64_JUMP_SLOT => Ok(RelocKind::JumpSlot),
            t => Err(errd(
                "elf.reloc.unsupported",
                format!("r_type {t} at {:#x}", self.offset),
            )),
        }
    }

    /// Apply this relocation against mapped images.
    pub fn apply(
        &self,
        image: &mut Image,
        bias: u64,
        elf: &ElfFile<'_>,
        sym_count: usize,
        global: &std::collections::HashMap<String, u64>,
        exports: &std::collections::HashMap<String, u64>,
    ) -> Result<()> {
        // A no-op relocation writes nothing: accept it even at offset zero
        // or outside the load segments (some linkers emit padding entries).
        if self.kind()? == RelocKind::None {
            return Ok(());
        }
        // The relocation destination must lie inside this object's own
        // PT_LOAD segments — never another object's memory (and never
        // unmapped space); `write_raw` bypasses page perms, so the check
        // here is the only ownership guard.
        let in_own = elf.phdrs.iter().any(|p| {
            p.p_type == PT_LOAD && self.offset >= p.vaddr && self.offset + 8 <= p.vaddr + p.memsz
        });
        if !in_own {
            return Err(errd(
                "elf.reloc.bounds",
                format!("offset {:#x}", self.offset),
            ));
        }
        let slot = bias + self.offset;
        match self.kind()? {
            RelocKind::None => Ok(()), // unreachable — returned above
            RelocKind::Relative => {
                image.write_raw(slot, &(bias.wrapping_add(self.addend as u64)).to_le_bytes())
            }
            RelocKind::Relr => {
                let cur = image.read_u64(slot)?;
                image.write_raw(slot, &cur.wrapping_add(bias).to_le_bytes())
            }
            RelocKind::GlobDat | RelocKind::JumpSlot => {
                let name = elf.symbol_name(self.sym, sym_count)?;
                let value = exports
                    .get(&name)
                    .or_else(|| global.get(&name))
                    .copied()
                    .or_else(|| bionic::shim_addr(&name))
                    .ok_or_else(|| errd("elf.symbol.undefined", name.clone()))?;
                let a = if self.kind()? == RelocKind::GlobDat {
                    self.addend as u64
                } else {
                    0
                };
                image.write_raw(slot, &value.wrapping_add(a).to_le_bytes())
            }
        }
    }
}

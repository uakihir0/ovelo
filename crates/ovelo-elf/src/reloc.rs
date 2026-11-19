//! AArch64 dynamic relocations.
//!
//! Supported: `R_AARCH64_RELATIVE` (1027), `R_AARCH64_GLOB_DAT` (1025),
//! `R_AARCH64_JUMP_SLOT` (1026) and RELR packed relocations (decoded as
//! RELATIVE). `R_AARCH64_NONE`/`COPY`/`ABS64`... — anything else is rejected
//! with `AbiSymbol` before any initializer runs (design doc §05).

use crate::{errd, ElfFile, Image, LoadedSo};
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
        loaded: &std::collections::HashMap<String, LoadedSo<'_>>,
        exports: &std::collections::HashMap<String, u64>,
    ) -> Result<()> {
        let slot = bias + self.offset;
        match self.kind()? {
            RelocKind::None => Ok(()),
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
                    .or_else(|| loaded.values().find_map(|s| s.exports.get(&name)))
                    .copied()
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

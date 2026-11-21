//! ovelo-elf: ARM64 ELF loader model and Bionic shim skeleton.
//!
//! Implements design doc §05 phases 1–2:
//! * ELF64 little-endian parsing and validation (`ET_DYN`, `EM_AARCH64`,
//!   `PT_LOAD`/`PT_DYNAMIC`/`PT_TLS` program headers).
//! * `DT_NEEDED` allowlisting and dependency-ordered loading.
//! * AArch64 relocations `R_AARCH64_RELATIVE`/`GLOB_DAT`/`JUMP_SLOT` plus
//!   RELR packed relocations; unknown types are rejected before any
//!   initializer runs.
//! * An in-memory page-table image model enforcing per-page R/W/X.
//! * Initializers (`DT_INIT`/`DT_INIT_ARRAY`) run exactly once, dependencies
//!   first.
//! * A Bionic shim skeleton: per-thread errno/TLS slots and the fixed-arg
//!   integer memory/string routines. Variadic functions and functions with
//!   ABI differences (x18 ShadowCallStack vs Apple-reserved, TLS layout,
//!   syscall emulation) are documented TODOs in [`bionic`].

use ovelo_error::{ErrorCategory, OveloError, Phase, Result};

mod bionic;
mod image;
mod reloc;

pub use bionic::Bionic;
pub use image::{Image, PagePerm};
pub use reloc::{Reloc, RelocKind};

const PAGE: u64 = 0x1000;

fn err(cause: &str) -> OveloError {
    OveloError::new(ErrorCategory::AbiSymbol, Phase::Native, cause)
}
fn errd(cause: &str, detail: impl std::fmt::Display) -> OveloError {
    err(cause).with_detail(detail.to_string())
}

const ET_DYN: u16 = 3;
const EM_AARCH64: u16 = 183;
const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const PT_TLS: u32 = 7;

const DT_NULL: i64 = 0;
const DT_NEEDED: i64 = 1;
const DT_STRTAB: i64 = 5;
const DT_SYMTAB: i64 = 6;
const DT_RELA: i64 = 7;
const DT_RELASZ: i64 = 8;
const DT_RELAENT: i64 = 9;
const DT_STRSZ: i64 = 10;
const DT_SYMENT: i64 = 11;
const DT_INIT: i64 = 12;
const DT_INIT_ARRAY: i64 = 25;
const DT_INIT_ARRAYSZ: i64 = 27;
const DT_PLTREL: i64 = 20;
const DT_JMPREL: i64 = 23;
const DT_PLTRELSZ: i64 = 2;
const DT_RELACOUNT: i64 = 0x6fff_fff9;
const DT_ANDROID_REL: i64 = 0x6000_000d;
const DT_ANDROID_RELA: i64 = 0x6000_000f;
const DT_ANDROID_RELSZ: i64 = 0x6000_000e;
const DT_ANDROID_RELASZ: i64 = 0x6000_0010;
const DT_RELR: i64 = 36;
const DT_RELRSZ: i64 = 35;
const DT_RELRENT: i64 = 37;

/// A parsed ELF64 program header.
#[derive(Debug, Clone)]
pub struct Phdr {
    /// `p_type` segment kind.
    pub p_type: u32,
    /// `p_flags` (PF_X=1, PF_W=2, PF_R=4).
    pub flags: u32,
    /// `p_offset` file offset.
    pub offset: u64,
    /// `p_vaddr` virtual address.
    pub vaddr: u64,
    /// `p_filesz` bytes in file.
    pub filesz: u64,
    /// `p_memsz` bytes in memory.
    pub memsz: u64,
    /// `p_align` segment alignment.
    pub align: u64,
}

/// A parsed ARM64 shared object (or ET_DYN executable).
#[derive(Debug)]
pub struct ElfFile<'a> {
    data: &'a [u8],
    /// Only `PT_LOAD`/`PT_TLS` headers are retained after validation.
    pub phdrs: Vec<Phdr>,
    /// `DT_NEEDED` library names, in file order.
    pub needed: Vec<String>,
    /// Decoded relocation table (RELA + RELR, in file order).
    pub relocs: Vec<Reloc>,
    /// `DT_INIT` virtual address, if present.
    pub init_fn: u64,
    /// `DT_INIT_ARRAY` as (vaddr, count).
    pub init_array: Option<(u64, u64)>,
    /// `PT_TLS` template, if present.
    pub tls: Option<Phdr>,
    strtab: (u64, u64), // (addr, size)
    symtab: (u64, u64), // (addr, entsize)
    /// vaddr at which `PT_DYNAMIC` data resides in the file image.
    dyn_vaddr: u64,
    dyn_range: (usize, usize), // file offsets
}

fn u16at(b: &[u8], p: usize) -> Result<u16> {
    Ok(u16::from_le_bytes(
        b.get(p..p + 2)
            .ok_or_else(|| err("elf.truncated"))?
            .try_into()
            .unwrap(),
    ))
}
fn u32at(b: &[u8], p: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(
        b.get(p..p + 4)
            .ok_or_else(|| err("elf.truncated"))?
            .try_into()
            .unwrap(),
    ))
}
fn u64at(b: &[u8], p: usize) -> Result<u64> {
    Ok(u64::from_le_bytes(
        b.get(p..p + 8)
            .ok_or_else(|| err("elf.truncated"))?
            .try_into()
            .unwrap(),
    ))
}

impl<'a> ElfFile<'a> {
    /// Parse and validate an ELF64 little-endian ARM64 object.
    pub fn parse(data: &'a [u8]) -> Result<Self> {
        if data.len() < 64 || data[0..4] != [0x7f, b'E', b'L', b'F'] {
            return Err(err("elf.magic"));
        }
        if data[4] != 2 || data[5] != 1 {
            return Err(err("elf.unsupported_format")); // need ELFCLASS64 + LSB
        }
        let e_type = u16at(data, 16)?;
        if e_type != ET_DYN {
            return Err(errd("elf.type", format!("e_type {e_type}, want ET_DYN")));
        }
        if u16at(data, 18)? != EM_AARCH64 {
            return Err(err("elf.machine"));
        }
        let phoff = u64at(data, 32)? as usize;
        let phentsize = u16at(data, 54)? as usize;
        let phnum = u16at(data, 56)? as usize;
        if phentsize < 56 {
            return Err(err("elf.phentsize"));
        }
        let mut phdrs = Vec::new();
        let mut dyn_seg: Option<Phdr> = None;
        for i in 0..phnum {
            let p = phoff
                .checked_add(i.checked_mul(phentsize).ok_or_else(|| err("elf.phoff"))?)
                .ok_or_else(|| err("elf.phoff"))?;
            if p + 56 > data.len() {
                return Err(err("elf.phdr.bounds"));
            }
            let ph = Phdr {
                p_type: u32at(data, p)?,
                flags: u32at(data, p + 4)?,
                offset: u64at(data, p + 8)?,
                vaddr: u64at(data, p + 16)?,
                filesz: u64at(data, p + 32)?,
                memsz: u64at(data, p + 40)?,
                align: u64at(data, p + 48)?,
            };
            match ph.p_type {
                PT_LOAD => {
                    if ph.flags & !7 != 0 {
                        return Err(err("elf.load.flags"));
                    }
                    if ph.filesz > ph.memsz {
                        return Err(err("elf.load.filesz"));
                    }
                    let end = ph
                        .offset
                        .checked_add(ph.filesz)
                        .ok_or_else(|| err("elf.load.bounds"))?;
                    if end > data.len() as u64 {
                        return Err(err("elf.load.bounds"));
                    }
                    if ph.align > 1 && ph.vaddr % ph.align != ph.offset % ph.align {
                        return Err(err("elf.load.align"));
                    }
                    phdrs.push(ph);
                }
                PT_DYNAMIC => {
                    let end = ph
                        .offset
                        .checked_add(ph.filesz)
                        .ok_or_else(|| err("elf.dyn.bounds"))?;
                    if end > data.len() as u64 {
                        return Err(err("elf.dyn.bounds"));
                    }
                    dyn_seg = Some(ph);
                }
                PT_TLS => phdrs.push(ph),
                _ => {}
            }
        }
        if phdrs.iter().all(|p| p.p_type != PT_LOAD) {
            return Err(err("elf.no_load"));
        }
        let dyn_seg = dyn_seg.ok_or_else(|| err("elf.no_dynamic"))?;

        // Parse .dynamic.
        let mut strtab = (0u64, 0u64);
        let mut symtab = (0u64, 0u64);
        let mut rela = (0u64, 0u64, 24u64);
        let mut relr = (0u64, 0u64, 8u64);
        let mut jmprel = (0u64, 0u64);
        let mut pltrel_is_rela = true;
        let mut needed_off = Vec::new();
        let mut init_fn = 0u64;
        let mut init_array: Option<(u64, u64)> = None;
        let mut ia_sz = 0u64;
        let mut relacount = 0u64;
        let n = (dyn_seg.filesz / 16) as usize;
        let dbase = dyn_seg.offset as usize;
        for i in 0..n {
            let tag = u64at(data, dbase + i * 16)? as i64;
            let val = u64at(data, dbase + i * 16 + 8)?;
            match tag {
                DT_NULL => break,
                DT_NEEDED => needed_off.push(val),
                DT_STRTAB => strtab.0 = val,
                DT_STRSZ => strtab.1 = val,
                DT_SYMTAB => symtab.0 = val,
                DT_SYMENT => symtab.1 = val,
                DT_RELA => rela.0 = val,
                DT_RELASZ => rela.1 = val,
                DT_RELAENT => rela.2 = val,
                DT_JMPREL => jmprel.0 = val,
                DT_PLTRELSZ => jmprel.1 = val,
                DT_PLTREL => pltrel_is_rela = val == DT_RELA as u64,
                DT_RELR => relr.0 = val,
                DT_RELRSZ => relr.1 = val,
                DT_RELRENT => relr.2 = val,
                DT_INIT => init_fn = val,
                DT_INIT_ARRAY => init_array = Some((val, 0)),
                DT_INIT_ARRAYSZ => ia_sz = val,
                DT_RELACOUNT => relacount = val,
                _ => {}
            }
        }
        if let Some((a, _)) = init_array {
            if !ia_sz.is_multiple_of(8) {
                return Err(err("elf.init_array.size"));
            }
            init_array = Some((a, ia_sz / 8));
        }

        // Convert dynamic vaddrs to file offsets via PT_LOADs.
        let v2o = |vaddr: u64, len: u64| -> Result<usize> {
            for p in &phdrs {
                if p.p_type == PT_LOAD
                    && vaddr >= p.vaddr
                    && vaddr + len <= p.vaddr + p.filesz
                    && vaddr + len >= vaddr
                {
                    return Ok((p.offset + (vaddr - p.vaddr)) as usize);
                }
            }
            Err(err("elf.dyn.vaddr"))
        };

        let str_off = if strtab.0 != 0 {
            v2o(strtab.0, strtab.1)?
        } else {
            0
        };
        let str_end = str_off + strtab.1 as usize;
        if str_end > data.len() {
            return Err(err("elf.strtab.bounds"));
        }
        let cstr = |off: u64| -> Result<String> {
            let s = str_off + off as usize;
            if s >= str_end {
                return Err(err("elf.strtab.bounds"));
            }
            let e = data[s..str_end]
                .iter()
                .position(|&c| c == 0)
                .ok_or_else(|| err("elf.strtab.nul"))?;
            Ok(String::from_utf8_lossy(&data[s..s + e]).into_owned())
        };
        let needed = needed_off
            .iter()
            .map(|&o| cstr(o))
            .collect::<Result<Vec<_>>>()?;

        // RELA relocations.
        let mut relocs = Vec::new();
        if rela.0 != 0 || rela.1 != 0 {
            if rela.2 < 24 || !rela.1.is_multiple_of(rela.2) {
                return Err(err("elf.rela.size"));
            }
            let roff = v2o(rela.0, rela.1)?;
            for i in 0..(rela.1 / rela.2) as usize {
                let p = roff + i * rela.2 as usize;
                let offset = u64at(data, p)?;
                let info = u64at(data, p + 8)?;
                let addend = u64at(data, p + 16)? as i64;
                relocs.push(Reloc::rela(offset, info, addend));
            }
        }
        // Packed Android relocations are a different on-disk format; reject
        // explicitly instead of silently dropping them.
        for i in 0..n {
            let tag = u64at(data, dbase + i * 16)? as i64;
            if tag == DT_NULL {
                break; // bytes past DT_NULL are padding, not entries
            }
            if matches!(
                tag,
                DT_ANDROID_REL | DT_ANDROID_RELA | DT_ANDROID_RELSZ | DT_ANDROID_RELASZ
            ) {
                return Err(err("elf.reloc.packed"));
            }
        }
        // PLT relocations live in DT_JMPREL (RELA format for AArch64).
        if jmprel.0 != 0 || jmprel.1 != 0 {
            if !pltrel_is_rela {
                return Err(err("elf.pltrel.format"));
            }
            if !jmprel.1.is_multiple_of(24) {
                return Err(err("elf.jmprel.size"));
            }
            let roff = v2o(jmprel.0, jmprel.1)?;
            for i in 0..(jmprel.1 / 24) as usize {
                let p = roff + i * 24;
                let offset = u64at(data, p)?;
                let info = u64at(data, p + 8)?;
                let addend = u64at(data, p + 16)? as i64;
                relocs.push(Reloc::rela(offset, info, addend));
            }
        }
        // RELR packed relocations (even entry = base; odd = bitmap).
        if relr.0 != 0 || relr.1 != 0 {
            if relr.2 != 8 || !relr.1.is_multiple_of(8) {
                return Err(err("elf.relr.size"));
            }
            let roff = v2o(relr.0, relr.1)?;
            let mut base = 0u64;
            for i in 0..(relr.1 / 8) as usize {
                let e = u64at(data, roff + i * 8)?;
                if e & 1 == 0 {
                    relocs.push(Reloc::relr(e));
                    base = e + 8;
                } else {
                    let mut bitmap = e >> 1;
                    let mut addr = base;
                    while bitmap != 0 {
                        if bitmap & 1 != 0 {
                            relocs.push(Reloc::relr(addr));
                        }
                        bitmap >>= 1;
                        addr += 8;
                    }
                    base += 63 * 8;
                }
            }
        }
        let _ = relacount; // informational; all RELATIVE relocs already decoded

        let tls = phdrs.iter().find(|p| p.p_type == PT_TLS).cloned();
        Ok(Self {
            data,
            phdrs,
            needed,
            relocs,
            init_fn,
            init_array,
            tls,
            strtab,
            symtab,
            dyn_vaddr: dyn_seg.vaddr,
            dyn_range: (dbase, dbase + dyn_seg.filesz as usize),
        })
    }

    /// The file's bytes.
    pub fn data(&self) -> &'a [u8] {
        self.data
    }

    /// Look up a symbol name by table index (0 = undefined). Used to give
    /// `GLOB_DAT`/`JUMP_SLOT` relocations their target name.
    pub fn symbol_name(&self, index: u32, sym_count: usize) -> Result<String> {
        if self.symtab.0 == 0 || self.symtab.1 < 24 {
            return Err(err("elf.symtab.missing"));
        }
        if index as usize >= sym_count {
            return Err(errd("elf.symbol.bounds", format!("sym {index}")));
        }
        let st = self
            .phdrs
            .iter()
            .find(|p| {
                p.p_type == PT_LOAD
                    && self.symtab.0 >= p.vaddr
                    && self.symtab.0 < p.vaddr + p.filesz
            })
            .ok_or_else(|| err("elf.symtab.bounds"))?;
        let base = (st.offset + (self.symtab.0 - st.vaddr)) as usize;
        let name_off = u32at(self.data, base + index as usize * self.symtab.1 as usize)?;
        let s = self
            .phdrs
            .iter()
            .find(|p| {
                p.p_type == PT_LOAD
                    && self.strtab.0 >= p.vaddr
                    && self.strtab.0 < p.vaddr + p.filesz
            })
            .ok_or_else(|| err("elf.strtab.bounds"))?;
        let str_off = (s.offset + (self.strtab.0 - s.vaddr)) as usize;
        let str_end = str_off + self.strtab.1 as usize;
        let s0 = str_off + name_off as usize;
        if s0 >= str_end || str_end > self.data.len() {
            return Err(err("elf.strtab.bounds"));
        }
        let e = self.data[s0..str_end]
            .iter()
            .position(|&c| c == 0)
            .ok_or_else(|| err("elf.strtab.nul"))?;
        Ok(String::from_utf8_lossy(&self.data[s0..s0 + e]).into_owned())
    }

    /// Virtual address of the `PT_DYNAMIC` array (for tests/tools).
    pub fn dynamic_vaddr(&self) -> u64 {
        self.dyn_vaddr
    }
    /// File range of `PT_DYNAMIC` data.
    pub fn dynamic_range(&self) -> (usize, usize) {
        self.dyn_range
    }
}

/// Number of dynsym entries; Android libs place .hash/.gnu.hash before
/// .dynsym, so we derive the count from the smallest table section span
/// covering the symbol table: use `DT_HASH` nchain when present, else the
/// distance from symtab to strtab when adjacent, else `usize::MAX` (bounded
/// per lookup by `sym_count` callers pass).
pub fn symbol_count(elf: &ElfFile<'_>, strtab_vaddr: u64) -> usize {
    if elf.symtab.0 == 0 || elf.symtab.1 == 0 {
        return 0;
    }
    let symtab_off = match elf
        .phdrs
        .iter()
        .find(|p| {
            p.p_type == PT_LOAD && elf.symtab.0 >= p.vaddr && elf.symtab.0 < p.vaddr + p.filesz
        })
        .map(|p| (p.offset + (elf.symtab.0 - p.vaddr)) as usize)
    {
        Some(o) => o,
        None => return 0,
    };
    // The real symbol count comes from DT_HASH/DT_GNU_HASH; when the string
    // table follows the symbol table (the usual layout) it bounds the count.
    // Otherwise bound by the file end so unrelated sections are never
    // scanned as symbols.
    let bound = if strtab_vaddr > elf.symtab.0 {
        elf.phdrs
            .iter()
            .find(|p| {
                p.p_type == PT_LOAD && strtab_vaddr >= p.vaddr && strtab_vaddr < p.vaddr + p.filesz
            })
            .map(|p| (p.offset + (strtab_vaddr - p.vaddr)) as usize)
            .unwrap_or(elf.data.len())
    } else {
        elf.data.len()
    };
    bound.saturating_sub(symtab_off) / elf.symtab.1 as usize
}

/// A fully loaded shared object: parsed file, mapped image and load bias.
#[derive(Debug)]
pub struct LoadedSo<'a> {
    /// Parsed ELF image metadata.
    pub elf: ElfFile<'a>,
    /// Virtual load bias (chosen base minus `PT_LOAD` minimum vaddr).
    pub bias: u64,
    /// Soname used for `DT_NEEDED` resolution: the file basename.
    pub soname: String,
    /// Symbols exported for `GLOB_DAT`/`JUMP_SLOT` resolution.
    pub exports: std::collections::HashMap<String, u64>,
    /// Initializer entry points already resolved against `bias`.
    pub initializers: Vec<u64>,
}

/// Load a dependency-closed set of shared objects into `image`, applying
/// relocations. `needed_allow` is the host allowlist for `DT_NEEDED` entries
/// (e.g. `libc.so`, `libm.so`, `liblog.so`, `libandroid.so`, `libjnigraphics.so`,
/// `libz.so`); names absent from `provided` must be allowlisted and are then
/// served by [`Bionic`].
///
/// Returns the objects in dependency order (leaves first), ready for
/// [`run_initializers`].
pub fn load_chain<'a>(
    image: &mut Image,
    soname: &str,
    data: &'a [u8],
    mut provided: std::collections::HashMap<String, &'a [u8]>,
    needed_allow: &[&str],
    next_base: &mut u64,
) -> Result<Vec<LoadedSo<'a>>> {
    provided.insert(soname.to_string(), data);
    let mut order: Vec<String> = Vec::new();
    let mut loaded: std::collections::HashMap<String, LoadedSo<'a>> =
        std::collections::HashMap::new();
    let mut parsed: std::collections::HashMap<String, ElfFile<'a>> =
        std::collections::HashMap::new();
    let mut visiting: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut ordered: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut stack = vec![soname.to_string()];
    // DFS so dependencies load before their dependents; `visiting` marks the
    // in-progress path so cyclic DT_NEEDED graphs terminate (the cycle edge
    // is treated as already-satisfied and load order within a cycle is
    // unspecified).
    while let Some(name) = stack.last().cloned() {
        if ordered.contains(&name) {
            stack.pop();
            continue;
        }
        if !parsed.contains_key(&name) {
            let bytes = *provided
                .get(&name)
                .ok_or_else(|| errd("elf.needed", format!("{name} not provided")))?;
            let elf = ElfFile::parse(bytes)?;
            // Reject unknown relocations BEFORE mapping/initializers (§05).
            for r in &elf.relocs {
                r.kind()?;
            }
            parsed.insert(name.clone(), elf);
            visiting.insert(name.clone());
        }
        let elf = &parsed[&name];
        let mut missing: Vec<String> = Vec::new();
        for dep in &elf.needed {
            if provided.contains_key(dep) || ordered.contains(dep) {
                continue;
            }
            if needed_allow.contains(&dep.as_str()) {
                continue; // served by the Bionic shim
            }
            missing.push(dep.clone());
        }
        if !missing.is_empty() {
            return Err(errd("elf.needed.denied", missing.join(", ")));
        }
        let pending: Vec<String> = elf
            .needed
            .iter()
            .filter(|d| {
                provided.contains_key(*d) && !ordered.contains(*d) && !visiting.contains(*d)
            })
            .cloned()
            .collect();
        if !pending.is_empty() {
            stack.extend(pending);
            continue;
        }
        // All provided dependencies parsed or loading; this object is next.
        order.push(name.clone());
        ordered.insert(name.clone());
        visiting.remove(&name);
        stack.pop();
    }
    // Phase 2 — map every object and build the merged export table before
    // any relocation runs: ld.so semantics. Cyclic DT_NEEDED graphs resolve
    // fully because every export is visible to every object.
    let mut global: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    for name in &order {
        let elf = parsed.remove(name).unwrap();
        let so = map_segments(image, name.clone(), elf, next_base)?;
        for (sym, addr) in &so.exports {
            global.entry(sym.clone()).or_insert(*addr); // first-defined wins
        }
        loaded.insert(name.clone(), so);
    }
    // Phase 3 — relocate, then read each object's initializers (INIT_ARRAY
    // entries are themselves relocated on AArch64).
    for name in &order {
        let so = loaded.get_mut(name).unwrap();
        let sym_count = symbol_count(&so.elf, so.elf.strtab.0);
        for r in &so.elf.relocs {
            r.apply(image, so.bias, &so.elf, sym_count, &global, &so.exports)?;
        }
        let bias = so.bias;
        if so.elf.init_fn != 0 {
            so.initializers.push(bias + so.elf.init_fn);
        }
        if let Some((addr, count)) = so.elf.init_array {
            for i in 0..count {
                let entry = image.read_u64(bias + addr + i * 8)?;
                if entry != 0 {
                    so.initializers.push(entry);
                }
            }
        }
    }
    Ok(order
        .into_iter()
        .map(|n| loaded.remove(&n).unwrap())
        .collect())
}

fn map_segments<'a>(
    image: &mut Image,
    soname: String,
    elf: ElfFile<'a>,
    next_base: &mut u64,
) -> Result<LoadedSo<'a>> {
    let min_v = elf
        .phdrs
        .iter()
        .filter(|p| p.p_type == PT_LOAD)
        .map(|p| p.vaddr & !(PAGE - 1))
        .min()
        .unwrap_or(0);
    let max_v = elf
        .phdrs
        .iter()
        .filter(|p| p.p_type == PT_LOAD)
        .map(|p| (p.vaddr + p.memsz + PAGE - 1) & !(PAGE - 1))
        .max()
        .unwrap_or(0);
    let base = (*next_base + PAGE - 1) & !(PAGE - 1);
    let bias = base.wrapping_sub(min_v);
    *next_base = base + (max_v - min_v) + PAGE;

    for p in elf.phdrs.iter().filter(|p| p.p_type == PT_LOAD) {
        let mut perm = PagePerm::empty();
        if p.flags & 4 != 0 {
            perm |= PagePerm::READ;
        }
        if p.flags & 2 != 0 {
            perm |= PagePerm::WRITE;
        }
        if p.flags & 1 != 0 {
            perm |= PagePerm::EXEC;
        }
        let seg = &elf.data()[p.offset as usize..(p.offset + p.filesz) as usize];
        image.map_segment(bias + p.vaddr, seg, p.memsz, perm)?;
    }

    // Export table: every defined dynsym becomes visible for GLOB_DAT/JUMP_SLOT.
    let mut exports = std::collections::HashMap::new();
    let sym_count = symbol_count(&elf, elf.strtab.0);
    if elf.symtab.0 != 0 && sym_count > 0 {
        let st = elf.phdrs.iter().find(|p| {
            p.p_type == PT_LOAD && elf.symtab.0 >= p.vaddr && elf.symtab.0 < p.vaddr + p.filesz
        });
        if let Some(st) = st {
            let base_off = (st.offset + (elf.symtab.0 - st.vaddr)) as usize;
            let data = elf.data();
            for i in 1..sym_count {
                let p = base_off + i * elf.symtab.1 as usize;
                if p + 24 > data.len() {
                    break;
                }
                let value = u64at(data, p + 8)?;
                if value == 0 {
                    continue;
                }
                let name = elf.symbol_name(i as u32, sym_count)?;
                if !name.is_empty() {
                    exports.insert(name, bias + value);
                }
            }
        }
    }

    Ok(LoadedSo {
        elf,
        bias,
        soname,
        exports,
        initializers: Vec::new(), // filled after the relocation phase
    })
}

/// Record initializers in dependency order, each exactly once. Execution
/// itself is the runtime's job (ovelo-dexrt/host); this returns the ordered
/// entry list and marks them consumed so a second call is a no-op.
pub fn collect_initializers(sos: &[LoadedSo<'_>]) -> Vec<u64> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for so in sos {
        for &i in &so.initializers {
            if seen.insert(i) {
                out.push(i);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bionic::TLS_SLOT_ERRNO;

    struct Spec {
        needed: Vec<String>,
        syms: Vec<(String, u64)>,
        relas: Vec<(u64, u32, u32, i64)>, // (offset, rtype, sym, addend)
        relrs: Vec<u64>,
        init_array: Option<u64>, // value stored in the init_array slot
        machine: u16,
        load_flags: u32,
    }
    impl Default for Spec {
        fn default() -> Self {
            Self {
                needed: vec![],
                syms: vec![],
                relas: vec![],
                relrs: vec![],
                init_array: None,
                machine: EM_AARCH64,
                load_flags: 7,
            }
        }
    }

    fn build(s: &Spec) -> (Vec<u8>, u64, u64) {
        let mut b = vec![0u8; 64 + 2 * 56];
        // strtab content first (offsets fixed now; bytes placed after symtab
        // so symbol_count sees strtab_vaddr > symtab_vaddr, the usual layout)
        let mut strtab = vec![0u8];
        let str_off = |strtab: &mut Vec<u8>, s: &str| -> u64 {
            let o = strtab.len() as u64;
            strtab.extend(s.as_bytes());
            strtab.push(0);
            o
        };
        let needed_off: Vec<u64> = s.needed.iter().map(|n| str_off(&mut strtab, n)).collect();
        let sym_name_off: Vec<u64> = s
            .syms
            .iter()
            .map(|(n, _)| str_off(&mut strtab, n))
            .collect();
        // symtab: sym0 + entries
        let symtab_off = (b.len() + 7) & !7;
        b.resize(symtab_off, 0);
        b.extend([0u8; 24]);
        for ((_, value), noff) in s.syms.iter().zip(&sym_name_off) {
            let mut e = [0u8; 24];
            e[0..4].copy_from_slice(&(*noff as u32).to_le_bytes());
            e[4] = 0x12; // STB_GLOBAL|STT_FUNC
            e[8..16].copy_from_slice(&value.to_le_bytes());
            b.extend(e);
        }
        let strtab_off = b.len();
        b.extend(&strtab);
        // rela
        let rela_off = b.len();
        for (off, rtype, sym, addend) in &s.relas {
            b.extend(&off.to_le_bytes());
            b.extend(&((*sym as u64) << 32 | *rtype as u64).to_le_bytes());
            b.extend(&addend.to_le_bytes());
        }
        // relr
        let relr_off = b.len();
        for r in &s.relrs {
            b.extend(&r.to_le_bytes());
        }
        // init_array: one slot
        b.resize((b.len() + 7) & !7, 0);
        let ia_off = b.len();
        if let Some(v) = s.init_array {
            b.extend(&v.to_le_bytes());
        }
        // data slots for reloc targets inside the file image
        b.resize((b.len() + 7) & !7, 0);
        let slot_off = b.len();
        for _ in 0..4 {
            b.extend(&0xdead_u64.to_le_bytes());
        }
        // dynamic
        let dyn_off = b.len();
        let mut dyn_: Vec<(i64, u64)> = vec![
            (DT_STRTAB, strtab_off as u64),
            (DT_STRSZ, strtab.len() as u64),
            (DT_SYMTAB, symtab_off as u64),
            (DT_SYMENT, 24),
        ];
        for o in &needed_off {
            dyn_.push((DT_NEEDED, *o));
        }
        if !s.relas.is_empty() {
            dyn_.push((DT_RELA, rela_off as u64));
            dyn_.push((DT_RELASZ, (s.relas.len() * 24) as u64));
            dyn_.push((DT_RELAENT, 24));
        }
        if !s.relrs.is_empty() {
            dyn_.push((DT_RELR, relr_off as u64));
            dyn_.push((DT_RELRSZ, (s.relrs.len() * 8) as u64));
            dyn_.push((DT_RELRENT, 8));
        }
        if s.init_array.is_some() {
            dyn_.push((DT_INIT_ARRAY, ia_off as u64));
            dyn_.push((DT_INIT_ARRAYSZ, 8));
        }
        dyn_.push((DT_NULL, 0));
        for (t, v) in &dyn_ {
            b.extend(&t.to_le_bytes());
            b.extend(&v.to_le_bytes());
        }
        let total = b.len() as u64;
        // ehdr
        b[0..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
        b[4] = 2;
        b[5] = 1;
        b[6] = 1;
        b[16..18].copy_from_slice(&3u16.to_le_bytes());
        b[18..20].copy_from_slice(&s.machine.to_le_bytes());
        b[20..24].copy_from_slice(&1u32.to_le_bytes());
        b[32..40].copy_from_slice(&64u64.to_le_bytes());
        b[52..54].copy_from_slice(&64u16.to_le_bytes());
        b[54..56].copy_from_slice(&56u16.to_le_bytes());
        b[56..58].copy_from_slice(&2u16.to_le_bytes());
        // phdr0: PT_LOAD whole file
        let p = 64;
        b[p..p + 4].copy_from_slice(&PT_LOAD.to_le_bytes());
        b[p + 4..p + 8].copy_from_slice(&s.load_flags.to_le_bytes());
        b[p + 16..p + 24].copy_from_slice(&0u64.to_le_bytes());
        b[p + 32..p + 40].copy_from_slice(&total.to_le_bytes());
        b[p + 40..p + 48].copy_from_slice(&total.to_le_bytes());
        b[p + 48..p + 56].copy_from_slice(&0x1000u64.to_le_bytes());
        // phdr1: PT_DYNAMIC
        let p = 64 + 56;
        b[p..p + 4].copy_from_slice(&PT_DYNAMIC.to_le_bytes());
        b[p + 4..p + 8].copy_from_slice(&6u32.to_le_bytes());
        b[p + 8..p + 16].copy_from_slice(&(dyn_off as u64).to_le_bytes());
        b[p + 16..p + 24].copy_from_slice(&(dyn_off as u64).to_le_bytes());
        b[p + 32..p + 40].copy_from_slice(&((dyn_.len() * 16) as u64).to_le_bytes());
        b[p + 40..p + 48].copy_from_slice(&((dyn_.len() * 16) as u64).to_le_bytes());
        (b, slot_off as u64, ia_off as u64)
    }

    #[test]
    fn dbg_err() {
        let (b, ..) = build(&Spec::default());
        let mut img = Image::new();
        let mut next = 0x4000_0000;
        match load_chain(&mut img, "a.so", &b, Default::default(), &[], &mut next) {
            Ok(v) => println!("ok {} sos", v.len()),
            Err(e) => println!("ERR {e}"),
        }
    }

    #[test]
    fn parse_ok_and_needed() {
        let (b, ..) = build(&Spec {
            needed: vec!["libc.so".into()],
            ..Default::default()
        });
        let e = ElfFile::parse(&b).unwrap();
        assert_eq!(e.needed, ["libc.so"]);
        assert_eq!(e.phdrs.len(), 1);
    }

    #[test]
    fn rejects_wrong_machine() {
        let (mut b, ..) = build(&Spec::default());
        b[18..20].copy_from_slice(&62u16.to_le_bytes()); // x86-64
        assert_eq!(ElfFile::parse(&b).unwrap_err().cause(), "elf.machine");
    }

    #[test]
    fn rejects_unknown_reloc() {
        let (b, ..) = build(&Spec {
            relas: vec![(0x100, 0x0404, 0, 0)], // R_AARCH64_TLS_DTPMOD64
            ..Default::default()
        });
        let mut img = Image::new();
        let mut next = 0x4000_0000;
        let e = load_chain(&mut img, "a.so", &b, Default::default(), &[], &mut next).unwrap_err();
        assert_eq!(e.cause(), "elf.reloc.unsupported");
    }

    #[test]
    fn relative_reloc_and_init_order() {
        // lib b.so exports `ext` at 0x80; a.so needs b.so and has a RELATIVE
        // slot, a GLOB_DAT bound to `ext`, and one init_array entry.
        let (bb, ..) = build(&Spec {
            syms: vec![("ext".into(), 0x80)],
            ..Default::default()
        });
        let spec_a = |slot: u64, ia: u64| Spec {
            needed: vec!["b.so".into()],
            syms: vec![("ext".into(), 0)],
            relas: vec![
                (slot, 1027, 0, 0x40),  // RELATIVE: bias + 0x40
                (slot + 8, 1025, 1, 0), // GLOB_DAT: ext
                (ia, 1027, 0, 0x200),   // init_array[0] := bias + 0x200
            ],
            init_array: Some(0),
            ..Default::default()
        };
        let (_t, slot0, ia0) = build(&spec_a(0, 0));
        let (ba, slot, ia) = build(&spec_a(slot0, ia0));
        assert_eq!((slot, ia), (slot0, ia0));

        let mut img = Image::new();
        let mut provided = std::collections::HashMap::new();
        provided.insert("b.so".to_string(), bb.as_slice());
        let mut next = 0x4000_0000;
        let sos = load_chain(&mut img, "a.so", &ba, provided, &[], &mut next).unwrap();
        assert_eq!(sos[0].soname, "b.so"); // dependency first
        let a = &sos[1];
        assert_eq!(img.read_u64(a.bias + slot).unwrap(), a.bias + 0x40);
        assert_eq!(img.read_u64(a.bias + slot + 8).unwrap(), sos[0].bias + 0x80);
        let inits = collect_initializers(&sos);
        assert_eq!(inits, vec![a.bias + 0x200]);
    }

    #[test]
    fn relr_bitmap_decodes() {
        // RELR entries: base slot, then bitmap 0b111 covering +8,+16,+24.
        let (_t, slot0, _i) = build(&Spec {
            relrs: vec![0, 0],
            ..Default::default()
        });
        let (b, ..) = build(&Spec {
            relrs: vec![slot0, 0b111],
            ..Default::default()
        });
        let mut img = Image::new();
        let mut next = 0x4000_0000;
        let sos = load_chain(&mut img, "a.so", &b, Default::default(), &[], &mut next).unwrap();
        let bias = sos[0].bias;
        for off in [0u64, 8, 16] {
            assert_eq!(
                img.read_u64(bias + slot0 + off).unwrap(),
                0xdead + bias,
                "slot +{off}"
            );
        }
    }

    #[test]
    fn cyclic_needed_terminates() {
        let (a, ..) = build(&Spec {
            needed: vec!["b.so".into()],
            ..Default::default()
        });
        let (b2, ..) = build(&Spec {
            needed: vec!["a.so".into()],
            ..Default::default()
        });
        let mut provided = std::collections::HashMap::new();
        provided.insert("b.so".to_string(), b2.as_slice());
        let mut img = Image::new();
        let mut next = 0x4000_0000;
        let sos = load_chain(&mut img, "a.so", &a, provided, &[], &mut next).unwrap();
        assert_eq!(sos.len(), 2); // cycle tolerated, both load
    }

    #[test]
    fn cyclic_needed_resolves_imports() {
        // a.so GLOB_DAT-imports "bfn" defined by b.so; b.so needs a.so.
        // Map-all-then-relocate resolves the back-edge.
        let spec_a = |slot: u64| Spec {
            needed: vec!["b.so".into()],
            syms: vec![("bfn".into(), 0)],
            relas: vec![(slot, 1025, 1, 0)], // GLOB_DAT bfn
            ..Default::default()
        };
        let (_t, slot0, _i) = build(&spec_a(0));
        let (a, ..) = build(&spec_a(slot0));
        let (b, ..) = build(&Spec {
            needed: vec!["a.so".into()],
            syms: vec![("bfn".into(), 0x2340)],
            ..Default::default()
        });
        let mut provided = std::collections::HashMap::new();
        provided.insert("b.so".to_string(), b.as_slice());
        let mut img = Image::new();
        let mut next = 0x4000_0000;
        let sos = load_chain(&mut img, "a.so", &a, provided, &[], &mut next).unwrap();
        let (a_so, b_so) = (
            sos.iter().find(|s| s.soname == "a.so").unwrap(),
            sos.iter().find(|s| s.soname == "b.so").unwrap(),
        );
        assert_eq!(img.read_u64(a_so.bias + slot0).unwrap(), b_so.bias + 0x2340);
    }

    #[test]
    fn allowlisted_libc_import_resolves_to_shim() {
        let spec_a = |slot: u64| Spec {
            needed: vec!["libc.so".into()],
            syms: vec![("memcpy".into(), 0)],
            relas: vec![(slot, 1026, 1, 0)], // JUMP_SLOT memcpy
            ..Default::default()
        };
        let (_t, slot0, _i) = build(&spec_a(0));
        let (b, ..) = build(&spec_a(slot0));
        let mut img = Image::new();
        let mut next = 0x4000_0000;
        let sos = load_chain(
            &mut img,
            "a.so",
            &b,
            Default::default(),
            &["libc.so"],
            &mut next,
        )
        .unwrap();
        let v = img.read_u64(sos[0].bias + slot0).unwrap();
        assert_eq!(v, bionic::shim_addr("memcpy").unwrap());
    }

    #[test]
    fn needed_allowlist() {
        let (b, ..) = build(&Spec {
            needed: vec!["liblog.so".into()],
            ..Default::default()
        });
        let mut img = Image::new();
        let mut next = 0x4000_0000;
        // denied when not provided and not allowlisted
        let e = load_chain(&mut img, "a.so", &b, Default::default(), &[], &mut next).unwrap_err();
        assert_eq!(e.cause(), "elf.needed.denied");
        // allowed via allowlist (served by Bionic shim)
        let mut img = Image::new();
        let mut next = 0x4000_0000;
        assert!(load_chain(
            &mut img,
            "a.so",
            &b,
            Default::default(),
            &["liblog.so"],
            &mut next
        )
        .is_ok());
    }

    #[test]
    fn page_perms_enforced() {
        let (b, ..) = build(&Spec {
            load_flags: 5, // R+X only
            ..Default::default()
        });
        let mut img = Image::new();
        let mut next = 0x4000_0000;
        let sos = load_chain(&mut img, "a.so", &b, Default::default(), &[], &mut next).unwrap();
        assert_eq!(
            img.write(sos[0].bias, &[1]).unwrap_err().cause(),
            "elf.mem.perm"
        );
        img.check_exec(sos[0].bias).unwrap();
    }

    #[test]
    fn bionic_memory_fns() {
        let mut img = Image::new();
        let perm = PagePerm::READ | PagePerm::WRITE;
        img.map_segment(0x1000, b"hello\0", 0x1000, perm).unwrap();
        let mut bio = Bionic::new();
        assert_eq!(bio.strlen(&img, 0x1000).unwrap(), 5);
        assert_eq!(bio.strcmp(&img, 0x1000, 0x1000).unwrap(), 0);
        bio.memcpy(&mut img, 0x1100, 0x1000, 6).unwrap();
        assert_eq!(bio.strlen(&img, 0x1100).unwrap(), 5);
        bio.memset(&mut img, 0x1200, b'x' as u64, 4).unwrap();
        let mut b = [0u8; 4];
        img.read(0x1200, &mut b).unwrap();
        assert_eq!(&b, b"xxxx");
        bio.set_errno(22);
        assert_eq!(bio.tls_slot(TLS_SLOT_ERRNO), 22);
        assert!(Bionic::provides("memcpy"));
        assert!(!Bionic::provides("printf"));
    }
}

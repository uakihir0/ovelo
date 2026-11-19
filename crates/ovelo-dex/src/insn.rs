//! DEX instruction decode: opcode table and operand extraction (design §04).

use crate::read::err;
use ovelo_error::Result;

/// Operand layout of a DEX instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// No operands (1 code unit): `AA|op` byte pair only.
    /// `op` only.
    F10x,
    /// `op vA, vB` nibbles.
    /// `op vA, vB` (4-bit regs).
    F12x,
    /// `op vAA` with signed literal nibble (`const/4`).
    /// `op vA, +lit4` (signed nibble).
    F11n,
    /// `op vAA`.
    /// `op vAA`.
    F11x,
    /// `op +AA` (i8 branch offset).
    /// `op +AA` (i8 branch).
    F10t,
    /// `op +AAAA` (i16 branch offset).
    /// `op +AAAA` (i16 branch).
    F20t,
    /// `op vAA, vBBBB` (two code units).
    /// `op vAA, vBBBB`.
    F22x,
    /// `op vAA, vBBBB` — literal/offset variants.
    /// `op vAA, +BBBB` (i16 branch).
    F21t,
    /// `op vAA, +BBBB` (i16 literal).
    F21s,
    /// `op vAA, lit<<16`.
    F21h,
    /// `op vAA, thing@BBBB`.
    F21c,
    /// `op vAA, vBB, vCC`.
    /// `op vAA, vBB, vCC`.
    F23x,
    /// `op vA, vB, +CCCC`.
    /// `op vA, vB, +CCCC`.
    F22t,
    /// `op vA, vB, thing@CCCC`.
    /// `op vA, vB, thing@CCCC`.
    F22c,
    /// `op vA, vB, +CCCC` (i16 literal).
    F22s,
    /// `op vAA, vBB, +CC` (i8 literal).
    F22b,
    /// `op vAA, +BBBBBBBB` (i32 branch/switch offset).
    /// `op vAA, +BBBBBBBB`.
    F31t,
    /// `op +BBBBBBBB` (i32 branch, no register operand; `30t`).
    F30t,
    /// `op vAA, +BBBBBBBB` (i32 literal).
    F31i,
    /// `op vAA, +BBBBBBBB` (wide i32).
    F31l,
    /// `op vAA, thing@BBBBBBBB`.
    F31c,
    /// `op vAAAA, vBBBB` (three code units).
    /// `op vAAAA, vBBBB`.
    F32x,
    /// `op {vC,vD,vE,vF,vG}, meth@BBBB`.
    /// `op {vC..vG}, thing@BBBB` (<=5 regs).
    F35c,
    /// `op {vCCCC..vNNNN}, meth@BBBB`.
    /// `op {vCCCC..+AA}, thing@BBBB` (range).
    F3rc,
    /// `op {vC..vG}, meth@BBBB, proto@HHHH` (invoke-polymorphic).
    /// `op {vC..vG}, meth@BBBB, proto@HHHH`.
    F45cc,
    /// `op {vCCCC..+AA}, meth@BBBB, proto@HHHH` (range polymorphic call).
    F4rcc,
    /// `op vAA, lit64`.
    F51l,
}

impl Format {
    /// Instruction length in 16-bit code units.
    pub fn size(self) -> usize {
        match self {
            Self::F10x | Self::F12x | Self::F11n | Self::F11x | Self::F10t => 1,
            Self::F20t | Self::F22x | Self::F21t | Self::F21s | Self::F21h | Self::F21c
            | Self::F23x | Self::F22t | Self::F22c | Self::F22s | Self::F22b => 2,
            Self::F30t | Self::F31t | Self::F31i | Self::F31l | Self::F31c
            | Self::F32x | Self::F35c | Self::F3rc => 3,
            Self::F45cc | Self::F4rcc => 4,
            Self::F51l => 5,
        }
    }
}

/// Decoded instruction at a code-unit offset.
#[derive(Debug, Clone)]
pub struct Insn {
    /// Code-unit offset within the method's `insns` array.
    pub offset: u32,
    /// Opcode byte.
    pub opcode: u8,
    /// `name` field.
    pub name: &'static str,
    /// `format` field.
    pub format: Format,
    /// Raw code units (size = format.size()).
    pub raw: Vec<u16>,
    /// Register operands decoded per format, in operand order.
    pub regs: Vec<u16>,
    /// Item-table index operand (type/field/method/string/proto), if any.
    pub idx: Option<u32>,
    /// Signed literal operand.
    pub lit: Option<i64>,
    /// Branch target as a code-unit offset (absolute), if the opcode branches.
    pub target: Option<u32>,
}

impl Insn {
    /// True for payload pseudo-instructions (switch/array-data payload blocks).
    pub fn is_payload(&self) -> bool {
        matches!(self.opcode, 0)
            && matches!(self.raw.first(), Some(&x) if x == 0x0100 || x == 0x0200 || x == 0x0300)
    }
}

/// Decode one instruction (or payload block) at `off` code units.
/// Returns (insn, consumed code units). `b` is the raw `insns` u16 slice.
pub fn decode(insns: &[u16], off: usize) -> Result<(Insn, usize)> {
    let w0 = *insns.get(off).ok_or_else(|| err("dex.insn.bounds"))?;
    // Payload pseudo-instructions share opcode slot 0x00 in the low byte.
    match w0 {
        0x0100 => return decode_packed_switch(insns, off),
        0x0200 => return decode_sparse_switch(insns, off),
        0x0300 => return decode_fill_array(insns, off),
        _ => {}
    }
    let op = (w0 & 0xff) as u8;
    let (name, format) = opcode_info(op).ok_or_else(|| {
        err(format!("dex.opcode.unsupported:{op:#04x}"))
    })?;
    let size = format.size();
    let raw: Vec<u16> = insns
        .get(off..off + size)
        .ok_or_else(|| err("dex.insn.truncated"))?
        .to_vec();
    let mut insn = Insn {
        offset: off as u32,
        opcode: op,
        name,
        format,
        raw,
        regs: Vec::new(),
        idx: None,
        lit: None,
        target: None,
    };
    let w = |i: usize| insn.raw[i];
    let high8 = w0 >> 8;
    let a = high8 & 0x0f;
    let b4 = high8 >> 4;
    match format {
        Format::F10x => {}
        Format::F12x => insn.regs = vec![a, b4],
        Format::F11n => {
            insn.regs = vec![a];
            insn.lit = Some(if b4 & 8 != 0 { b4 as i64 - 16 } else { b4 as i64 });
        }
        Format::F11x => insn.regs = vec![high8],
        Format::F10t => {
            insn.target = Some(((off as i32) + (w0 >> 8) as i8 as i32) as u32);
        }
        Format::F20t => {
            insn.target = Some(((off as i32) + w(1) as i16 as i32) as u32);
        }
        Format::F22x => insn.regs = vec![high8, w(1)],
        Format::F21t => {
            insn.regs = vec![high8];
            insn.target = Some(((off as i32) + w(1) as i16 as i32) as u32);
        }
        Format::F21s => {
            insn.regs = vec![high8];
            insn.lit = Some(w(1) as i16 as i64);
        }
        Format::F21h => {
            insn.regs = vec![high8];
            // const/high16 and const-wide/high16 differ in shift amount;
            // callers disambiguate via opcode (0x15 vs 0x19).
            insn.lit = Some(w(1) as i16 as i64);
        }
        Format::F21c => {
            insn.regs = vec![high8];
            insn.idx = Some(w(1) as u32);
        }
        Format::F23x => insn.regs = vec![high8, w(1) & 0xff, w(1) >> 8],
        Format::F22t => {
            insn.regs = vec![a, b4];
            insn.target = Some(((off as i32) + w(1) as i16 as i32) as u32);
        }
        Format::F22c | Format::F22s => {
            insn.regs = vec![a, b4];
            if format == Format::F22c {
                insn.idx = Some(w(1) as u32);
            } else {
                insn.lit = Some(w(1) as i16 as i64);
            }
        }
        Format::F22b => {
            insn.regs = vec![high8, w(1) & 0xff];
            insn.lit = Some((w(1) >> 8) as u8 as i8 as i64);
        }
        Format::F30t => {
            let delta = ((w(1) as u32) | ((w(2) as u32) << 16)) as i32;
            insn.target = Some(((off as i64) + delta as i64) as u32);
        }
        Format::F31t => {
            insn.regs = vec![high8];
            let delta = ((w(1) as u32) | ((w(2) as u32) << 16)) as i32;
            insn.target = Some(((off as i64) + delta as i64) as u32);
        }
        Format::F31i | Format::F31l => {
            insn.regs = vec![high8];
            insn.lit = Some((((w(2) as u32) << 16) | w(1) as u32) as i32 as i64);
        }
        Format::F31c => {
            insn.regs = vec![high8];
            insn.idx = Some((w(1) as u32) | ((w(2) as u32) << 16));
        }
        Format::F32x => insn.regs = vec![w(1), w(2)],
        Format::F35c => {
            // w0 high byte = A|G: B (high nibble) is the arg count, A (low
            // nibble) is the optional 5th register.
            let count = b4 as usize;
            if count > 5 {
                return Err(err("dex.insn.regcount"));
            }
            insn.idx = Some(w(1) as u32);
            let r2 = w(2);
            let mut regs = vec![r2 & 0x0f, (r2 >> 4) & 0x0f, (r2 >> 8) & 0x0f, r2 >> 12, a];
            regs.truncate(count);
            insn.regs = regs;
        }
        Format::F3rc => {
            let count = high8 as usize;
            insn.idx = Some(w(1) as u32);
            let start = w(2);
            insn.regs = vec![start];
            insn.lit = Some(count as i64); // reg range start + count
        }
        Format::F45cc => {
            // Same A|G layout as 35c; w2 = C..F regs, w3 = HHHH proto idx.
            let count = b4 as usize;
            if count > 5 {
                return Err(err("dex.insn.regcount"));
            }
            insn.idx = Some(w(1) as u32); // method
            insn.lit = Some(w(3) as u32 as i64); // proto
            let r2 = w(2);
            let mut regs = vec![r2 & 0x0f, (r2 >> 4) & 0x0f, (r2 >> 8) & 0x0f, r2 >> 12, a];
            regs.truncate(count);
            insn.regs = regs;
        }
        Format::F4rcc => {
            // AA = range count, CCCC = first register, HHHH = proto idx.
            insn.idx = Some(w(1) as u32); // method
            insn.lit = Some(w(3) as u32 as i64); // proto
            insn.regs = vec![w(2)]; // range start
        }
        Format::F51l => {
            insn.regs = vec![high8];
            let bits = ((w(4) as u64) << 48)
                | ((w(3) as u64) << 32)
                | ((w(2) as u64) << 16)
                | w(1) as u64;
            insn.lit = Some(bits as i64);
        }
    }
    Ok((insn, size))
}

/// `0x0100` packed-switch-payload: ident, size, first_key, then `size` i32
/// targets.
fn decode_packed_switch(insns: &[u16], off: usize) -> Result<(Insn, usize)> {
    let size = *insns.get(off + 1).ok_or_else(|| err("dex.payload.bounds"))? as usize;
    let units = 4 + size * 2;
    if off + units > insns.len() {
        return Err(err("dex.payload.bounds"));
    }
    Ok((
        Insn {
            offset: off as u32,
            opcode: 0,
            name: "packed-switch-payload",
            format: Format::F10x,
            raw: insns[off..off + units].to_vec(),
            regs: vec![],
            idx: None,
            lit: None,
            target: None,
        },
        units,
    ))
}

/// `0x0200` sparse-switch-payload: ident, size, keys[size], targets[size].
fn decode_sparse_switch(insns: &[u16], off: usize) -> Result<(Insn, usize)> {
    let size = *insns.get(off + 1).ok_or_else(|| err("dex.payload.bounds"))? as usize;
    let units = 2 + size * 4;
    if off + units > insns.len() {
        return Err(err("dex.payload.bounds"));
    }
    Ok((
        Insn {
            offset: off as u32,
            opcode: 0,
            name: "sparse-switch-payload",
            format: Format::F10x,
            raw: insns[off..off + units].to_vec(),
            regs: vec![],
            idx: None,
            lit: None,
            target: None,
        },
        units,
    ))
}

/// `0x0300` fill-array-data-payload: ident, element_width, size, data (padded
/// to u16).
fn decode_fill_array(insns: &[u16], off: usize) -> Result<(Insn, usize)> {
    let w = |i: usize| -> Result<u16> {
        Ok(*insns.get(off + i).ok_or_else(|| err("dex.payload.bounds"))?)
    };
    let width = w(1)? as usize;
    let count = ((w(3)? as u32) << 16 | w(2)? as u32) as usize;
    let bytes = width
        .checked_mul(count)
        .ok_or_else(|| err("dex.payload.overflow"))?;
    let units = 4 + bytes.div_ceil(2);
    if width == 0 || off + units > insns.len() {
        return Err(err("dex.payload.bounds"));
    }
    Ok((
        Insn {
            offset: off as u32,
            opcode: 0,
            name: "fill-array-data-payload",
            format: Format::F10x,
            raw: insns[off..off + units].to_vec(),
            regs: vec![],
            idx: None,
            lit: None,
            target: None,
        },
        units,
    ))
}

/// Iterate all instructions in a code unit array (payload blocks included).
pub fn decode_all(insns: &[u16]) -> Result<Vec<Insn>> {
    let mut out = Vec::new();
    let mut off = 0usize;
    while off < insns.len() {
        let (i, n) = decode(insns, off)?;
        out.push(i);
        off += n;
    }
    Ok(out)
}

/// (name, format) for opcode — full DEX 035–039 table.
fn opcode_info(op: u8) -> Option<(&'static str, Format)> {
    use Format::*;
    Some(match op {
        0x00 => ("nop", F10x),
        0x01 => ("move", F12x),
        0x02 => ("move/from16", F22x),
        0x03 => ("move/16", F32x),
        0x04 => ("move-wide", F12x),
        0x05 => ("move-wide/from16", F22x),
        0x06 => ("move-wide/16", F32x),
        0x07 => ("move-object", F12x),
        0x08 => ("move-object/from16", F22x),
        0x09 => ("move-object/16", F32x),
        0x0a => ("move-result", F11x),
        0x0b => ("move-result-wide", F11x),
        0x0c => ("move-result-object", F11x),
        0x0d => ("move-exception", F11x),
        0x0e => ("return-void", F10x),
        0x0f => ("return", F11x),
        0x10 => ("return-wide", F11x),
        0x11 => ("return-object", F11x),
        0x12 => ("const/4", F11n),
        0x13 => ("const/16", F21s),
        0x14 => ("const", F31i),
        0x15 => ("const/high16", F21h),
        0x16 => ("const-wide/16", F21s),
        0x17 => ("const-wide/32", F31l),
        0x18 => ("const-wide", F51l),
        0x19 => ("const-wide/high16", F21h),
        0x1a => ("const-string", F21c),
        0x1b => ("const-string/jumbo", F31c),
        0x1c => ("const-class", F21c),
        0x1d => ("monitor-enter", F11x),
        0x1e => ("monitor-exit", F11x),
        0x1f => ("check-cast", F21c),
        0x20 => ("instance-of", F22c),
        0x21 => ("array-length", F12x),
        0x22 => ("new-instance", F21c),
        0x23 => ("new-array", F22c),
        0x24 => ("filled-new-array", F35c),
        0x25 => ("filled-new-array/range", F3rc),
        0x26 => ("fill-array-data", F31t),
        0x27 => ("throw", F11x),
        0x28 => ("goto", F10t),
        0x29 => ("goto/16", F20t),
        0x2a => ("goto/32", F30t),
        0x2b => ("packed-switch", F31t),
        0x2c => ("sparse-switch", F31t),
        0x2d => ("cmpl-float", F23x),
        0x2e => ("cmpg-float", F23x),
        0x2f => ("cmpl-double", F23x),
        0x30 => ("cmpg-double", F23x),
        0x31 => ("cmp-long", F23x),
        0x32 => ("if-eq", F22t),
        0x33 => ("if-ne", F22t),
        0x34 => ("if-lt", F22t),
        0x35 => ("if-ge", F22t),
        0x36 => ("if-gt", F22t),
        0x37 => ("if-le", F22t),
        0x38 => ("if-eqz", F21t),
        0x39 => ("if-nez", F21t),
        0x3a => ("if-ltz", F21t),
        0x3b => ("if-gez", F21t),
        0x3c => ("if-gtz", F21t),
        0x3d => ("if-lez", F21t),
        // 0x3e..=0x43 unused
        0x44 => ("aget", F23x),
        0x45 => ("aget-wide", F23x),
        0x46 => ("aget-object", F23x),
        0x47 => ("aget-boolean", F23x),
        0x48 => ("aget-byte", F23x),
        0x49 => ("aget-char", F23x),
        0x4a => ("aget-short", F23x),
        0x4b => ("aput", F23x),
        0x4c => ("aput-wide", F23x),
        0x4d => ("aput-object", F23x),
        0x4e => ("aput-boolean", F23x),
        0x4f => ("aput-byte", F23x),
        0x50 => ("aput-char", F23x),
        0x51 => ("aput-short", F23x),
        0x52 => ("iget", F22c),
        0x53 => ("iget-wide", F22c),
        0x54 => ("iget-object", F22c),
        0x55 => ("iget-boolean", F22c),
        0x56 => ("iget-byte", F22c),
        0x57 => ("iget-char", F22c),
        0x58 => ("iget-short", F22c),
        0x59 => ("iput", F22c),
        0x5a => ("iput-wide", F22c),
        0x5b => ("iput-object", F22c),
        0x5c => ("iput-boolean", F22c),
        0x5d => ("iput-byte", F22c),
        0x5e => ("iput-char", F22c),
        0x5f => ("iput-short", F22c),
        0x60 => ("sget", F21c),
        0x61 => ("sget-wide", F21c),
        0x62 => ("sget-object", F21c),
        0x63 => ("sget-boolean", F21c),
        0x64 => ("sget-byte", F21c),
        0x65 => ("sget-char", F21c),
        0x66 => ("sget-short", F21c),
        0x67 => ("sput", F21c),
        0x68 => ("sput-wide", F21c),
        0x69 => ("sput-object", F21c),
        0x6a => ("sput-boolean", F21c),
        0x6b => ("sput-byte", F21c),
        0x6c => ("sput-char", F21c),
        0x6d => ("sput-short", F21c),
        0x6e => ("invoke-virtual", F35c),
        0x6f => ("invoke-super", F35c),
        0x70 => ("invoke-direct", F35c),
        0x71 => ("invoke-static", F35c),
        0x72 => ("invoke-interface", F35c),
        // 0x73 unused
        0x74 => ("invoke-virtual/range", F3rc),
        0x75 => ("invoke-super/range", F3rc),
        0x76 => ("invoke-direct/range", F3rc),
        0x77 => ("invoke-static/range", F3rc),
        0x78 => ("invoke-interface/range", F3rc),
        // 0x79,0x7a unused
        0x7b => ("neg-int", F12x),
        0x7c => ("not-int", F12x),
        0x7d => ("neg-long", F12x),
        0x7e => ("not-long", F12x),
        0x7f => ("neg-float", F12x),
        0x80 => ("neg-double", F12x),
        0x81 => ("int-to-long", F12x),
        0x82 => ("int-to-float", F12x),
        0x83 => ("int-to-double", F12x),
        0x84 => ("long-to-int", F12x),
        0x85 => ("long-to-float", F12x),
        0x86 => ("long-to-double", F12x),
        0x87 => ("float-to-int", F12x),
        0x88 => ("float-to-long", F12x),
        0x89 => ("float-to-double", F12x),
        0x8a => ("double-to-int", F12x),
        0x8b => ("double-to-long", F12x),
        0x8c => ("double-to-float", F12x),
        0x8d => ("int-to-byte", F12x),
        0x8e => ("int-to-char", F12x),
        0x8f => ("int-to-short", F12x),
        0x90 => ("add-int", F23x),
        0x91 => ("sub-int", F23x),
        0x92 => ("mul-int", F23x),
        0x93 => ("div-int", F23x),
        0x94 => ("rem-int", F23x),
        0x95 => ("and-int", F23x),
        0x96 => ("or-int", F23x),
        0x97 => ("xor-int", F23x),
        0x98 => ("shl-int", F23x),
        0x99 => ("shr-int", F23x),
        0x9a => ("ushr-int", F23x),
        0x9b => ("add-long", F23x),
        0x9c => ("sub-long", F23x),
        0x9d => ("mul-long", F23x),
        0x9e => ("div-long", F23x),
        0x9f => ("rem-long", F23x),
        0xa0 => ("and-long", F23x),
        0xa1 => ("or-long", F23x),
        0xa2 => ("xor-long", F23x),
        0xa3 => ("shl-long", F23x),
        0xa4 => ("shr-long", F23x),
        0xa5 => ("ushr-long", F23x),
        0xa6 => ("add-float", F23x),
        0xa7 => ("sub-float", F23x),
        0xa8 => ("mul-float", F23x),
        0xa9 => ("div-float", F23x),
        0xaa => ("rem-float", F23x),
        0xab => ("add-double", F23x),
        0xac => ("sub-double", F23x),
        0xad => ("mul-double", F23x),
        0xae => ("div-double", F23x),
        0xaf => ("rem-double", F23x),
        0xb0 => ("add-int/2addr", F12x),
        0xb1 => ("sub-int/2addr", F12x),
        0xb2 => ("mul-int/2addr", F12x),
        0xb3 => ("div-int/2addr", F12x),
        0xb4 => ("rem-int/2addr", F12x),
        0xb5 => ("and-int/2addr", F12x),
        0xb6 => ("or-int/2addr", F12x),
        0xb7 => ("xor-int/2addr", F12x),
        0xb8 => ("shl-int/2addr", F12x),
        0xb9 => ("shr-int/2addr", F12x),
        0xba => ("ushr-int/2addr", F12x),
        0xbb => ("add-long/2addr", F12x),
        0xbc => ("sub-long/2addr", F12x),
        0xbd => ("mul-long/2addr", F12x),
        0xbe => ("div-long/2addr", F12x),
        0xbf => ("rem-long/2addr", F12x),
        0xc0 => ("and-long/2addr", F12x),
        0xc1 => ("or-long/2addr", F12x),
        0xc2 => ("xor-long/2addr", F12x),
        0xc3 => ("shl-long/2addr", F12x),
        0xc4 => ("shr-long/2addr", F12x),
        0xc5 => ("ushr-long/2addr", F12x),
        0xc6 => ("add-float/2addr", F12x),
        0xc7 => ("sub-float/2addr", F12x),
        0xc8 => ("mul-float/2addr", F12x),
        0xc9 => ("div-float/2addr", F12x),
        0xca => ("rem-float/2addr", F12x),
        0xcb => ("add-double/2addr", F12x),
        0xcc => ("sub-double/2addr", F12x),
        0xcd => ("mul-double/2addr", F12x),
        0xce => ("div-double/2addr", F12x),
        0xcf => ("rem-double/2addr", F12x),
        0xd0 => ("add-int/lit16", F22s),
        0xd1 => ("rsub-int", F22s),
        0xd2 => ("mul-int/lit16", F22s),
        0xd3 => ("div-int/lit16", F22s),
        0xd4 => ("rem-int/lit16", F22s),
        0xd5 => ("and-int/lit16", F22s),
        0xd6 => ("or-int/lit16", F22s),
        0xd7 => ("xor-int/lit16", F22s),
        0xd8 => ("add-int/lit8", F22b),
        0xd9 => ("rsub-int/lit8", F22b),
        0xda => ("mul-int/lit8", F22b),
        0xdb => ("div-int/lit8", F22b),
        0xdc => ("rem-int/lit8", F22b),
        0xdd => ("and-int/lit8", F22b),
        0xde => ("or-int/lit8", F22b),
        0xdf => ("xor-int/lit8", F22b),
        0xe0 => ("shl-int/lit8", F22b),
        0xe1 => ("shr-int/lit8", F22b),
        0xe2 => ("ushr-int/lit8", F22b),
        // 0xe3..=0xf9 unused
        0xfa => ("invoke-polymorphic", F45cc),
        0xfb => ("invoke-polymorphic/range", F4rcc),
        0xfc => ("invoke-custom", F35c),
        0xfd => ("invoke-custom/range", F3rc),
        0xfe => ("const-method-handle", F21c),
        0xff => ("const-method-type", F21c),
        _ => return None,
    })
}



#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_goto32_has_no_register() {
        // goto/32 +3 over a 1-unit insn landing on unit 4.
        let (i, sz) = decode(&[0x002a, 3, 0, 0x000e], 0).unwrap();
        assert_eq!(sz, 3);
        assert_eq!(i.name, "goto/32");
        assert!(i.regs.is_empty());
        assert_eq!(i.target, Some(3));
    }

    #[test]
    fn decode_45cc_four_units() {
        // invoke-polymorphic {v2, v5}, meth@1, proto@7 — count nibble high.
        let (i, sz) = decode(&[0x20fa, 1, 0x0052, 7], 0).unwrap();
        assert_eq!(sz, 4);
        assert_eq!(i.regs, vec![2, 5]);
        assert_eq!(i.idx, Some(1));
        assert_eq!(i.lit, Some(7));
    }

    #[test]
    fn decode_4rcc_four_units() {
        // invoke-polymorphic/range {v9..+3}, meth@2, proto@4.
        let (i, sz) = decode(&[0x03fb, 2, 9, 4], 0).unwrap();
        assert_eq!(sz, 4);
        assert_eq!(i.regs, vec![9]);
        assert_eq!(i.idx, Some(2));
        assert_eq!(i.lit, Some(4));
    }

    #[test]
    fn decode_35c_counts_from_high_nibble() {
        // invoke-static {v0}, meth@0: count=1 (high nibble), no 5th reg.
        let (i, _) = decode(&[0x1071, 0, 0], 0).unwrap();
        assert_eq!(i.name, "invoke-static");
        assert_eq!(i.regs, vec![0]);
        // count=5, G=v6: regs = C..F + G.
        let (i, _) = decode(&[0x5671, 0, 0x4321], 0).unwrap();
        assert_eq!(i.regs, vec![1, 2, 3, 4, 6]);
    }

    #[test]
    fn decode_45cc_truncated_rejected() {
        assert!(decode(&[0x10fa, 0, 0], 0).is_err());
    }
}

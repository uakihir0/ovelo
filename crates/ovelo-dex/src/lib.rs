//! `ovelo-dex`: DEX file parser and structural verifier (design doc §04).
//!
//! [`DexFile::parse`] reads the header and all ID tables; [`DexFile::verify`]
//! runs the structural checks (bounds, branch targets, register counts,
//! duplicate definitions) that gate `DEX_UNSUPPORTED` rejections before the
//! interpreter (`ovelo-dexrt`) executes anything.

mod insn;
mod read;
#[cfg(feature = "testkit")]
pub mod testdex;

pub use insn::{decode_all, Format, Insn};

use read::{err, mutf8, R};
use ovelo_error::Result;
use std::collections::HashSet;

const HEADER_SIZE: usize = 0x70;

/// Parsed DEX file with resolved accessor helpers.
pub struct DexFile {
    bytes: Vec<u8>,
    /// DEX version digits, e.g. `039`.
    pub version: [u8; 3],
    string_ids: Vec<u32>,
    type_ids: Vec<u32>,
    protos: Vec<ProtoId>,
    fields: Vec<FieldId>,
    methods: Vec<MethodId>,
    class_defs: Vec<ClassDef>,
    /// Map list — validated at parse, kept for diagnostics.
    #[allow(dead_code)]
    map: Vec<MapItem>,
}

#[derive(Debug, Clone, Copy)]
/// Proto id: method signature.
pub struct ProtoId {
    /// Shorty descriptor string index.
    pub shorty_idx: u32,
    /// `return_type_idx` field.
    pub return_type_idx: u32,
    /// `parameters_off` field.
    pub parameters_off: u32,
}

#[derive(Debug, Clone, Copy)]
/// Field id: (owner type, type, name).
pub struct FieldId {
    /// Defining-class type index.
    pub class_idx: u16,
    /// `type_idx` field.
    pub type_idx: u16,
    /// `name_idx` field.
    pub name_idx: u32,
}

#[derive(Debug, Clone, Copy)]
/// Method id: (owner type, proto, name).
pub struct MethodId {
    /// Defining-class type index.
    pub class_idx: u16,
    /// `proto_idx` field.
    pub proto_idx: u16,
    /// `name_idx` field.
    pub name_idx: u32,
}

#[derive(Debug, Clone, Copy)]
/// Class definition entry.
pub struct ClassDef {
    /// Type index of this class.
    pub class_idx: u32,
    /// `access_flags` field.
    pub access_flags: u32,
    /// `superclass_idx` field.
    pub superclass_idx: u32,
    /// `interfaces_off` field.
    pub interfaces_off: u32,
    /// `source_file_idx` field.
    pub source_file_idx: u32,
    /// `annotations_off` field.
    pub annotations_off: u32,
    /// `class_data_off` field.
    pub class_data_off: u32,
    /// `static_values_off` field.
    pub static_values_off: u32,
}

#[derive(Debug, Clone, Copy)]
struct MapItem {
    #[allow(dead_code)]
    ty: u16,
    size: u32,
    offset: u32,
}

/// One decoded field from `class_data_item`.
#[derive(Debug, Clone, Copy)]
pub struct EncodedField {
    /// `field_idx` field.
    pub field_idx: u32,
    /// `access_flags` field.
    pub access_flags: u32,
}

/// One decoded method from `class_data_item`.
#[derive(Debug, Clone, Copy)]
pub struct EncodedMethod {
    /// `method_idx` field.
    pub method_idx: u32,
    /// `access_flags` field.
    pub access_flags: u32,
    /// `code_off` field.
    pub code_off: u32,
}

/// Decoded `class_data_item`.
#[derive(Debug, Clone)]
pub struct ClassData {
    /// `static_fields` field.
    pub static_fields: Vec<EncodedField>,
    /// `instance_fields` field.
    pub instance_fields: Vec<EncodedField>,
    /// `direct_methods` field.
    pub direct_methods: Vec<EncodedMethod>,
    /// `virtual_methods` field.
    pub virtual_methods: Vec<EncodedMethod>,
}

/// Decoded `code_item`.
#[derive(Debug, Clone)]
pub struct CodeItem {
    /// `registers_size` field.
    pub registers_size: u16,
    /// `ins_size` field.
    pub ins_size: u16,
    /// `outs_size` field.
    pub outs_size: u16,
    /// `debug_info_off` field.
    pub debug_info_off: u32,
    /// Raw instruction code units.
    pub insns: Vec<u16>,
    /// `tries` field.
    pub tries: Vec<TryItem>,
    /// `handlers` field.
    pub handlers: Vec<CatchHandler>,
}

/// `try_item`: a code-unit range with an exception-handler reference.
#[derive(Debug, Clone, Copy)]
pub struct TryItem {
    /// Start address in code units.
    pub start_addr: u32,
    /// Coverage in code units.
    pub insn_count: u16,
    /// Index into `CodeItem::handlers`.
    pub handler_idx: usize,
}

/// One `encoded_catch_handler` entry.
#[derive(Debug, Clone)]
pub struct CatchHandler {
    /// (type_idx, addr) pairs for typed catches.
    pub handlers: Vec<(u32, u32)>,
    /// catch-all address, if present.
    pub catch_all_addr: Option<u32>,
}

impl DexFile {
    /// Parse the DEX header and all ID tables. Structural verification is
    /// separate: call [`DexFile::verify`] after (or [`DexFile::open`] to do
    /// both at once).
    pub fn parse(bytes: impl Into<Vec<u8>>) -> Result<Self> {
        let bytes = bytes.into();
        if bytes.len() < HEADER_SIZE {
            return Err(err("dex.header.bounds"));
        }
        let mut r = R::new(&bytes);
        if r.bytes(4)? != b"dex\n" {
            return Err(err("dex.magic"));
        }
        let version: [u8; 3] = r.bytes(3)?.try_into().unwrap();
        match &version {
            b"035" | b"036" | b"037" | b"038" | b"039" => {}
            _ => return Err(err("dex.version.unsupported")),
        }
        if r.u8()? != 0 {
            return Err(err("dex.magic"));
        }
        r.u32()?; // checksum
        r.bytes(20)?; // sha1 signature
        let file_size = r.u32()? as usize;
        let header_size = r.u32()? as usize;
        let endian = r.u32()?;
        if file_size != bytes.len() {
            return Err(err("dex.file_size.mismatch"));
        }
        if header_size != HEADER_SIZE || endian != 0x1234_5678 {
            return Err(err("dex.header.malformed"));
        }
        r.u32()?; // link_size
        r.u32()?; // link_off
        let map_off = r.u32()?;
        let string_ids_size = r.u32()? as usize;
        let string_ids_off = r.u32()?;
        let type_ids_size = r.u32()? as usize;
        let type_ids_off = r.u32()?;
        let proto_ids_size = r.u32()? as usize;
        let proto_ids_off = r.u32()?;
        let field_ids_size = r.u32()? as usize;
        let field_ids_off = r.u32()?;
        let method_ids_size = r.u32()? as usize;
        let method_ids_off = r.u32()?;
        let class_defs_size = r.u32()? as usize;
        let class_defs_off = r.u32()?;
        let data_size = r.u32()? as usize;
        let data_off = r.u32()?;
        if data_off as usize + data_size > bytes.len() {
            return Err(err("dex.data.bounds"));
        }

        let map = parse_map(&bytes, map_off)?;
        validate_map(&map, &bytes)?;

        let string_ids = read_u32_table(&bytes, string_ids_off, string_ids_size)?;
        let type_ids = read_u32_table(&bytes, type_ids_off, type_ids_size)?;
        let protos = read_table(&bytes, proto_ids_off, proto_ids_size, 12, |r| {
            Ok(ProtoId {
                shorty_idx: r.u32()?,
                return_type_idx: r.u32()?,
                parameters_off: r.u32()?,
            })
        })?;
        let fields = read_table(&bytes, field_ids_off, field_ids_size, 8, |r| {
            Ok(FieldId {
                class_idx: r.u16()?,
                type_idx: r.u16()?,
                name_idx: r.u32()?,
            })
        })?;
        let methods = read_table(&bytes, method_ids_off, method_ids_size, 8, |r| {
            Ok(MethodId {
                class_idx: r.u16()?,
                proto_idx: r.u16()?,
                name_idx: r.u32()?,
            })
        })?;
        let class_defs = read_table(&bytes, class_defs_off, class_defs_size, 32, |r| {
            Ok(ClassDef {
                class_idx: r.u32()?,
                access_flags: r.u32()?,
                superclass_idx: r.u32()?,
                interfaces_off: r.u32()?,
                source_file_idx: r.u32()?,
                annotations_off: r.u32()?,
                class_data_off: r.u32()?,
                static_values_off: r.u32()?,
            })
        })?;

        Ok(Self {
            bytes,
            version,
            string_ids,
            type_ids,
            protos,
            fields,
            methods,
            class_defs,
            map,
        })
    }

    /// Parse + verify in one step.
    pub fn open(bytes: impl Into<Vec<u8>>) -> Result<Self> {
        let d = Self::parse(bytes)?;
        d.verify()?;
        Ok(d)
    }

    // ---- accessors -----------------------------------------------------

    /// String at `string_ids[idx]` (MUTF-8 decoded).
    pub fn string(&self, idx: u32) -> Result<String> {
        let off = *self
            .string_ids
            .get(idx as usize)
            .ok_or_else(|| err("dex.string.idx"))?;
        let mut r = R::at(&self.bytes, off as usize);
        r.uleb128()?; // utf16 length
        let start = r.p;
        let mut end = start;
        while end < self.bytes.len() && self.bytes[end] != 0 {
            end += 1;
        }
        mutf8(&self.bytes[start..end])
    }

    /// Type descriptor at `type_ids[idx]` (e.g. `Ljava/lang/String;`).
    pub fn type_descriptor(&self, idx: u32) -> Result<String> {
        let sidx = *self
            .type_ids
            .get(idx as usize)
            .ok_or_else(|| err("dex.type.idx"))?;
        self.string(sidx)
    }

    /// `type_ids` index for a descriptor (`Lfoo/Bar;`), if the DEX names it.
    pub fn type_index_of(&self, desc: &str) -> Result<Option<u32>> {
        for (i, &sidx) in self.type_ids.iter().enumerate() {
            if self.string(sidx)? == desc {
                return Ok(Some(i as u32));
            }
        }
        Ok(None)
    }

    /// Field id item.
    pub fn field(&self, idx: u32) -> Result<&FieldId> {
        self.fields.get(idx as usize).ok_or_else(|| err("dex.field.idx"))
    }

    /// Method id item.
    pub fn method(&self, idx: u32) -> Result<&MethodId> {
        self.methods.get(idx as usize).ok_or_else(|| err("dex.method.idx"))
    }

    /// Number of entries in the `method_ids` table (defined + external
    /// references) — the valid range for [`DexFile::method`].
    pub fn method_ids_len(&self) -> usize {
        self.methods.len()
    }

    /// Proto id item.
    pub fn proto(&self, idx: u32) -> Result<&ProtoId> {
        self.protos.get(idx as usize).ok_or_else(|| err("dex.proto.idx"))
    }

    /// Class definitions in file order.
    pub fn class_defs(&self) -> &[ClassDef] {
        &self.class_defs
    }

    /// Parameters of a proto as type indices (type_list decode).
    pub fn proto_params(&self, p: &ProtoId) -> Result<Vec<u16>> {
        self.type_list(p.parameters_off)
    }

    /// Decode a `type_list` (u32 size + u16 items).
    pub fn type_list(&self, off: u32) -> Result<Vec<u16>> {
        if off == 0 {
            return Ok(Vec::new());
        }
        let mut r = R::at(&self.bytes, off as usize);
        let n = r.u32()? as usize;
        if n > (r.end - r.p) / 2 {
            return Err(err("dex.typelist.bounds"));
        }
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            out.push(r.u16()?);
        }
        Ok(out)
    }

    /// Decode `class_data_item` for a class def (empty when `class_data_off==0`).
    pub fn class_data(&self, def: &ClassDef) -> Result<ClassData> {
        if def.class_data_off == 0 {
            return Ok(ClassData {
                static_fields: vec![],
                instance_fields: vec![],
                direct_methods: vec![],
                virtual_methods: vec![],
            });
        }
        let mut r = R::at(&self.bytes, def.class_data_off as usize);
        let ns = r.uleb128()? as usize;
        let ni = r.uleb128()? as usize;
        let nd = r.uleb128()? as usize;
        let nv = r.uleb128()? as usize;
        Ok(ClassData {
            static_fields: read_encoded_fields(&mut r, ns)?,
            instance_fields: read_encoded_fields(&mut r, ni)?,
            direct_methods: read_encoded_methods(&mut r, nd)?,
            virtual_methods: read_encoded_methods(&mut r, nv)?,
        })
    }

    /// Decode a `code_item` at `code_off` (0 → no code).
    pub fn code_item(&self, code_off: u32) -> Result<Option<CodeItem>> {
        if code_off == 0 {
            return Ok(None);
        }
        let mut r = R::at(&self.bytes, code_off as usize);
        let registers_size = r.u16()?;
        let ins_size = r.u16()?;
        let outs_size = r.u16()?;
        let tries_size = r.u16()? as usize;
        let debug_info_off = r.u32()?;
        let insns_size = r.u32()? as usize;
        let insns_bytes = r.bytes(insns_size * 2)?;
        let insns: Vec<u16> = insns_bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        if tries_size > 0 && insns_size % 2 == 1 {
            r.u16()?; // padding
        }
        let mut tries = Vec::with_capacity(tries_size);
        let mut handler_offs = Vec::with_capacity(tries_size);
        for _ in 0..tries_size {
            let start_addr = r.u32()?;
            let insn_count = r.u16()?;
            handler_offs.push(r.u16()? as usize);
            tries.push(TryItem {
                start_addr,
                insn_count,
                handler_idx: 0, // resolved below
            });
        }
        // encoded_catch_handler_list follows the try array.
        let list_start = r.p - code_off as usize;
        let n_handlers = if tries_size > 0 { r.uleb128()? as usize } else { 0 };
        // Cap speculative allocation: each handler needs at least a few bytes.
        if n_handlers > r.end - r.p {
            return Err(err("dex.handler.bounds"));
        }
        let mut handlers = Vec::with_capacity(n_handlers);
        let mut handler_base_offsets = Vec::with_capacity(n_handlers);
        for _ in 0..n_handlers {
            handler_base_offsets.push(r.p - code_off as usize);
            let count = r.sleb128()?;
            let pairs = count.unsigned_abs() as usize;
            if pairs > r.end - r.p {
                return Err(err("dex.handler.bounds"));
            }
            let mut pairs_vec = Vec::with_capacity(pairs);
            for _ in 0..pairs {
                pairs_vec.push((r.uleb128()?, r.uleb128()?));
            }
            let catch_all_addr = if count <= 0 {
                Some(r.uleb128()?)
            } else {
                None
            };
            handlers.push(CatchHandler {
                handlers: pairs_vec,
                catch_all_addr,
            });
        }
        // Map each try's handler_off to a handler index.
        for (t, hoff) in tries.iter_mut().zip(handler_offs) {
            let idx = handler_base_offsets
                .iter()
                .position(|h| *h == hoff + list_start)
                .ok_or_else(|| err("dex.handler.bounds"))?;
            t.handler_idx = idx;
        }
        Ok(Some(CodeItem {
            registers_size,
            ins_size,
            outs_size,
            debug_info_off,
            insns,
            tries,
            handlers,
        }))
    }

    /// All methods (direct + virtual) of every class, for interpretation.
    pub fn methods(&self) -> Result<Vec<(u32 /*class_idx*/, EncodedMethod)>> {
        let mut out = Vec::new();
        for def in &self.class_defs {
            let cd = self.class_data(def)?;
            for m in cd.direct_methods.iter().chain(cd.virtual_methods.iter()) {
                out.push((def.class_idx, *m));
            }
        }
        Ok(out)
    }

    /// Raw file bytes (for API consumers like debug-info decoders).
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    // ---- structural verification ----------------------------------------

    /// Structural checks per §04 "Verification and loading": every referenced
    /// index is in bounds, branch targets land on instruction starts, register
    /// operands fit `registers_size`, try/handler ranges are well-formed, and
    /// no table holds duplicate definitions.
    pub fn verify(&self) -> Result<()> {
        let n_strings = self.string_ids.len() as u32;
        let n_types = self.type_ids.len() as u32;
        let n_protos = self.protos.len() as u32;
        let n_fields = self.fields.len() as u32;
        let n_methods = self.methods.len() as u32;

        // String data offsets land inside the file.
        for &off in &self.string_ids {
            if off as usize >= self.bytes.len() {
                return Err(err("dex.string.bounds"));
            }
        }
        for &sidx in &self.type_ids {
            if sidx >= n_strings {
                return Err(err("dex.type.string_idx"));
            }
        }
        for p in &self.protos {
            if p.shorty_idx >= n_strings || p.return_type_idx >= n_types {
                return Err(err("dex.proto.idx"));
            }
            for &t in &self.proto_params(p)? {
                if t as u32 >= n_types {
                    return Err(err("dex.proto.param_idx"));
                }
            }
        }
        for f in &self.fields {
            if f.class_idx as u32 >= n_types
                || f.type_idx as u32 >= n_types
                || f.name_idx >= n_strings
            {
                return Err(err("dex.field.ref_idx"));
            }
        }
        for m in &self.methods {
            if m.class_idx as u32 >= n_types
                || m.proto_idx as u32 >= n_protos
                || m.name_idx >= n_strings
            {
                return Err(err("dex.method.ref_idx"));
            }
        }

        // No duplicate definitions inside tables.
        dedup(self.string_ids.iter().copied(), "dex.dup.string_id")?;
        dedup(self.type_ids.iter().copied(), "dex.dup.type_id")?;
        dedup_fields(&self.fields)?;
        dedup_methods(&self.methods)?;

        let mut class_seen = HashSet::new();
        for def in &self.class_defs {
            if def.class_idx >= n_types {
                return Err(err("dex.class.type_idx"));
            }
            if !class_seen.insert(def.class_idx) {
                return Err(err("dex.class.duplicate"));
            }
            if def.superclass_idx != u32::MAX && def.superclass_idx >= n_types {
                return Err(err("dex.class.super_idx"));
            }
            if def.source_file_idx != u32::MAX && def.source_file_idx >= n_strings {
                return Err(err("dex.class.srcfile_idx"));
            }
            for &t in &self.type_list(def.interfaces_off)? {
                if t as u32 >= n_types {
                    return Err(err("dex.class.iface_idx"));
                }
            }
            // Auxiliary structures must sit inside the data section.
            for off in [def.annotations_off, def.static_values_off] {
                if off != 0 && off as usize >= self.bytes.len() {
                    return Err(err("dex.class.aux_bounds"));
                }
            }

            let cd = self.class_data(def)?;
            // No duplicate field/method definitions within the class.
            dedup(cd.static_fields.iter().map(|f| f.field_idx), "dex.class.dup.field")?;
            dedup(cd.instance_fields.iter().map(|f| f.field_idx), "dex.class.dup.field")?;
            dedup(cd.direct_methods.iter().map(|m| m.method_idx), "dex.class.dup.method")?;
            dedup(cd.virtual_methods.iter().map(|m| m.method_idx), "dex.class.dup.method")?;

            for f in cd.static_fields.iter().chain(cd.instance_fields.iter()) {
                if f.field_idx >= n_fields {
                    return Err(err("dex.class.field_idx"));
                }
            }
            for m in cd.direct_methods.iter().chain(cd.virtual_methods.iter()) {
                if m.method_idx >= n_methods {
                    return Err(err("dex.class.method_idx"));
                }
                self.verify_code(m.code_off)?;
            }
        }
        Ok(())
    }

    /// Verify one `code_item`: insns bounds, branch targets on instruction
    /// boundaries, register and index operands in range, try/catch well-formed.
    fn verify_code(&self, code_off: u32) -> Result<()> {
        let Some(code) = self.code_item(code_off)? else {
            return Ok(());
        };
        // Registers cover incoming arguments (caller guarantee).
        if (code.ins_size as u32) > code.registers_size as u32 {
            return Err(err("dex.code.regs"));
        }
        let insns_len = code.insns.len() as u32;

        let decoded = decode_all(&code.insns)?;
        // Executable instruction starts (payload blocks are data, not code).
        let exec_starts: HashSet<u32> = decoded
            .iter()
            .filter(|i| !i.is_payload())
            .map(|i| i.offset)
            .collect();

        for i in &decoded {
            if i.is_payload() {
                continue;
            }
            for &reg in &i.regs {
                if reg >= code.registers_size {
                    return Err(err("dex.code.reg_idx"));
                }
            }
            // Range-form invokes: start + count must fit registers.
            if matches!(i.format, Format::F3rc | Format::F4rcc) {
                let start = i.regs[0] as u32;
                let count = (i.raw[0] >> 8) as u32;
                if start + count > code.registers_size as u32 {
                    return Err(err("dex.code.reg_range"));
                }
            }
            if let Some(t) = i.target {
                match i.name {
                    // Payload references must point at the matching data block.
                    "packed-switch" => {
                        expect_payload(&decoded, t, 0x0100)?;
                        verify_switch_cases(&decoded, t, i.offset, &exec_starts, false)?;
                    }
                    "sparse-switch" => {
                        expect_payload(&decoded, t, 0x0200)?;
                        verify_switch_cases(&decoded, t, i.offset, &exec_starts, true)?;
                    }
                    "fill-array-data" => expect_payload(&decoded, t, 0x0300)?,
                    // Ordinary branches must land on executable instructions.
                    _ => {
                        if !exec_starts.contains(&t) {
                            return Err(err("dex.code.branch_target"));
                        }
                    }
                }
            }
            if let Some(idx) = i.idx {
                self.verify_kind_idx(i, idx)?;
            }
        }

        for t in &code.tries {
            let end = t.start_addr + t.insn_count as u32;
            // The try range is [start, end): only start must be an instruction
            // boundary; end is exclusive and may fall mid-instruction.
            if t.insn_count == 0
                || !exec_starts.contains(&t.start_addr)
                || end > insns_len
            {
                return Err(err("dex.try.bounds"));
            }
        }
        for h in &code.handlers {
            for (_, addr) in &h.handlers {
                if !exec_starts.contains(addr) {
                    return Err(err("dex.handler.addr"));
                }
            }
            if let Some(a) = h.catch_all_addr {
                if !exec_starts.contains(&a) {
                    return Err(err("dex.handler.addr"));
                }
            }
        }
        Ok(())
    }

    /// Check a `thing@idx` operand against the right table by opcode class.
    fn verify_kind_idx(&self, i: &Insn, idx: u32) -> Result<()> {
        let n_strings = self.string_ids.len() as u32;
        let n_types = self.type_ids.len() as u32;
        let n_fields = self.fields.len() as u32;
        let n_methods = self.methods.len() as u32;
        let n_protos = self.protos.len() as u32;
        let bad = |w: &str| err(format!("dex.code.operand.{w}"));
        match i.name {
            "const-string" | "const-string/jumbo" => {
                if idx >= n_strings {
                    return Err(bad("string"));
                }
            }
            "const-method-type" => {
                if idx >= n_protos {
                    return Err(bad("proto"));
                }
            }
            "const-class" | "check-cast" | "instance-of" | "new-instance" | "new-array" => {
                if idx >= n_types {
                    return Err(bad("type"));
                }
            }
            "const-method-handle" => {} // method_handle_items not modelled yet
            "sget" | "sget-wide" | "sget-object" | "sget-boolean" | "sget-byte"
            | "sget-char" | "sget-short" | "sput" | "sput-wide" | "sput-object"
            | "sput-boolean" | "sput-byte" | "sput-char" | "sput-short" | "iget"
            | "iget-wide" | "iget-object" | "iget-boolean" | "iget-byte" | "iget-char"
            | "iget-short" | "iput" | "iput-wide" | "iput-object" | "iput-boolean"
            | "iput-byte" | "iput-char" | "iput-short" => {
                if idx >= n_fields {
                    return Err(bad("field"));
                }
            }
            "invoke-virtual" | "invoke-super" | "invoke-direct" | "invoke-static"
            | "invoke-interface" | "invoke-virtual/range" | "invoke-super/range"
            | "invoke-direct/range" | "invoke-static/range" | "invoke-interface/range"
            | "invoke-polymorphic" | "invoke-polymorphic/range" => {
                if idx >= n_methods {
                    return Err(bad("method"));
                }
                if matches!(i.format, Format::F45cc | Format::F4rcc)
                    && i.lit.unwrap_or(u32::MAX as i64) as u32 >= n_protos
                {
                    return Err(bad("proto"));
                }
            }
            "filled-new-array" | "filled-new-array/range" => {
                if idx >= n_types {
                    return Err(bad("type"));
                }
            }
            "invoke-custom" | "invoke-custom/range" => {} // call_site_ids unmodelled
            _ => {}
        }
        Ok(())
    }
}

/// Validate every case destination of a switch payload: signed offsets are
/// relative to the switch instruction and must land on executable starts.
fn verify_switch_cases(
    decoded: &[Insn],
    payload_off: u32,
    switch_off: u32,
    exec_starts: &HashSet<u32>,
    sparse: bool,
) -> Result<()> {
    let payload = decoded
        .iter()
        .find(|p| p.offset == payload_off)
        .ok_or_else(|| err("dex.code.switch_payload"))?;
    let size = *payload.raw.get(1).ok_or_else(|| err("dex.payload.bounds"))? as usize;
    // targets live after ident+size(+first_key for packed) [+keys for sparse].
    let first_target_unit = if sparse { 2 + size * 2 } else { 4 };
    for i in 0..size {
        let u = first_target_unit + i * 2;
        if u + 1 >= payload.raw.len() {
            return Err(err("dex.payload.bounds"));
        }
        let delta = (payload.raw[u] as u32) | ((payload.raw[u + 1] as u32) << 16);
        let target = (switch_off as i64) + (delta as i32) as i64;
        if target < 0 || !exec_starts.contains(&(target as u32)) {
            return Err(err("dex.code.branch_target"));
        }
    }
    Ok(())
}

fn expect_payload(decoded: &[Insn], target: u32, ident: u16) -> Result<()> {
    let ok = decoded
        .iter()
        .any(|p| p.offset == target && p.raw.first() == Some(&ident));
    if ok {
        Ok(())
    } else {
        Err(err("dex.payload.mismatch"))
    }
}

fn dedup(v: impl IntoIterator<Item = u32>, cause: &str) -> Result<()> {
    let mut seen = HashSet::new();
    for x in v {
        if !seen.insert(x) {
            return Err(err(cause));
        }
    }
    Ok(())
}

fn read_encoded_fields(r: &mut R, count: usize) -> Result<Vec<EncodedField>> {
    if count > (r.end - r.p) / 2 {
        return Err(err("dex.classdata.bounds"));
    }
    let mut out = Vec::with_capacity(count);
    let mut idx = 0u32;
    for _ in 0..count {
        idx = idx
            .checked_add(r.uleb128()?)
            .ok_or_else(|| err("dex.classdata.overflow"))?;
        out.push(EncodedField {
            field_idx: idx,
            access_flags: r.uleb128()?,
        });
    }
    Ok(out)
}

fn read_encoded_methods(r: &mut R, count: usize) -> Result<Vec<EncodedMethod>> {
    if count > (r.end - r.p) / 3 {
        return Err(err("dex.classdata.bounds"));
    }
    let mut out = Vec::with_capacity(count);
    let mut idx = 0u32;
    for _ in 0..count {
        idx = idx
            .checked_add(r.uleb128()?)
            .ok_or_else(|| err("dex.classdata.overflow"))?;
        out.push(EncodedMethod {
            method_idx: idx,
            access_flags: r.uleb128()?,
            code_off: r.uleb128()?,
        });
    }
    Ok(out)
}

fn dedup_fields(fields: &[FieldId]) -> Result<()> {
    let mut seen = HashSet::with_capacity(fields.len());
    for f in fields {
        if !seen.insert((f.class_idx, f.type_idx, f.name_idx)) {
            return Err(err("dex.dup.field_id"));
        }
    }
    Ok(())
}

fn dedup_methods(methods: &[MethodId]) -> Result<()> {
    let mut seen = HashSet::with_capacity(methods.len());
    for m in methods {
        if !seen.insert((m.class_idx, m.proto_idx, m.name_idx)) {
            return Err(err("dex.dup.method_id"));
        }
    }
    Ok(())
}

fn read_u32_table(b: &[u8], off: u32, count: usize) -> Result<Vec<u32>> {
    read_table(b, off, count, 4, |r| r.u32())
}

fn read_table<T>(b: &[u8], off: u32, count: usize, item: usize, f: impl Fn(&mut R) -> Result<T>) -> Result<Vec<T>> {
    let off = off as usize;
    let need = off
        .checked_add(
            count
                .checked_mul(item)
                .ok_or_else(|| err("dex.table.overflow"))?,
        )
        .ok_or_else(|| err("dex.table.overflow"))?;
    if need > b.len() {
        return Err(err("dex.table.bounds"));
    }
    let mut out = Vec::with_capacity(count);
    let mut r = R::at(b, off);
    for _ in 0..count {
        out.push(f(&mut r)?);
    }
    Ok(out)
}

fn parse_map(b: &[u8], map_off: u32) -> Result<Vec<MapItem>> {
    let mut r = R::at(b, map_off as usize);
    let count = r.u32()? as usize;
    if count > (r.end - r.p) / 12 {
        return Err(err("dex.map.bounds"));
    }
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let ty = r.u16()?;
        r.u16()?; // unused
        let size = r.u32()?;
        let offset = r.u32()?;
        out.push(MapItem { ty, size, offset });
    }
    Ok(out)
}

fn validate_map(map: &[MapItem], b: &[u8]) -> Result<()> {
    for m in map {
        if m.size != 0 && m.offset as usize >= b.len() {
            return Err(err("dex.map.bounds"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Incremental DEX builder with offset patching.
    struct B {
        b: Vec<u8>,
    }
    impl B {
        fn new() -> Self {
            Self { b: vec![0; 0x70] }
        }
        fn pos(&self) -> usize {
            self.b.len()
        }
        fn u8(&mut self, v: u8) {
            self.b.push(v);
        }
        fn u16(&mut self, v: u16) {
            self.b.extend(v.to_le_bytes());
        }
        fn u32(&mut self, v: u32) {
            self.b.extend(v.to_le_bytes());
        }
        fn uleb(&mut self, mut v: u32) {
            loop {
                let mut byte = (v & 0x7f) as u8;
                v >>= 7;
                if v != 0 {
                    byte |= 0x80;
                }
                self.b.push(byte);
                if v == 0 {
                    break;
                }
            }
        }
        fn patch(&mut self, off: usize, v: u32) {
            self.b[off..off + 4].copy_from_slice(&v.to_le_bytes());
        }
        fn string_data(&mut self, s: &str) -> usize {
            let off = self.pos();
            self.uleb(s.chars().count() as u32);
            self.b.extend(s.as_bytes());
            self.u8(0);
            off
        }
        fn align4(&mut self) {
            while !self.pos().is_multiple_of(4) {
                self.u8(0);
            }
        }
    }

    /// Build a DEX: class LTest; with `m` [regs=2 ins=1] containing
    /// const/4, new-instance, if-eqz, goto, a try/catch, move-exception,
    /// return-void — plus a string and map_list.
    fn build_dex() -> Vec<u8> {
        let strings = ["LTest;", "m", "V", "hello"];
        let mut b = B::new();
        // ID tables (offsets patched later into header).
        let string_ids_off = b.pos();
        for _ in &strings {
            b.u32(0);
        }
        let type_ids_off = b.pos();
        b.u32(0); // LTest;
        b.u32(2); // V
        let proto_ids_off = b.pos();
        b.u32(2); // shorty "V"
        b.u32(1); // return V
        b.u32(0); // params
        let method_ids_off = b.pos();
        b.u16(0); // class LTest;
        b.u16(0); // proto 0
        b.u32(1); // name "m"
        let class_defs_off = b.pos();
        for _ in 0..8 {
            b.u32(0);
        }

        // Data: strings, class_data, code_item, map.
        let mut sdata = Vec::new();
        for s in &strings {
            sdata.push(b.string_data(s));
        }
        b.align4();
        for (i, off) in sdata.iter().enumerate() {
            b.patch(string_ids_off + i * 4, *off as u32);
        }
        let class_data_off = b.pos();
        b.uleb(0); // static fields
        b.uleb(0); // instance fields
        b.uleb(0); // direct methods
        b.uleb(1); // virtual methods
        b.uleb(0); // method_idx_diff -> method 0
        b.uleb(0); // access
        let code_off_slot = b.pos();
        b.uleb(0); // patched below (code_off < 128 fits one uleb? must patch manually)
        // Rewrite as 2-byte uleb to allow patch of larger offsets:
        b.b[code_off_slot] = 0x80;
        b.u8(0); // continuation byte -> value patched later

        b.align4();
        let code_off = b.pos();
        // patch the 2-byte uleb: low7 = code_off&0x7f, high = code_off>>7
        b.b[code_off_slot] = 0x80 | ((code_off & 0x7f) as u8);
        b.b[code_off_slot + 1] = (code_off >> 7) as u8;
        b.u16(3); // registers
        b.u16(1); // ins
        b.u16(1); // outs
        b.u16(1); // tries
        b.u32(0); // debug off
        // insns:
        // 0: const/4 v0,0            -> 0x0012
        // 1: new-instance v1, type0  -> 0x0122, 0x0000  (try starts)
        // 3: goto +4                 -> 0x0428 -> target 7
        // 4: if-eqz v0, -4           -> 0x0038, -4 -> target 0
        // 5: move-exception v2       -> 0x020d  (handler)
        // 6: return-void             -> 0x000e
        let insns: [u16; 8] = [0x0012, 0x0122, 0x0000, 0x0428, 0x0038, 0xfffc, 0x020d, 0x000e];
        b.u32(8); // insns_size
        for w in insns {
            b.u16(w);
        }
        // insns_size even -> no padding
        let try_start = b.pos();
        b.u32(1); // start_addr = 1
        b.u16(2); // insn_count (units 1..3)
        b.u16(0); // handler_off (patch)
        let handler_list_off = b.pos() - code_off;
        b.uleb(1); // handler list size
        let handler_rel = b.pos() - code_off;
        b.uleb(1); // sleb128 count = 1 (positive, no catch-all)
        b.uleb(0); // type_idx 0 (LTest;)
        b.uleb(6); // addr = 6 (move-exception unit offset... wait addr must be code-unit offset of handler)
        // fix: handler addr = 6 where move-exception sits at unit 6
        b.align4();
        let map_off = b.pos();
        b.u32(9); // map size
        let map_entries = [
            (0x0000u16, 1u32, 0u32),                  // header
            (0x0001, strings.len() as u32, string_ids_off as u32),
            (0x0002, 2, type_ids_off as u32),
            (0x0003, 1, proto_ids_off as u32),
            (0x0005, 1, method_ids_off as u32),
            (0x0006, 1, class_defs_off as u32),
            (0x2000, 1, class_data_off as u32),
            (0x2001, 1, code_off as u32),
            (0x1000, 0, 0),
        ];
        for (ty, sz, off) in map_entries {
            b.u16(ty);
            b.u16(0);
            b.u32(sz);
            b.u32(off);
        }
        // fix try's handler_off: byte offset relative to the handler list start
        let hoff_pos = try_start + 6;
        let rel = (handler_rel - handler_list_off) as u16;
        b.b[hoff_pos..hoff_pos + 2].copy_from_slice(&rel.to_le_bytes());

        // Header.
        let total = b.pos() as u32;
        let mut h = Vec::new();
        h.extend(b"dex\n035\0");
        h.extend(0u32.to_le_bytes()); // checksum (unchecked by us)
        h.extend([0u8; 20]); // signature
        h.extend(total.to_le_bytes());
        h.extend(0x70u32.to_le_bytes());
        h.extend(0x12345678u32.to_le_bytes());
        h.extend(0u32.to_le_bytes());
        h.extend(0u32.to_le_bytes());
        h.extend((map_off as u32).to_le_bytes());
        h.extend((strings.len() as u32).to_le_bytes());
        h.extend((string_ids_off as u32).to_le_bytes());
        h.extend(2u32.to_le_bytes());
        h.extend((type_ids_off as u32).to_le_bytes());
        h.extend(1u32.to_le_bytes());
        h.extend((proto_ids_off as u32).to_le_bytes());
        h.extend(0u32.to_le_bytes()); // field_ids_size
        h.extend(0u32.to_le_bytes()); // field_ids_off
        h.extend(1u32.to_le_bytes());
        h.extend((method_ids_off as u32).to_le_bytes());
        h.extend(1u32.to_le_bytes());
        h.extend((class_defs_off as u32).to_le_bytes());
        h.extend((total - 0x70).to_le_bytes()); // data_size approx
        h.extend(0x70u32.to_le_bytes()); // data_off approx
        b.b[..0x70].copy_from_slice(&h);

        // class_def: class_idx 0, access, super u32::MAX, ifaces 0, src 0, anno 0, class_data_off, svalues 0
        b.patch(class_defs_off, 0);
        b.patch(class_defs_off + 4, 0);
        b.patch(class_defs_off + 8, u32::MAX);
        b.patch(class_defs_off + 12, 0);
        b.patch(class_defs_off + 16, u32::MAX); // NO_INDEX source_file
        b.patch(class_defs_off + 24, class_data_off as u32);
        b.b
    }

    #[test]
    fn parses_and_verifies_tiny_class() {
        let dex = DexFile::open(build_dex()).unwrap();
        assert_eq!(&dex.version, b"035");
        assert_eq!(dex.string(0).unwrap(), "LTest;");
        assert_eq!(dex.type_descriptor(1).unwrap(), "V");
        let defs = dex.class_defs();
        assert_eq!(defs.len(), 1);
        let cd = dex.class_data(&defs[0]).unwrap();
        assert_eq!(cd.virtual_methods.len(), 1);
        let code = dex.code_item(cd.virtual_methods[0].code_off).unwrap().unwrap();
        assert_eq!(code.insns.len(), 8);
        let insns = decode_all(&code.insns).unwrap();
        assert_eq!(insns.iter().map(|i| i.name).collect::<Vec<_>>(), vec![
            "const/4", "new-instance", "goto", "if-eqz", "move-exception", "return-void"
        ]);
        assert_eq!(code.tries.len(), 1);
        assert_eq!(code.handlers[code.tries[0].handler_idx].handlers[0].1, 6);
    }

    #[test]
    fn rejects_bad_magic() {
        let mut d = build_dex();
        d[0] = b'X';
        assert!(DexFile::open(d).is_err());
    }

    #[test]
    fn rejects_bad_branch_target() {
        let mut d = build_dex();
        // if-eqz at unit 4: change offset -4 to -5 -> lands mid-instruction.
        // code_item offset: find insns[5] byte offset: header of code item is
        // 16 bytes after code_off; insn index 5 -> byte 16 + 10.
        let dex = DexFile::parse(d.clone()).unwrap();
        let cd = dex.class_data(&dex.class_defs()[0]).unwrap();
        let off = cd.virtual_methods[0].code_off as usize + 16 + 5 * 2;
        d[off..off + 2].copy_from_slice(&(-5i16).to_le_bytes());
        assert!(DexFile::open(d).is_err());
    }

    #[test]
    fn rejects_register_out_of_range() {
        let mut d = build_dex();
        // const/4 v0 -> v9 (A nibble=9 > registers_size=3)
        let dex = DexFile::parse(d.clone()).unwrap();
        let cd = dex.class_data(&dex.class_defs()[0]).unwrap();
        let off = cd.virtual_methods[0].code_off as usize + 16;
        d[off + 1] = 0x09; // A=9, lit=0
        assert!(DexFile::open(d).is_err());
    }

    #[test]
    fn rejects_oversized_try() {
        let mut d = build_dex();
        let dex = DexFile::parse(d.clone()).unwrap();
        let cd = dex.class_data(&dex.class_defs()[0]).unwrap();
        let off = cd.virtual_methods[0].code_off as usize + 16 + 8 * 2 + 4;
        // try insn_count at try_start+4 = code_off + 16 + 16 + 4
        d[off..off + 2].copy_from_slice(&99u16.to_le_bytes());
        assert!(DexFile::open(d).is_err());
    }
}

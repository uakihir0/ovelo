//! Synthetic DEX builder for tests (`testkit` feature).
//!
//! Emits a minimal-but-valid DEX file from a declarative class list so
//! tests across crates can exercise the parser and the interpreter
//! against `external` framework stubs and defined guest classes.

/// ULEB128 encode.
pub fn uleb(mut v: u32) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let mut b = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            b |= 0x80;
        }
        out.push(b);
        if v == 0 {
            break;
        }
    }
    out
}

/// SLEB128 encode.
pub fn sleb(v: i32) -> Vec<u8> {
    let mut out = Vec::new();
    let mut v = v;
    loop {
        let mut b = (v & 0x7f) as u8;
        v >>= 7;
        let sign = b & 0x40;
        if (v == 0 && sign == 0) || (v == -1 && sign != 0) {
            out.push(b);
            break;
        }
        b |= 0x80;
        out.push(b);
    }
    out
}

/// (start_addr, insn_count, typed handlers (desc, addr), catch_all_addr)
pub type Try = (u32, u16, Vec<(&'static str, u32)>, Option<u32>);

/// A declared method.
pub struct M {
    /// Method name.
    pub name: &'static str,
    /// Extra string literals this method can const-string.
    pub lits: &'static [&'static str],
    /// Return type descriptor.
    pub ret: &'static str,
    /// Param type descriptors.
    pub params: &'static [&'static str],
    /// In the virtuals table when true, directs otherwise.
    pub is_virtual: bool,
    /// `registers` in the code item.
    pub regs: u16,
    /// `ins` size.
    pub ins: u16,
    /// `outs` size.
    pub outs: u16,
    /// Encoded code units.
    pub insns: Vec<u16>,
    /// try/catch records.
    pub tries: Vec<Try>,
}

/// A declared class.
pub struct C {
    /// `external` classes contribute method_ids/types but no class_def —
    /// they model framework classes resolved to builtins/host stubs.
    pub ext: bool,
    /// Class descriptor.
    pub class: &'static str,
    /// Superclass descriptor.
    pub sup: Option<&'static str>,
    /// Interface descriptors.
    pub ifaces: &'static [&'static str],
    /// Static fields `(type, name)`.
    pub sfields: &'static [(&'static str, &'static str)],
    /// Instance fields `(type, name)`.
    pub ifields: &'static [(&'static str, &'static str)],
    /// Methods.
    pub methods: Vec<M>,
}

/// Index lookups for patching instructions.
pub struct Ix {
    /// type_ids in order.
    pub t: Vec<String>,
    /// field_ids in order `(class, type, name)`.
    pub f: Vec<(String, String, String)>,
    /// method_ids in order `(class, proto_idx, name)`.
    pub m: Vec<(String, usize, String)>,
    /// string_ids in order.
    pub ss: Vec<String>,
}

impl Ix {
    /// type_idx of descriptor `d`.
    pub fn t(&self, d: &str) -> u32 {
        self.t.iter().position(|x| x == d).unwrap() as u32
    }
    /// string_idx of literal `v`.
    pub fn s_idx(&self, v: &str) -> u32 {
        self.ss.iter().position(|x| x == v).unwrap() as u32
    }
    /// field_idx of `c.n`.
    pub fn f(&self, c: &str, n: &str) -> u32 {
        self.f
            .iter()
            .position(|(a, _, b)| a == c && b == n)
            .unwrap() as u32
    }
    /// method_idx of `c.n`.
    pub fn m(&self, c: &str, n: &str) -> u32 {
        self.m
            .iter()
            .position(|(a, _, b)| a == c && b == n)
            .unwrap() as u32
    }
}

/// Build a DEX image; returns `(bytes, index lookups)`.
pub fn build(classes: &[C]) -> (Vec<u8>, Ix) {
    let mut strings: Vec<String> = Vec::new();
    let mut types: Vec<String> = Vec::new();
    let mut protos: Vec<(String, String, Vec<String>)> = Vec::new();
    let mut fields: Vec<(String, String, String)> = Vec::new();
    let mut methods: Vec<(String, usize, String)> = Vec::new();
    fn intern(v: &mut Vec<String>, s: &str) -> u32 {
        v.iter()
            .position(|x| x == s)
            .map(|i| i as u32)
            .unwrap_or_else(|| {
                v.push(s.to_string());
                (v.len() - 1) as u32
            })
    }
    let shorty = |ret: &str, params: &[&str]| -> String {
        let mut s = String::from(&ret[..1]);
        for p in params {
            s.push_str(&p[..1]);
        }
        s
    };
    for c in classes {
        intern(&mut types, c.class);
        if let Some(s2) = c.sup {
            intern(&mut types, s2);
        }
        for i in c.ifaces {
            intern(&mut types, i);
        }
        for (ty, _n) in c.sfields.iter().chain(c.ifields.iter()) {
            intern(&mut types, ty);
        }
        for m in &c.methods {
            intern(&mut types, m.ret);
            for p in m.params {
                intern(&mut types, p);
            }
            for t in m.tries.iter().flat_map(|t| t.2.iter()) {
                intern(&mut types, t.0);
            }
            for l in m.lits {
                intern(&mut strings, l);
            }
            let sh = shorty(m.ret, m.params);
            let pi = protos
                .iter()
                .position(|(s2, r, p)| {
                    s2 == &sh
                        && r == m.ret
                        && p.iter().map(|x| x.as_str()).eq(m.params.iter().copied())
                })
                .unwrap_or_else(|| {
                    protos.push((
                        sh,
                        m.ret.to_string(),
                        m.params.iter().map(|x| x.to_string()).collect(),
                    ));
                    protos.len() - 1
                });
            methods.push((c.class.to_string(), pi, m.name.to_string()));
        }
        for (ty, n) in c.sfields.iter().chain(c.ifields.iter()) {
            fields.push((c.class.to_string(), ty.to_string(), n.to_string()));
        }
    }
    for t in &types {
        intern(&mut strings, t);
    }
    for (sh, r, ps) in &protos {
        intern(&mut strings, sh);
        intern(&mut strings, r);
        for p in ps {
            intern(&mut strings, p);
        }
    }
    for (c2, t, n) in &fields {
        intern(&mut strings, n);
        intern(&mut strings, t);
        intern(&mut strings, c2);
    }
    for (c2, _pi, n) in &methods {
        intern(&mut strings, n);
        intern(&mut strings, c2);
    }
    let ix = Ix {
        t: types.clone(),
        f: fields.clone(),
        m: methods.clone(),
        ss: strings.clone(),
    };
    let sidx = |s: &str| strings.iter().position(|x| x == s).unwrap() as u32;
    let tidx = |s: &str| types.iter().position(|x| x == s).unwrap() as u32;
    let fidx = |c3: &str, t: &str, n: &str| {
        fields
            .iter()
            .position(|(a, b, d)| a == c3 && b == t && d == n)
            .unwrap() as u32
    };
    let midx = |c3: &str, n: &str| {
        methods
            .iter()
            .position(|(a, _, b)| a == c3 && b == n)
            .unwrap() as u32
    };

    let mut b = vec![0u8; 0x70];
    macro_rules! u32w {
        ($v:expr) => {
            b.extend(($v as u32).to_le_bytes())
        };
    }
    macro_rules! u16w {
        ($v:expr) => {
            b.extend(($v as u16).to_le_bytes())
        };
    }
    macro_rules! patch {
        ($off:expr, $v:expr) => {
            b[$off..$off + 4].copy_from_slice(&($v as u32).to_le_bytes())
        };
    }

    let string_ids_off = b.len();
    for _ in &strings {
        u32w!(0);
    }
    let type_ids_off = b.len();
    for t in &types {
        u32w!(sidx(t));
    }
    let proto_ids_off = b.len();
    for (sh, r, _ps) in &protos {
        u32w!(sidx(sh));
        u32w!(tidx(r));
        u32w!(0);
    }
    let field_ids_off = b.len();
    for (c2, t, n) in &fields {
        u16w!(tidx(c2) as u16);
        u16w!(tidx(t) as u16);
        u32w!(sidx(n));
    }
    let method_ids_off = b.len();
    for (c2, pi, n) in &methods {
        u16w!(tidx(c2) as u16);
        u16w!(*pi as u16);
        u32w!(sidx(n));
    }
    let defs: Vec<&C> = classes.iter().filter(|c| !c.ext).collect();
    let class_defs_off = b.len();
    for _ in &defs {
        for _ in 0..8 {
            u32w!(0);
        }
    }

    let mut sdata = Vec::new();
    for s2 in &strings {
        let off = b.len();
        b.extend(uleb(s2.chars().count() as u32));
        b.extend(s2.as_bytes());
        b.push(0);
        sdata.push(off);
    }
    for (i, off) in sdata.iter().enumerate() {
        patch!(string_ids_off + i * 4, *off);
    }
    while !b.len().is_multiple_of(4) {
        b.push(0);
    }
    for (pi, (_sh, _r, ps)) in protos.iter().enumerate() {
        if !ps.is_empty() {
            let off = b.len();
            u32w!(ps.len());
            for p in ps {
                u16w!(tidx(p) as u16);
            }
            patch!(proto_ids_off + pi * 12 + 8, off);
        }
    }
    let mut iface_offs = Vec::new();
    for c in &defs {
        if !c.ifaces.is_empty() {
            let off = b.len();
            u32w!(c.ifaces.len());
            for i2 in c.ifaces {
                u16w!(tidx(i2) as u16);
            }
            iface_offs.push(off);
        } else {
            iface_offs.push(0);
        }
    }
    let mut code_patch: Vec<usize> = Vec::new();
    let mut class_data_offs = Vec::new();
    for c in &defs {
        let off = b.len();
        class_data_offs.push(off);
        b.extend(uleb(c.sfields.len() as u32));
        b.extend(uleb(c.ifields.len() as u32));
        let directs: Vec<&M> = c.methods.iter().filter(|m| !m.is_virtual).collect();
        let virtuals: Vec<&M> = c.methods.iter().filter(|m| m.is_virtual).collect();
        b.extend(uleb(directs.len() as u32));
        b.extend(uleb(virtuals.len() as u32));
        let mut prev = 0u32;
        for (t, n) in c.sfields.iter() {
            let idx = fidx(c.class, t, n);
            b.extend(uleb(idx - prev));
            prev = idx;
            b.extend(uleb(0));
        }
        let mut prev = 0u32;
        for (t, n) in c.ifields.iter() {
            let idx = fidx(c.class, t, n);
            b.extend(uleb(idx - prev));
            prev = idx;
            b.extend(uleb(0));
        }
        let mut emit = |b: &mut Vec<u8>, m: &M, prev: &mut u32| {
            let idx = midx(c.class, m.name);
            b.extend(uleb(idx - *prev));
            *prev = idx;
            b.extend(uleb(0));
            code_patch.push(b.len());
            b.extend([0, 0, 0]); // 3-byte code_off patch (aligned uleb)
        };
        let mut prev = 0u32;
        for m in &directs {
            emit(&mut b, m, &mut prev);
        }
        let mut prev = 0u32;
        for m in &virtuals {
            emit(&mut b, m, &mut prev);
        }
    }
    while !b.len().is_multiple_of(4) {
        b.push(0);
    }
    let mut mseq = 0usize;
    for c in &defs {
        let ordered: Vec<&M> = c
            .methods
            .iter()
            .filter(|m| !m.is_virtual)
            .chain(c.methods.iter().filter(|m| m.is_virtual))
            .collect();
        for m in ordered {
            let code_off = b.len();
            let pp = code_patch[mseq];
            // 3-byte aligned uleb: 0x80,0x80,hi? write as fixed 3-byte uleb
            let v = code_off as u32;
            b[pp] = 0x80 | ((v & 0x7f) as u8);
            b[pp + 1] = 0x80 | (((v >> 7) & 0x7f) as u8);
            b[pp + 2] = ((v >> 14) & 0x7f) as u8;
            mseq += 1;
            u16w!(m.regs);
            u16w!(m.ins);
            u16w!(m.outs);
            u16w!(m.tries.len() as u16);
            u32w!(0);
            u32w!(m.insns.len());
            for w in &m.insns {
                u16w!(*w);
            }
            if m.insns.len() % 2 == 1 && !m.tries.is_empty() {
                u16w!(0);
            }
            if !m.tries.is_empty() {
                let try_pos = b.len();
                for _ in 0..m.tries.len() {
                    u32w!(0);
                    u16w!(0);
                    u16w!(0);
                }
                let list_off = b.len();
                b.extend(uleb(m.tries.len() as u32));
                let mut hoffs = Vec::new();
                for t in &m.tries {
                    hoffs.push(b.len() - list_off);
                    let n = t.2.len() as i32;
                    b.extend(sleb(if t.3.is_some() { -n } else { n }));
                    for (ty, addr) in &t.2 {
                        b.extend(uleb(tidx(ty)));
                        b.extend(uleb(*addr));
                    }
                    if let Some(a) = t.3 {
                        b.extend(uleb(a));
                    }
                }
                for (k, t) in m.tries.iter().enumerate() {
                    let tp = try_pos + k * 8;
                    b[tp..tp + 4].copy_from_slice(&t.0.to_le_bytes());
                    b[tp + 4..tp + 6].copy_from_slice(&t.1.to_le_bytes());
                    b[tp + 6..tp + 8].copy_from_slice(&(hoffs[k] as u16).to_le_bytes());
                }
            }
            while !b.len().is_multiple_of(4) {
                b.push(0);
            }
        }
    }
    for (i, c) in defs.iter().enumerate() {
        let o = class_defs_off + i * 32;
        patch!(o, tidx(c.class));
        patch!(o + 4, 1u32);
        patch!(o + 8, c.sup.map(tidx).unwrap_or(u32::MAX));
        patch!(o + 12, iface_offs[i]);
        patch!(o + 16, u32::MAX);
        patch!(o + 24, class_data_offs[i]);
    }
    let map_off = b.len();
    u32w!(10);
    let ents = [
        (0x0000u16, 1u32, 0u32),
        (0x0001, strings.len() as u32, string_ids_off as u32),
        (0x0002, types.len() as u32, type_ids_off as u32),
        (0x0003, protos.len() as u32, proto_ids_off as u32),
        (0x0004, fields.len() as u32, field_ids_off as u32),
        (0x0005, methods.len() as u32, method_ids_off as u32),
        (0x0006, defs.len() as u32, class_defs_off as u32),
        (0x2000, defs.len() as u32, class_data_offs[0] as u32),
        (0x2001, mseq as u32, 0u32),
        (0x1000, 1u32, map_off as u32),
    ];
    for (t, sz, o) in ents {
        u16w!(t);
        u16w!(0);
        u32w!(sz);
        u32w!(o);
    }
    let file_size = b.len() as u32;
    let mut h = Vec::new();
    h.extend(b"dex\n035\0");
    h.extend(0u32.to_le_bytes());
    h.extend([0u8; 20]);
    h.extend(file_size.to_le_bytes());
    h.extend(0x70u32.to_le_bytes());
    h.extend(0x12345678u32.to_le_bytes());
    h.extend(0u32.to_le_bytes());
    h.extend(0u32.to_le_bytes());
    h.extend((map_off as u32).to_le_bytes());
    h.extend((strings.len() as u32).to_le_bytes());
    h.extend((string_ids_off as u32).to_le_bytes());
    h.extend((types.len() as u32).to_le_bytes());
    h.extend((type_ids_off as u32).to_le_bytes());
    h.extend((protos.len() as u32).to_le_bytes());
    h.extend((proto_ids_off as u32).to_le_bytes());
    h.extend((fields.len() as u32).to_le_bytes());
    h.extend((field_ids_off as u32).to_le_bytes());
    h.extend((methods.len() as u32).to_le_bytes());
    h.extend((method_ids_off as u32).to_le_bytes());
    h.extend((defs.len() as u32).to_le_bytes());
    h.extend((class_defs_off as u32).to_le_bytes());
    h.extend((file_size - 0x70).to_le_bytes());
    h.extend(0x70u32.to_le_bytes());
    b[..0x70].copy_from_slice(&h);
    (b, ix)
}

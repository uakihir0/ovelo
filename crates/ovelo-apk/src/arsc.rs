//! `resources.arsc` enumeration (design doc §03 step 2).
//!
//! Parses the resource table enough to enumerate packages, types, config
//! buckets and entries. Full value resolution (theme parents, attribute
//! references, density/locale selection) is layered on top later.

use ovelo_error::{ErrorCategory, OveloError, Phase, Result};

const RES_TABLE_TYPE: u16 = 0x0002;
const RES_STRING_POOL_TYPE: u16 = 0x0001;
const RES_TABLE_PACKAGE_TYPE: u16 = 0x0200;
const RES_TABLE_TYPE_SPEC_TYPE: u16 = 0x0202;
const RES_TABLE_TYPE_TYPE: u16 = 0x0201;
const RES_TABLE_CONFIG_TYPE: u16 = 0x0204; // inside library only; kept for completeness

const UTF8_FLAG: u32 = 0x0000_0100;
const ENTRY_FLAG_COMPLEX: u16 = 0x0001;
const ENTRY_FLAG_PUBLIC: u16 = 0x0002;
const SPEC_PUBLIC: u32 = 0x4000_0000;

fn err(cause: impl Into<String>) -> OveloError {
    OveloError::new(ErrorCategory::PkgInvalid, Phase::Install, cause)
}

fn u16at(b: &[u8], i: usize) -> Result<u16> {
    if i + 2 > b.len() {
        return Err(err("arsc.truncated"));
    }
    Ok(u16::from_le_bytes(b[i..i + 2].try_into().unwrap()))
}
fn u32at(b: &[u8], i: usize) -> Result<u32> {
    if i + 4 > b.len() {
        return Err(err("arsc.truncated"));
    }
    Ok(u32::from_le_bytes(b[i..i + 4].try_into().unwrap()))
}

/// Decoded string pool shared by table and package chunks.
#[derive(Debug, Default)]
pub struct StringPool {
    strings: Vec<String>,
}

impl StringPool {
    /// String at index.
    pub fn get(&self, idx: u32) -> Option<&str> {
        self.strings.get(idx as usize).map(|s| s.as_str())
    }
    /// Number of strings.
    pub fn len(&self) -> usize {
        self.strings.len()
    }
    /// All strings, in pool order.
    pub fn to_vec(&self) -> Vec<String> {
        self.strings.clone()
    }
    /// Whether the pool is empty.
    pub fn is_empty(&self) -> bool {
        self.strings.is_empty()
    }

    /// Parse a string pool chunk starting at `pos` (chunk header included).
    pub fn parse(b: &[u8], pos: usize) -> Result<(Self, usize)> {
        let header_size = u16at(b, pos + 2)? as usize;
        let size = u32at(b, pos + 4)? as usize;
        if size < header_size || pos + size > b.len() {
            return Err(err("arsc.stringpool.bounds"));
        }
        let end = pos + size;
        let string_count = u32at(b, pos + 8)? as usize;
        let style_count = u32at(b, pos + 12)? as usize;
        let flags = u32at(b, pos + 16)?;
        let strings_start = u32at(b, pos + 20)? as usize;
        let utf8 = flags & UTF8_FLAG != 0;

        if header_size + (string_count + style_count) * 4 > size {
            return Err(err("arsc.stringpool.truncated"));
        }
        let mut offsets = Vec::with_capacity(string_count);
        for i in 0..string_count {
            offsets.push(u32at(b, pos + header_size + i * 4)? as usize);
        }
        let region_end = end;
        let mut strings = Vec::with_capacity(string_count);
        for off in offsets {
            let start = pos + strings_start + off;
            if start >= region_end {
                return Err(err("arsc.stringpool.offset"));
            }
            let s = if utf8 {
                read_utf8(b, start, region_end)?
            } else {
                read_utf16(b, start, region_end)?
            };
            strings.push(s);
        }
        Ok((Self { strings }, end))
    }
}

fn read_utf8(b: &[u8], mut p: usize, end: usize) -> Result<String> {
    // u8 utf16-len (maybe 2 bytes), u8 utf8-len (maybe 2 bytes), bytes, NUL.
    if p >= end {
        return Err(err("arsc.string.truncated"));
    }
    let first = b[p];
    p += if first & 0x80 != 0 { 2 } else { 1 };
    if p >= end {
        return Err(err("arsc.string.truncated"));
    }
    let len = if b[p] & 0x80 != 0 {
        if p + 2 > end {
            return Err(err("arsc.string.truncated"));
        }
        let l = (((b[p] & 0x7f) as usize) << 8) | b[p + 1] as usize;
        p += 2;
        l
    } else {
        let l = b[p] as usize;
        p += 1;
        l
    };
    if p + len > end {
        return Err(err("arsc.string.truncated"));
    }
    Ok(String::from_utf8_lossy(&b[p..p + len]).into_owned())
}

fn read_utf16(b: &[u8], p: usize, end: usize) -> Result<String> {
    if p + 2 > end {
        return Err(err("arsc.string.truncated"));
    }
    let first = u16at(b, p)? as usize;
    let (len, start) = if first & 0x8000 != 0 {
        let second = u16at(b, p + 2)? as usize;
        ((((first & 0x7fff) << 16) | second), p + 4)
    } else {
        (first, p + 2)
    };
    if start + len * 2 > end {
        return Err(err("arsc.string.truncated"));
    }
    let mut units = Vec::with_capacity(len);
    for i in 0..len {
        units.push(u16at(b, start + i * 2)?);
    }
    Ok(String::from_utf16_lossy(&units))
}

/// A typed value in a table entry.
#[derive(Debug, Clone)]
pub struct EntryValue {
    /// Value data type (e.g. 0x03 string, 0x10 int-dec).
    pub data_type: u8,
    /// Raw 32-bit payload.
    pub data: u32,
    /// Resolved string when `data_type == 0x03`.
    pub string: Option<String>,
}

/// One resource entry in one config bucket.
#[derive(Debug, Clone)]
pub struct ResourceEntry {
    /// Entry key name (from the package's key string pool).
    pub key: String,
    /// Resource id 0xPPTTEEEE.
    pub res_id: u32,
    /// Public flag from the type spec.
    pub public: bool,
    /// Simple value (None for complex/map entries).
    pub value: Option<EntryValue>,
    /// Parent resource id for complex entries (style parents; 0 if none).
    pub parent: u32,
    /// Item map for complex entries (style attribute values).
    pub items: Vec<(u32, EntryValue)>,
    /// Config description (e.g. "default", "ja", "xxhdpi").
    pub config: String,
}

/// A resource type (e.g. `string`, `attr`) with its entries.
#[derive(Debug, Clone)]
pub struct ResourceType {
    /// Type name from the type string pool.
    pub name: String,
    /// Type id (1-based within the package).
    pub type_id: u8,
    /// Entries across config buckets.
    pub entries: Vec<ResourceEntry>,
}

/// A package chunk (`ResTable_package`).
#[derive(Debug, Clone)]
pub struct ResourcePackage {
    /// Package id (0x7f app, 0x01 framework).
    pub package_id: u32,
    /// Package name.
    pub name: String,
    /// Types in this package.
    pub types: Vec<ResourceType>,
    /// The package's key string pool — item keys of complex entries
    /// (`<plurals>` quantity names) index into it.
    pub key_strings: Vec<String>,
}

/// Parse `resources.arsc` into packages and entries.
pub fn parse_arsc(b: &[u8]) -> Result<Vec<ResourcePackage>> {
    if b.len() < 12 || u16at(b, 0)? != RES_TABLE_TYPE {
        return Err(err("arsc.not_table"));
    }
    let header_size = u16at(b, 2)? as usize;
    let size = u32at(b, 4)? as usize;
    if size > b.len() || header_size > size {
        return Err(err("arsc.bounds"));
    }
    let package_count = u32at(b, 8)? as usize;
    let mut pos = header_size;
    let mut global_strings = StringPool::default();
    let mut packages = Vec::new();
    let mut seen_packages = 0usize;

    while pos + 8 <= size {
        let ty = u16at(b, pos)?;
        let chunk_header = u16at(b, pos + 2)? as usize;
        let chunk_size = u32at(b, pos + 4)? as usize;
        if chunk_size < 8 || pos + chunk_size > size {
            return Err(err("arsc.chunk.bounds"));
        }
        match ty {
            RES_STRING_POOL_TYPE => {
                let (pool, _end) = StringPool::parse(b, pos)?;
                global_strings = pool;
            }
            RES_TABLE_PACKAGE_TYPE => {
                packages.push(parse_package(
                    b,
                    pos,
                    chunk_header,
                    chunk_size,
                    &global_strings,
                )?);
                seen_packages += 1;
            }
            _ => {}
        }
        pos += chunk_size;
    }
    if seen_packages != package_count {
        return Err(err("arsc.package.count_mismatch"));
    }
    Ok(packages)
}

fn config_name(b: &[u8], off: usize, size: usize) -> String {
    let size = size.min(b.len().saturating_sub(off));
    // ResTable_config (off points at its `size` u32):
    //   +4  imsi   : mcc u16, mnc u16
    //   +8  locale : language[2], country[2]   — each either two ASCII
    //                chars or a 0x8000-tagged packed 3-char code
    //   +12 screenType : orientation u8, touchscreen u8, density u16
    //   +16 input, +20 screenSize, +24 version, +28 screenConfig…
    let mut parts = Vec::new();
    let packed = |c0: u8, c1: u8, base: u8| -> String {
        // byte0 is the HIGH byte of the packed u16 (layout is big-endian
        // regardless of host arch): bit15 is the marker, chars sit in
        // 5-bit groups first→last as `c - base`.
        let first = base.wrapping_add(c1 & 0x1f) as char;
        let second = base
            .wrapping_add((c1 & 0xe0) >> 5)
            .wrapping_add((c0 & 0x03) << 3) as char;
        let third = base.wrapping_add((c0 & 0x7c) >> 2) as char;
        format!("{first}{second}{third}")
    };
    if size >= 12 {
        let lang = [b[off + 8], b[off + 9]];
        let region = [b[off + 10], b[off + 11]];
        if lang != [0, 0] {
            let l = if lang[0] & 0x80 != 0 {
                packed(lang[0], lang[1], b'a')
            } else {
                String::from_utf8_lossy(&lang).to_string()
            };
            let r = if region == [0, 0] {
                String::new()
            } else if region[0] & 0x80 != 0 {
                packed(region[0], region[1], b'0')
            } else {
                String::from_utf8_lossy(&region).to_string()
            };
            if r.is_empty() {
                parts.push(l);
            } else {
                parts.push(format!("{l}-r{r}"));
            }
        }
    }
    if size >= 16 {
        let density = u16::from_le_bytes(b[off + 14..off + 16].try_into().unwrap());
        match density {
            0 => {}
            120 => parts.push("ldpi".into()),
            160 => parts.push("mdpi".into()),
            240 => parts.push("hdpi".into()),
            320 => parts.push("xhdpi".into()),
            480 => parts.push("xxhdpi".into()),
            640 => parts.push("xxxhdpi".into()),
            0xfffe => parts.push("anydpi".into()),
            0xffff => parts.push("nodpi".into()),
            d => parts.push(format!("{d}dpi")),
        }
        // Orientation shares the screenType field (byte +12).
        match b[off + 12] {
            1 => parts.push("port".into()),
            2 => parts.push("land".into()),
            3 => parts.push("square".into()),
            _ => {} // 0=any
        }
    }
    // ResTable_config +24 version: sdkVersion u16 → `v<N>` buckets.
    if size >= 28 {
        let sdk = u16::from_le_bytes(b[off + 24..off + 26].try_into().unwrap());
        if sdk != 0 {
            parts.push(format!("v{sdk}"));
        }
    }
    // ResTable_config +28 screenConfig: screenLayout u8, uiMode u8,
    // smallestScreenWidthDp u16 — distinguishing sw<N>dp / night /
    // screen-size buckets is what keeps a sw600dp layout from being
    // labeled "default" and clobbering the real phone layout.
    if size >= 32 {
        let screen_layout = b[off + 28];
        let ui_mode = b[off + 29];
        let sw = u16::from_le_bytes(b[off + 30..off + 32].try_into().unwrap());
        if sw != 0 {
            parts.push(format!("sw{sw}dp"));
        }
        match screen_layout & 0x0f {
            1 => parts.push("small".into()),
            3 => parts.push("large".into()),
            4 => parts.push("xlarge".into()),
            _ => {}
        }
        if screen_layout & 0x30 == 0x20 {
            parts.push("long".into());
        } else if screen_layout & 0x30 == 0x10 {
            parts.push("notlong".into());
        }
        match ui_mode & 0x30 {
            0x10 => parts.push("notnight".into()),
            0x20 => parts.push("night".into()),
            _ => {}
        }
        match ui_mode & 0x0f {
            0x04 => parts.push("car".into()),
            0x05 => parts.push("television".into()),
            0x03 => parts.push("watch".into()),
            _ => {}
        }
    }
    // +32 screenSizeDp: w<N>dp / h<N>dp qualifiers.
    if size >= 36 {
        let w = u16::from_le_bytes(b[off + 32..off + 34].try_into().unwrap());
        let h = u16::from_le_bytes(b[off + 34..off + 36].try_into().unwrap());
        if w != 0 {
            parts.push(format!("w{w}dp"));
        }
        if h != 0 {
            parts.push(format!("h{h}dp"));
        }
    }
    if parts.is_empty() {
        "default".to_string()
    } else {
        parts.join("-")
    }
}

fn end_of_chunk(b: &[u8], pos: usize, chunk_size: usize) -> Result<usize> {
    pos.checked_add(chunk_size)
        .filter(|e| *e <= b.len())
        .ok_or_else(|| err("arsc.chunk.bounds"))
}

fn parse_package(
    b: &[u8],
    pos: usize,
    header_size: usize,
    chunk_size: usize,
    global: &StringPool,
) -> Result<ResourcePackage> {
    // Fixed package header: chunk(8) + id(4) + name(256) + 4 x u32 offsets.
    if header_size < 288 || pos + 288 > end_of_chunk(b, pos, chunk_size)? {
        return Err(err("arsc.pkg.truncated"));
    }
    let package_id = u32at(b, pos + 8)?;
    let name_bytes = &b[pos + 12..pos + 12 + 256];
    let name_units: Vec<u16> = name_bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .take_while(|u| *u != 0)
        .collect();
    let name = String::from_utf16_lossy(&name_units);
    let type_strings_off = u32at(b, pos + 12 + 256)? as usize;
    let _last_public_type = u32at(b, pos + 12 + 256 + 4)?;
    let key_strings_off = u32at(b, pos + 12 + 256 + 8)? as usize;
    let _last_public_key = u32at(b, pos + 12 + 256 + 12)?;

    let base = pos;
    let end = pos + chunk_size;
    let mut p = pos + header_size;
    let mut type_strings = StringPool::default();
    let mut key_strings = StringPool::default();
    let mut spec_flags: std::collections::HashMap<u8, Vec<u32>> = Default::default();
    let mut types: Vec<ResourceType> = Vec::new();

    while p + 8 <= end {
        let ty = u16at(b, p)?;
        let hsz = u16at(b, p + 2)? as usize;
        let csz = u32at(b, p + 4)? as usize;
        if csz < 8 || p + csz > end {
            return Err(err("arsc.pkg.chunk_bounds"));
        }
        match ty {
            RES_STRING_POOL_TYPE => {
                let (pool, _) = StringPool::parse(b, p)?;
                if p - base == type_strings_off {
                    type_strings = pool;
                } else if p - base == key_strings_off {
                    key_strings = pool;
                }
            }
            RES_TABLE_TYPE_SPEC_TYPE => {
                let type_id = b[p + 8];
                let entry_count = u32at(b, p + 12)? as usize;
                let mut flags = Vec::with_capacity(entry_count);
                for i in 0..entry_count {
                    flags.push(u32at(b, p + hsz + i * 4)?);
                }
                spec_flags.insert(type_id, flags);
            }
            RES_TABLE_TYPE_TYPE => {
                if p + 20 > end || csz < 20 {
                    return Err(err("arsc.type.truncated"));
                }
                let type_id = b[p + 8];
                let flags = b[p + 9];
                let entry_count = u32at(b, p + 12)? as usize;
                let entries_start = u32at(b, p + 16)? as usize;
                const FLAG_SPARSE: u8 = 0x01;
                const FLAG_OFFSET16: u8 = 0x02;
                let entry_at = |i: usize| -> Result<Option<(usize, usize)>> {
                    // Returns (entry_id, byte offset) or None for absent.
                    if flags & FLAG_SPARSE != 0 {
                        if p + hsz + i * 4 + 4 > p + csz {
                            return Err(err("arsc.entry.bounds"));
                        }
                        // Sparse spec = u16 entryIdx | u16 entryOffset(in 4B units);
                        // every listed record is present (no 0xffff sentinel).
                        let e = u32at(b, p + hsz + i * 4)?;
                        let idx = (e & 0xffff) as usize;
                        let off = ((e >> 16) * 4) as usize;
                        Ok(Some((idx, off)))
                    } else if flags & FLAG_OFFSET16 != 0 {
                        if p + hsz + i * 2 + 2 > p + csz {
                            return Err(err("arsc.entry.bounds"));
                        }
                        let e = u16at(b, p + hsz + i * 2)? as usize;
                        Ok(if e == 0xffff { None } else { Some((i, e * 4)) })
                    } else {
                        if p + hsz + i * 4 + 4 > p + csz {
                            return Err(err("arsc.entry.bounds"));
                        }
                        let rel = u32at(b, p + hsz + i * 4)? as usize;
                        Ok(if rel == 0xffff_ffff {
                            None
                        } else {
                            Some((i, rel))
                        })
                    }
                };
                // ResTable_config sits inside the header right after the fixed
                // fields; its first u32 is its own size.
                let cfg_off = p + 20;
                let config = {
                    let csize = u32at(b, cfg_off)?.min(1024) as usize;
                    if csize >= 4 {
                        config_name(b, cfg_off, csize)
                    } else {
                        "default".to_string()
                    }
                };
                let tname = type_strings
                    .get((type_id as u32).saturating_sub(1))
                    .unwrap_or("?")
                    .to_string();
                let flags = spec_flags.get(&type_id);
                for entry_id in 0..entry_count {
                    // Entry offsets array starts right after the header.
                    let Some((entry_id, rel)) = entry_at(entry_id)? else {
                        continue; // no entry in this config
                    };
                    let eoff = p + entries_start + rel;
                    if eoff + 8 > p + csz {
                        return Err(err("arsc.entry.bounds"));
                    }
                    let _esize = u16at(b, eoff)? as usize;
                    let eflags = u16at(b, eoff + 2)?;
                    let key_idx = u32at(b, eoff + 4)?;
                    let key = key_strings.get(key_idx).unwrap_or("?").to_string();
                    let mut parent = 0u32;
                    let mut items: Vec<(u32, EntryValue)> = Vec::new();
                    let value = if eflags & ENTRY_FLAG_COMPLEX == 0 {
                        if eoff + 8 + 8 > end {
                            return Err(err("arsc.value.bounds"));
                        }
                        let data_type = b[eoff + 8 + 3];
                        let data = u32at(b, eoff + 8 + 4)?;
                        let string = if data_type == 0x03 {
                            global.get(data).map(|s| s.to_string())
                        } else {
                            None
                        };
                        Some(EntryValue {
                            data_type,
                            data,
                            string,
                        })
                    } else {
                        // ResTable_entry complex form: parent + count +
                        // count ResTable_map_entry{name, Res_value}.
                        if eoff + 16 > end {
                            return Err(err("arsc.value.bounds"));
                        }
                        parent = u32at(b, eoff + 8)?;
                        let count = u32at(b, eoff + 12)? as usize;
                        if eoff + 16 + count * 12 > end {
                            return Err(err("arsc.value.bounds"));
                        }
                        for i in 0..count {
                            let m = eoff + 16 + i * 12;
                            let item_key = u32at(b, m)?;
                            let data_type = b[m + 4 + 3];
                            let data = u32at(b, m + 8)?;
                            let string = if data_type == 0x03 {
                                global.get(data).map(|s| s.to_string())
                            } else {
                                None
                            };
                            items.push((
                                item_key,
                                EntryValue {
                                    data_type,
                                    data,
                                    string,
                                },
                            ));
                        }
                        None
                    };
                    let public = flags
                        .and_then(|f| f.get(entry_id))
                        .map(|f| f & SPEC_PUBLIC != 0)
                        .unwrap_or(false);
                    let _ = ENTRY_FLAG_PUBLIC;
                    types
                        .iter_mut()
                        .find(|t| t.type_id == type_id)
                        .map(|t| {
                            t.entries.push(ResourceEntry {
                                key: key.clone(),
                                res_id: ((package_id & 0xff) << 24)
                                    | ((type_id as u32) << 16)
                                    | entry_id as u32,
                                public,
                                value: value.clone(),
                                parent,
                                items: items.clone(),
                                config: config.clone(),
                            })
                        })
                        .unwrap_or_else(|| {
                            types.push(ResourceType {
                                name: tname.clone(),
                                type_id,
                                entries: vec![ResourceEntry {
                                    key,
                                    res_id: ((package_id & 0xff) << 24)
                                        | ((type_id as u32) << 16)
                                        | entry_id as u32,
                                    public,
                                    value,
                                    parent,
                                    items,
                                    config: config.clone(),
                                }],
                            })
                        });
                }
            }
            _ => {}
        }
        p += csz;
    }
    let _ = RES_TABLE_CONFIG_TYPE;
    Ok(ResourcePackage {
        package_id,
        name,
        types,
        key_strings: key_strings.to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn str_pool_utf8(strings: &[&str]) -> Vec<u8> {
        let n = strings.len();
        let header = 8 + 5 * 4;
        let mut offsets = Vec::new();
        let mut data = Vec::new();
        for s in strings {
            offsets.push(data.len() as u32);
            let b = s.as_bytes();
            data.push(b.len() as u8);
            data.push(b.len() as u8);
            data.extend(b);
            data.push(0);
        }
        while data.len() % 4 != 0 {
            data.push(0);
        }
        let size = header + n * 4 + data.len();
        let mut out = Vec::new();
        out.extend(RES_STRING_POOL_TYPE.to_le_bytes());
        out.extend((header as u16).to_le_bytes());
        out.extend((size as u32).to_le_bytes());
        out.extend((n as u32).to_le_bytes());
        out.extend(0u32.to_le_bytes());
        out.extend(UTF8_FLAG.to_le_bytes());
        out.extend(((header + n * 4) as u32).to_le_bytes());
        out.extend(0u32.to_le_bytes());
        for o in offsets {
            out.extend(o.to_le_bytes());
        }
        out.extend(data);
        out
    }

    #[test]
    fn parses_table_with_one_string() {
        // Global string pool: ["Hello"]
        let global = str_pool_utf8(&["Hello"]);
        // Package header: id=0x7f, name="com.test", typeStrings off right after header,
        // keyStrings right after typeStrings.
        let type_pool = str_pool_utf8(&["string"]);
        let key_pool = str_pool_utf8(&["app_name"]);
        let pkg_header_size = 8 + 4 + 256 + 4 * 5; // chunk hdr + id + name + 5 u32 fields
        let type_strings_off = pkg_header_size;
        let key_strings_off = type_strings_off + type_pool.len();
        let spec_off = key_strings_off + key_pool.len();
        // type spec: type_id=1, 1 entry, flags [SPEC_PUBLIC]
        let mut spec = Vec::new();
        spec.extend(RES_TABLE_TYPE_SPEC_TYPE.to_le_bytes());
        spec.extend(16u16.to_le_bytes()); // header
        spec.extend((16 + 4u32).to_le_bytes()); // size
        spec.push(1); // type id
        spec.extend([0, 0, 0]);
        spec.extend(1u32.to_le_bytes()); // entryCount
        spec.extend(SPEC_PUBLIC.to_le_bytes());
        let _type_off = spec_off + spec.len();
        // ResTable_type: header 8 + config(4 size only? real config is bigger;
        // emulate minimal config of size 4 won't parse config_name — use size 16)
        let mut typ = Vec::new();
        let header = 8 + 4 + 4 + 4 + 16; // type(8)+id/res0/res1(4)+entryCount(4)+entriesStart(4)+config(16)
        let entries_start = header + 4; // after 1 u32 offset
        let chunk_size = entries_start + 8 + 8; // entry header + value
        typ.extend(RES_TABLE_TYPE_TYPE.to_le_bytes());
        typ.extend((header as u16).to_le_bytes());
        typ.extend((chunk_size as u32).to_le_bytes());
        typ.push(1); // type id
        typ.extend([0, 0, 0]);
        typ.extend(1u32.to_le_bytes()); // entryCount
        typ.extend((entries_start as u32).to_le_bytes());
        typ.extend(16u32.to_le_bytes()); // config size
        typ.extend([0u8; 12]); // minimal config body
        typ.extend(0u32.to_le_bytes()); // entry 0 offset = 0
                                        // entry: size=8, flags=0, key=0; value: size=8 res0=0 type=3 data=0
        typ.extend(8u16.to_le_bytes());
        typ.extend(0u16.to_le_bytes());
        typ.extend(0u32.to_le_bytes());
        typ.extend(8u16.to_le_bytes());
        typ.push(0);
        typ.push(0x03);
        typ.extend(0u32.to_le_bytes());
        assert_eq!(typ.len(), chunk_size);

        let pkg_size = pkg_header_size + type_pool.len() + key_pool.len() + spec.len() + typ.len();
        let mut pkg = Vec::new();
        pkg.extend(RES_TABLE_PACKAGE_TYPE.to_le_bytes());
        pkg.extend((pkg_header_size as u16).to_le_bytes());
        pkg.extend((pkg_size as u32).to_le_bytes());
        pkg.extend(0x7fu32.to_le_bytes());
        let mut pname = [0u8; 256];
        for (i, u) in "com.test".encode_utf16().enumerate() {
            pname[i * 2..i * 2 + 2].copy_from_slice(&u.to_le_bytes());
        }
        pkg.extend(pname);
        pkg.extend((type_strings_off as u32).to_le_bytes());
        pkg.extend(0u32.to_le_bytes());
        pkg.extend((key_strings_off as u32).to_le_bytes());
        pkg.extend(0u32.to_le_bytes());
        pkg.extend(0u32.to_le_bytes()); // typeIdOffset
        pkg.extend(type_pool);
        pkg.extend(key_pool);
        pkg.extend(spec);
        pkg.extend(typ);
        assert_eq!(pkg.len(), pkg_size);

        let mut table = Vec::new();
        let total = 12 + global.len() + pkg.len();
        table.extend(RES_TABLE_TYPE.to_le_bytes());
        table.extend(12u16.to_le_bytes());
        table.extend((total as u32).to_le_bytes());
        table.extend(1u32.to_le_bytes());
        table.extend(global);
        table.extend(pkg);

        let pkgs = parse_arsc(&table).unwrap();
        assert_eq!(pkgs.len(), 1);
        assert_eq!(pkgs[0].package_id, 0x7f);
        assert_eq!(pkgs[0].name, "com.test");
        assert_eq!(pkgs[0].types[0].name, "string");
        let e = &pkgs[0].types[0].entries[0];
        assert_eq!(e.key, "app_name");
        assert_eq!(e.res_id, 0x7f01_0000);
        assert!(e.public);
        assert_eq!(e.value.as_ref().unwrap().string.as_deref(), Some("Hello"));
    }
}

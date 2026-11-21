//! ZIP central directory inspection for APK input (design doc §03 step 1).
//!
//! Everything is validated *before* any decompression: entry counts, sizes,
//! duplicates, absolute paths and path traversal. Exceeding limits rejects
//! the whole archive.

use ovelo_error::{ErrorCategory, OveloError, Phase, Result};

const EOCD_SIG: u32 = 0x0605_4b50;
const EOCD_LEN: usize = 22;
const EOCD_MAX_SEARCH: usize = 64 * 1024 + EOCD_LEN;
const CD_ENTRY_SIG: u32 = 0x0201_4b50;
const LOCAL_SIG: u32 = 0x0403_4b50;

/// Safety limits enforced while reading the central directory.
#[derive(Debug, Clone)]
pub struct ZipLimits {
    /// Maximum number of entries.
    pub max_entries: usize,
    /// Maximum total uncompressed size in bytes.
    pub max_uncompressed_total: u64,
    /// Maximum single-entry uncompressed size.
    pub max_uncompressed_entry: u64,
    /// Maximum compressed:uncompressed ratio guard is not applied to entries
    /// that are tiny anyway; entries larger than this are bounded by
    /// `max_uncompressed_entry` instead.
    pub max_comment_len: usize,
}

impl Default for ZipLimits {
    fn default() -> Self {
        Self {
            max_entries: 65_535,
            max_uncompressed_total: 4 * 1024 * 1024 * 1024,
            max_uncompressed_entry: 1024 * 1024 * 1024,
            max_comment_len: 64 * 1024,
        }
    }
}

fn zip_err(cause: impl Into<String>) -> OveloError {
    OveloError::new(ErrorCategory::PkgInvalid, Phase::Install, cause)
}

/// One central directory record.
#[derive(Debug, Clone)]
pub struct CentralEntry {
    /// Entry path (normalized, `/`-separated).
    pub name: String,
    /// Compression method (0 = stored, 8 = deflate).
    pub method: u16,
    /// CRC-32 of uncompressed data.
    pub crc32: u32,
    /// Compressed size.
    pub compressed_size: u64,
    /// Uncompressed size.
    pub uncompressed_size: u64,
    /// Offset of the local file header.
    pub local_header_offset: u64,
    /// GP flag bit 0x08: sizes live in a data descriptor after the data.
    pub has_data_descriptor: bool,
    /// True when the GP flag marks the name as UTF-8.
    pub utf8_name: bool,
}

/// Location of the central directory inside the archive.
#[derive(Debug, Clone, Copy)]
pub struct CentralDirectory {
    /// Absolute offset of the first central directory record.
    pub offset: u64,
    /// Total size of the central directory.
    pub size: u64,
    /// Number of records.
    pub entries: u64,
}

fn find_eocd(bytes: &[u8]) -> Result<(usize, u64)> {
    if bytes.len() < EOCD_LEN {
        return Err(zip_err("zip.eocd.missing"));
    }
    let start = bytes.len().saturating_sub(EOCD_MAX_SEARCH);
    for i in (start..=bytes.len() - EOCD_LEN).rev() {
        if u32::from_le_bytes(bytes[i..i + 4].try_into().unwrap()) == EOCD_SIG {
            let comment_len =
                u16::from_le_bytes(bytes[i + 20..i + 22].try_into().unwrap()) as usize;
            // The comment must reach the end of file exactly.
            if i + EOCD_LEN + comment_len == bytes.len() {
                let cd_offset =
                    u32::from_le_bytes(bytes[i + 16..i + 20].try_into().unwrap()) as u64;
                return Ok((i, cd_offset));
            }
        }
    }
    Err(zip_err("zip.eocd.missing"))
}

/// Find the end-of-central-directory record and the CD offset.
pub fn find_central_directory(bytes: &[u8]) -> Result<CentralDirectory> {
    let (eocd, cd_offset) = find_eocd(bytes)?;
    let entries16 = u16::from_le_bytes(bytes[eocd + 10..eocd + 12].try_into().unwrap());
    let size = u32::from_le_bytes(bytes[eocd + 12..eocd + 16].try_into().unwrap()) as u64;
    if entries16 == 0xffff || size as u32 == 0xffff_ffff || cd_offset as u32 == 0xffff_ffff {
        return Err(zip_err("zip.zip64.unsupported"));
    }
    let end = cd_offset
        .checked_add(size)
        .filter(|e| *e <= eocd as u64)
        .ok_or_else(|| zip_err("zip.central.out_of_bounds"))?;
    let _ = end;
    Ok(CentralDirectory {
        offset: cd_offset,
        size,
        entries: entries16 as u64,
    })
}

fn check_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(zip_err("zip.name.empty"));
    }
    let bytes = name.as_bytes();
    if bytes.contains(&0) {
        return Err(zip_err("zip.name.nul"));
    }
    if name.starts_with('/') || name.contains('\\') {
        return Err(zip_err("zip.name.absolute_or_backslash"));
    }
    let mut depth: i32 = 0;
    for seg in name.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                depth -= 1;
                if depth < 0 {
                    return Err(zip_err("zip.name.traversal"));
                }
            }
            _ => depth += 1,
        }
    }
    if depth <= 0 {
        return Err(zip_err("zip.name.traversal"));
    }
    // Windows drive-letter or UNC style prefix.
    if name.len() >= 2 && name.as_bytes()[1] == b':' && name.as_bytes()[0].is_ascii_alphabetic() {
        return Err(zip_err("zip.name.drive_letter"));
    }
    Ok(())
}

/// Parse and validate the central directory.
pub fn parse_central_directory(
    bytes: &[u8],
    limits: &ZipLimits,
) -> Result<(CentralDirectory, Vec<CentralEntry>)> {
    let cd = find_central_directory(bytes)?;
    if cd.entries as usize > limits.max_entries {
        return Err(zip_err("zip.too_many_entries"));
    }
    let mut entries = Vec::with_capacity(cd.entries as usize);
    let mut pos = cd.offset as usize;
    let end = (cd.offset + cd.size) as usize;
    let mut total_uncompressed: u64 = 0;

    for _ in 0..cd.entries {
        if pos + 46 > end {
            return Err(zip_err("zip.central.truncated"));
        }
        let rec = &bytes[pos..pos + 46];
        if u32::from_le_bytes(rec[0..4].try_into().unwrap()) != CD_ENTRY_SIG {
            return Err(zip_err("zip.central.bad_signature"));
        }
        let flags = u16::from_le_bytes(rec[8..10].try_into().unwrap());
        let method = u16::from_le_bytes(rec[10..12].try_into().unwrap());
        let crc32 = u32::from_le_bytes(rec[16..20].try_into().unwrap());
        let compressed = u32::from_le_bytes(rec[20..24].try_into().unwrap()) as u64;
        let uncompressed = u32::from_le_bytes(rec[24..28].try_into().unwrap()) as u64;
        let name_len = u16::from_le_bytes(rec[28..30].try_into().unwrap()) as usize;
        let extra_len = u16::from_le_bytes(rec[30..32].try_into().unwrap()) as usize;
        let comment_len = u16::from_le_bytes(rec[32..34].try_into().unwrap()) as usize;
        let lh_offset = u32::from_le_bytes(rec[42..46].try_into().unwrap()) as u64;

        if comment_len > limits.max_comment_len {
            return Err(zip_err("zip.comment.too_long"));
        }
        let name_end = pos + 46 + name_len;
        if name_end > end {
            return Err(zip_err("zip.central.truncated_name"));
        }
        let name_bytes = &bytes[pos + 46..name_end];
        let name = String::from_utf8_lossy(name_bytes).into_owned();
        check_name(&name)?;

        if compressed == u32::MAX as u64 || uncompressed == u32::MAX as u64 {
            return Err(zip_err("zip.zip64.unsupported"));
        }
        if uncompressed > limits.max_uncompressed_entry {
            return Err(zip_err("zip.entry.too_large"));
        }
        total_uncompressed = total_uncompressed
            .checked_add(uncompressed)
            .filter(|t| *t <= limits.max_uncompressed_total)
            .ok_or_else(|| zip_err("zip.total.too_large"))?;
        if lh_offset as usize >= bytes.len() {
            return Err(zip_err("zip.local.out_of_bounds"));
        }
        match method {
            0 | 8 => {}
            _ => return Err(zip_err("zip.method.unsupported")),
        }

        entries.push(CentralEntry {
            name,
            method,
            crc32,
            compressed_size: compressed,
            uncompressed_size: uncompressed,
            local_header_offset: lh_offset,
            has_data_descriptor: flags & 0x0008 != 0,
            utf8_name: flags & 0x0800 != 0,
        });

        pos = name_end + extra_len + comment_len;
    }

    if pos > end {
        return Err(zip_err("zip.central.overrun"));
    }
    let mut seen = std::collections::HashSet::with_capacity(entries.len());
    for e in &entries {
        if !seen.insert(e.name.clone()) {
            return Err(zip_err("zip.duplicate_entry"));
        }
    }
    Ok((cd, entries))
}

/// Decompress one entry's contents.
pub fn read_entry(bytes: &[u8], entry: &CentralEntry) -> Result<Vec<u8>> {
    let lh = entry.local_header_offset as usize;
    if lh + 30 > bytes.len()
        || u32::from_le_bytes(bytes[lh..lh + 4].try_into().unwrap()) != LOCAL_SIG
    {
        return Err(zip_err("zip.local.bad_signature"));
    }
    let name_len = u16::from_le_bytes(bytes[lh + 26..lh + 28].try_into().unwrap()) as usize;
    let extra_len = u16::from_le_bytes(bytes[lh + 28..lh + 30].try_into().unwrap()) as usize;
    let data_start = lh + 30 + name_len + extra_len;
    let data_end = data_start
        .checked_add(entry.compressed_size as usize)
        .filter(|e| *e <= bytes.len())
        .ok_or_else(|| zip_err("zip.data.out_of_bounds"))?;
    let data = &bytes[data_start..data_end];

    let out = match entry.method {
        0 => data.to_vec(),
        8 => {
            // Inflate into at most the declared size (hard-capped); a lying
            // header must not let an entry expand beyond its CD size.
            let cap = (entry.uncompressed_size as usize).min(MAX_INFLATE);
            miniz_oxide::inflate::decompress_to_vec_with_limit(data, cap + 1)
                .map_err(|_| zip_err("zip.deflate.corrupt"))?
        }
        _ => return Err(zip_err("zip.method.unsupported")),
    };
    if out.len() as u64 != entry.uncompressed_size {
        return Err(zip_err("zip.size.mismatch"));
    }
    if crc32fast::hash(&out) != entry.crc32 {
        return Err(zip_err("zip.crc.mismatch"));
    }
    Ok(out)
}

/// Hard cap on inflated entry size (compressed-bomb guard).
const MAX_INFLATE: usize = 1 << 30;

/// Byte range of an entry's compressed data (for signature verification,
/// which digests the raw archive, not decompressed contents).
pub fn entry_data_range(entry: &CentralEntry, bytes: &[u8]) -> Result<std::ops::Range<usize>> {
    let lh = entry.local_header_offset as usize;
    if lh + 30 > bytes.len() {
        return Err(zip_err("zip.local.out_of_bounds"));
    }
    let name_len = u16::from_le_bytes(bytes[lh + 26..lh + 28].try_into().unwrap()) as usize;
    let extra_len = u16::from_le_bytes(bytes[lh + 28..lh + 30].try_into().unwrap()) as usize;
    let start = lh + 30 + name_len + extra_len;
    let end = start + entry.compressed_size as usize;
    if end > bytes.len() {
        return Err(zip_err("zip.data.out_of_bounds"));
    }
    Ok(start..end)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn le16(v: u16) -> [u8; 2] {
        v.to_le_bytes()
    }
    fn le32(v: u32) -> [u8; 4] {
        v.to_le_bytes()
    }

    /// Build a minimal zip with stored entries.
    fn make_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut cds = Vec::new();
        for (name, data) in entries {
            let offset = out.len() as u32;
            let crc = crc32fast::hash(data);
            out.extend(le32(LOCAL_SIG));
            out.extend(le16(20)); // version
            out.extend(le16(0)); // flags
            out.extend(le16(0)); // method stored
            out.extend(le16(0));
            out.extend(le16(0));
            out.extend(le32(crc));
            out.extend(le32(data.len() as u32));
            out.extend(le32(data.len() as u32));
            out.extend(le16(name.len() as u16));
            out.extend(le16(0));
            out.extend(name.as_bytes());
            out.extend(*data);

            let mut cd = Vec::new();
            cd.extend(le32(CD_ENTRY_SIG));
            cd.extend(le16(20));
            cd.extend(le16(20));
            cd.extend(le16(0));
            cd.extend(le16(0));
            cd.extend(le16(0));
            cd.extend(le16(0));
            cd.extend(le32(crc));
            cd.extend(le32(data.len() as u32));
            cd.extend(le32(data.len() as u32));
            cd.extend(le16(name.len() as u16));
            cd.extend(le16(0));
            cd.extend(le16(0));
            cd.extend(le16(0));
            cd.extend(le16(0));
            cd.extend(le32(0));
            cd.extend(le32(offset));
            cd.extend(name.as_bytes());
            cds.push(cd);
        }
        let cd_offset = out.len() as u32;
        let mut cd_size = 0u32;
        for cd in &cds {
            out.extend(cd);
            cd_size += cd.len() as u32;
        }
        out.extend(le32(EOCD_SIG));
        out.extend(le16(0));
        out.extend(le16(0));
        out.extend(le16(entries.len() as u16));
        out.extend(le16(entries.len() as u16));
        out.extend(le32(cd_size));
        out.extend(le32(cd_offset));
        out.extend(le16(0));
        out
    }

    #[test]
    fn parses_and_reads_stored_entries() {
        let zip = make_zip(&[("a.txt", b"hello"), ("dir/b.bin", b"\x01\x02")]);
        let (_cd, entries) = parse_central_directory(&zip, &ZipLimits::default()).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(read_entry(&zip, &entries[0]).unwrap(), b"hello");
        assert_eq!(read_entry(&zip, &entries[1]).unwrap(), b"\x01\x02");
    }

    #[test]
    fn rejects_traversal() {
        for bad in ["../evil", "a/../../b", "/abs", "a\\b", "C:/x"] {
            let zip = make_zip(&[(bad, b"x")]);
            assert!(
                parse_central_directory(&zip, &ZipLimits::default()).is_err(),
                "accepted {bad}"
            );
        }
    }

    #[test]
    fn rejects_duplicates() {
        let zip = make_zip(&[("same", b"1"), ("same", b"2")]);
        assert!(parse_central_directory(&zip, &ZipLimits::default()).is_err());
    }

    #[test]
    fn reads_deflate() {
        use flate2::write::DeflateEncoder;
        use flate2::Compression;
        use std::io::Write;
        let raw = b"deflate me ".repeat(100);
        let mut e = DeflateEncoder::new(Vec::new(), Compression::fast());
        e.write_all(&raw).unwrap();
        let comp = e.finish().unwrap();

        // Build a zip with a deflate entry manually.
        let mut out = Vec::new();
        let name = "d.txt";
        let crc = crc32fast::hash(&raw);
        let offset = out.len() as u32;
        out.extend(le32(LOCAL_SIG));
        out.extend(le16(20));
        out.extend(le16(0));
        out.extend(le16(8));
        out.extend(le16(0));
        out.extend(le16(0));
        out.extend(le32(crc));
        out.extend(le32(comp.len() as u32));
        out.extend(le32(raw.len() as u32));
        out.extend(le16(name.len() as u16));
        out.extend(le16(0));
        out.extend(name.as_bytes());
        out.extend(&comp);
        let cd_off = out.len() as u32;
        out.extend(le32(CD_ENTRY_SIG));
        out.extend(le16(20));
        out.extend(le16(20));
        out.extend(le16(0));
        out.extend(le16(8));
        out.extend(le16(0));
        out.extend(le16(0));
        out.extend(le32(crc));
        out.extend(le32(comp.len() as u32));
        out.extend(le32(raw.len() as u32));
        out.extend(le16(name.len() as u16));
        out.extend(le16(0));
        out.extend(le16(0));
        out.extend(le16(0));
        out.extend(le16(0));
        out.extend(le32(0));
        out.extend(le32(offset));
        out.extend(name.as_bytes());
        let cd_size = out.len() as u32 - cd_off;
        out.extend(le32(EOCD_SIG));
        out.extend(le16(0));
        out.extend(le16(0));
        out.extend(le16(1));
        out.extend(le16(1));
        out.extend(le32(cd_size));
        out.extend(le32(cd_off));
        out.extend(le16(0));

        let (_cd, entries) = parse_central_directory(&out, &ZipLimits::default()).unwrap();
        assert_eq!(read_entry(&out, &entries[0]).unwrap(), raw);
    }

    #[test]
    fn rejects_bad_crc() {
        let mut zip = make_zip(&[("a.txt", b"hello")]);
        // Corrupt one data byte.
        let pos = 30 + 5; // local header 30 + name len
        zip[pos] ^= 0xff;
        let (_cd, entries) = parse_central_directory(&zip, &ZipLimits::default()).unwrap();
        assert!(read_entry(&zip, &entries[0]).is_err());
    }

    #[test]
    fn enforces_limits() {
        let zip = make_zip(&[("a", b"x"), ("b", b"y")]);
        let limits = ZipLimits {
            max_entries: 1,
            ..Default::default()
        };
        assert!(parse_central_directory(&zip, &limits).is_err());
    }
}

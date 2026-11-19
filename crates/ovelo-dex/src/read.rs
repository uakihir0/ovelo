//! Primitive DEX readers: byte slices, uleb128/sleb128, MUTF-8 strings.

use ovelo_error::{ErrorCategory, OveloError, Phase, Result};

pub(crate) fn err(cause: impl Into<String>) -> OveloError {
    OveloError::new(ErrorCategory::DexUnsupported, Phase::DexLoad, cause)
}

#[derive(Clone, Copy)]
pub struct R<'a> {
    pub b: &'a [u8],
    pub p: usize,
    pub end: usize,
}

impl<'a> R<'a> {
    pub fn new(b: &'a [u8]) -> Self {
        Self { b, p: 0, end: b.len() }
    }

    pub fn at(b: &'a [u8], p: usize) -> Self {
        Self { b, p, end: b.len() }
    }

    pub fn u8(&mut self) -> Result<u8> {
        if self.p >= self.end {
            return Err(err("dex.read.bounds"));
        }
        let v = self.b[self.p];
        self.p += 1;
        Ok(v)
    }

    pub fn u16(&mut self) -> Result<u16> {
        if self.p + 2 > self.end {
            return Err(err("dex.read.bounds"));
        }
        let v = u16::from_le_bytes(self.b[self.p..self.p + 2].try_into().unwrap());
        self.p += 2;
        Ok(v)
    }

    #[allow(dead_code)]
    pub fn i16(&mut self) -> Result<i16> {
        Ok(self.u16()? as i16)
    }

    pub fn u32(&mut self) -> Result<u32> {
        if self.p + 4 > self.end {
            return Err(err("dex.read.bounds"));
        }
        let v = u32::from_le_bytes(self.b[self.p..self.p + 4].try_into().unwrap());
        self.p += 4;
        Ok(v)
    }

    #[allow(dead_code)]
    pub fn u64(&mut self) -> Result<u64> {
        if self.p + 8 > self.end {
            return Err(err("dex.read.bounds"));
        }
        let v = u64::from_le_bytes(self.b[self.p..self.p + 8].try_into().unwrap());
        self.p += 8;
        Ok(v)
    }

    pub fn uleb128(&mut self) -> Result<u32> {
        let mut result = 0u32;
        for i in 0..5 {
            let byte = self.u8()?;
            if i == 4 && byte & 0xf0 != 0 {
                return Err(err("dex.uleb.overflow"));
            }
            result |= ((byte & 0x7f) as u32) << (7 * i);
            if byte & 0x80 == 0 {
                return Ok(result);
            }
        }
        Err(err("dex.uleb.overflow"))
    }

    /// uleb128p1: uleb128 value minus one; 0 encodes -1 (no index).
    #[allow(dead_code)]
    pub fn uleb128p1(&mut self) -> Result<i64> {
        Ok(self.uleb128()? as i64 - 1)
    }

    pub fn sleb128(&mut self) -> Result<i32> {
        let mut result = 0i32;
        let mut shift = 0;
        loop {
            let byte = self.u8()?;
            result |= ((byte & 0x7f) as i32) << shift;
            shift += 7;
            if byte & 0x80 == 0 {
                if shift < 32 && byte & 0x40 != 0 {
                    result |= -1i32 << shift;
                }
                return Ok(result);
            }
            if shift >= 35 {
                return Err(err("dex.sleb.overflow"));
            }
        }
    }

    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.p + n > self.end {
            return Err(err("dex.read.bounds"));
        }
        let s = &self.b[self.p..self.p + n];
        self.p += n;
        Ok(s)
    }
}

/// Decode MUTF-8 (modified UTF-8: NUL as 0xC0 0x80, no 4-byte sequences —
/// supplementary chars arrive as surrogate pairs in UTF-16 units).
pub fn mutf8(bytes: &[u8]) -> Result<String> {
    let mut units: Vec<u16> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == 0 {
            break;
        }
        if b & 0x80 == 0 {
            units.push(b as u16);
            i += 1;
        } else if b & 0xe0 == 0xc0 {
            if i + 1 >= bytes.len() || bytes[i + 1] & 0xc0 != 0x80 {
                return Err(err("dex.mutf8.truncated"));
            }
            let v = (((b & 0x1f) as u16) << 6) | (bytes[i + 1] & 0x3f) as u16;
            units.push(v);
            i += 2;
        } else if b & 0xf0 == 0xe0 {
            if i + 2 >= bytes.len()
                || bytes[i + 1] & 0xc0 != 0x80
                || bytes[i + 2] & 0xc0 != 0x80
            {
                return Err(err("dex.mutf8.truncated"));
            }
            let v = (((b & 0x0f) as u16) << 12)
                | (((bytes[i + 1] & 0x3f) as u16) << 6)
                | (bytes[i + 2] & 0x3f) as u16;
            units.push(v);
            i += 3;
        } else {
            return Err(err("dex.mutf8.invalid"));
        }
    }
    Ok(String::from_utf16_lossy(&units))
}

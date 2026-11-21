//! `android.os.Bundle` — typed key/value parcel used for instance state,
//! intents extras, and provider results (design doc §08 restore flow).
//!
//! Serializes to a compact binary so state survives process death; the
//! format is internal (magic `OVB1`), not Android's Parcel wire format.

use std::collections::BTreeMap;

use ovelo_error::OveloError;

use crate::err;

/// A typed bundle value (Parcelable subset).
#[derive(Debug, Clone, PartialEq)]
pub enum BValue {
    /// `putInt`.
    Int(i32),
    /// `putLong`.
    Long(i64),
    /// `putFloat`.
    Float(f32),
    /// `putDouble`.
    Double(f64),
    /// `putBoolean`.
    Bool(bool),
    /// `putString`.
    Str(String),
    /// `putStringArrayList`.
    StrList(Vec<String>),
    /// `putByteArray`.
    Bytes(Vec<u8>),
}

/// `Bundle` — an ordered typed map.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Bundle {
    map: BTreeMap<String, BValue>,
}

impl Bundle {
    /// Empty bundle.
    pub fn new() -> Self {
        Self::default()
    }

    /// `put*`.
    pub fn put(&mut self, key: impl Into<String>, v: BValue) {
        self.map.insert(key.into(), v);
    }

    /// `get*`.
    pub fn get(&self, key: &str) -> Option<&BValue> {
        self.map.get(key)
    }

    /// `containsKey`.
    pub fn contains(&self, key: &str) -> bool {
        self.map.contains_key(key)
    }

    /// `remove`.
    pub fn remove(&mut self, key: &str) {
        self.map.remove(key);
    }

    /// `size()`.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// `isEmpty()`.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Iterate entries in key order.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &BValue)> {
        self.map.iter()
    }

    /// Serialize (`OVB1` + entries: tag,keylen,key,vallen,val).
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = b"OVB1".to_vec();
        out.extend_from_slice(&(self.map.len() as u32).to_le_bytes());
        for (k, v) in &self.map {
            out.extend_from_slice(&(k.len() as u32).to_le_bytes());
            out.extend_from_slice(k.as_bytes());
            let (tag, payload) = encode(v);
            out.push(tag);
            out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            out.extend_from_slice(&payload);
        }
        out
    }

    /// Deserialize; trailing bytes are an error.
    pub fn from_bytes(b: &[u8]) -> Result<Self, OveloError> {
        let mut r = Rd { b, i: 0 };
        if r.take(4)? != b"OVB1" {
            return Err(err("framework.bundle.magic"));
        }
        let n = r.u32()?;
        let mut map = BTreeMap::new();
        for _ in 0..n {
            let klen = r.u32()? as usize;
            let key = String::from_utf8(r.take(klen)?.to_vec())
                .map_err(|_| err("framework.bundle.utf8"))?;
            let tag = r.u8()?;
            let vlen = r.u32()? as usize;
            let raw = r.take(vlen)?;
            let v = decode(tag, raw)?;
            map.insert(key, v);
        }
        if r.i != b.len() {
            return Err(err("framework.bundle.trailing"));
        }
        Ok(Self { map })
    }
}

fn encode(v: &BValue) -> (u8, Vec<u8>) {
    match v {
        BValue::Int(x) => (1, x.to_le_bytes().to_vec()),
        BValue::Long(x) => (2, x.to_le_bytes().to_vec()),
        BValue::Float(x) => (3, x.to_le_bytes().to_vec()),
        BValue::Double(x) => (4, x.to_le_bytes().to_vec()),
        BValue::Bool(x) => (5, vec![*x as u8]),
        BValue::Str(s) => (6, s.as_bytes().to_vec()),
        BValue::StrList(l) => {
            let mut p = (l.len() as u32).to_le_bytes().to_vec();
            for s in l {
                p.extend_from_slice(&(s.len() as u32).to_le_bytes());
                p.extend_from_slice(s.as_bytes());
            }
            (7, p)
        }
        BValue::Bytes(b) => (8, b.clone()),
    }
}

fn decode(tag: u8, raw: &[u8]) -> Result<BValue, OveloError> {
    let bad = || err("framework.bundle.decode");
    let utf = |b: &[u8]| String::from_utf8(b.to_vec()).map_err(|_| err("framework.bundle.utf8"));
    Ok(match tag {
        1 => BValue::Int(i32::from_le_bytes(raw.try_into().map_err(|_| bad())?)),
        2 => BValue::Long(i64::from_le_bytes(raw.try_into().map_err(|_| bad())?)),
        3 => BValue::Float(f32::from_le_bytes(raw.try_into().map_err(|_| bad())?)),
        4 => BValue::Double(f64::from_le_bytes(raw.try_into().map_err(|_| bad())?)),
        5 => BValue::Bool(*raw.first().ok_or_else(bad)? != 0),
        6 => BValue::Str(utf(raw)?),
        7 => {
            let mut r = Rd { b: raw, i: 0 };
            let n = r.u32()?;
            let mut l = Vec::with_capacity(n.min(1 << 20) as usize);
            for _ in 0..n {
                let len = r.u32()? as usize;
                l.push(utf(r.take(len)?)?);
            }
            BValue::StrList(l)
        }
        8 => BValue::Bytes(raw.to_vec()),
        _ => return Err(err("framework.bundle.tag")),
    })
}

struct Rd<'a> {
    b: &'a [u8],
    i: usize,
}

impl Rd<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], OveloError> {
        let s = self
            .b
            .get(self.i..self.i.saturating_add(n))
            .ok_or_else(|| err("framework.bundle.eof"))?;
        self.i += n;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, OveloError> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Result<u32, OveloError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let mut b = Bundle::new();
        b.put("i", BValue::Int(-3));
        b.put("s", BValue::Str("日本".into()));
        b.put("l", BValue::StrList(vec!["a".into(), "b".into()]));
        b.put("z", BValue::Bytes(vec![0, 255]));
        b.put("d", BValue::Double(1.5));
        b.put("t", BValue::Bool(true));
        let back = Bundle::from_bytes(&b.to_bytes()).unwrap();
        assert_eq!(b, back);
        assert_eq!(b.to_bytes().len() - 8, back.to_bytes().len() - 8);
    }

    #[test]
    fn rejects_junk() {
        assert!(Bundle::from_bytes(b"NOPE").is_err());
        let mut b = Bundle::new();
        b.put("x", BValue::Int(1));
        let mut raw = b.to_bytes();
        raw.push(9); // trailing byte
        assert!(Bundle::from_bytes(&raw).is_err());
    }
}

//! GGUF header metadata reader (key/value section only).
//!
//! Reads every metadata key with its value; arrays longer than
//! [`ARRAY_PREVIEW`] elements (the tokenizer vocabulary) are summarized
//! as `{type, len, head}` instead of materialized. Works on a prefix of
//! the file: [`GgufMetaError::Incomplete`] asks the caller for a longer
//! one.

use std::collections::BTreeMap;

use serde_json::{json, Value};

/// Arrays up to this length are returned in full.
pub const ARRAY_PREVIEW: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GgufMetaError {
    NotGguf,
    UnsupportedVersion(u32),
    /// The prefix ended inside the metadata section.
    Incomplete,
    Malformed(String),
}

impl std::fmt::Display for GgufMetaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GgufMetaError::NotGguf => write!(f, "not a GGUF file (bad magic)"),
            GgufMetaError::UnsupportedVersion(v) => write!(f, "unsupported GGUF version {v}"),
            GgufMetaError::Incomplete => write!(f, "header extends past the bytes read"),
            GgufMetaError::Malformed(m) => write!(f, "malformed GGUF header: {m}"),
        }
    }
}

impl std::error::Error for GgufMetaError {}

#[derive(Debug, Clone, PartialEq)]
pub struct GgufMeta {
    pub version: u32,
    pub tensor_count: u64,
    pub kv: BTreeMap<String, Value>,
}

struct Rd<'a> {
    b: &'a [u8],
    p: usize,
}

impl Rd<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], GgufMetaError> {
        if self.p.checked_add(n).is_none_or(|e| e > self.b.len()) {
            return Err(GgufMetaError::Incomplete);
        }
        let s = &self.b[self.p..self.p + n];
        self.p += n;
        Ok(s)
    }
    fn u32(&mut self) -> Result<u32, GgufMetaError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, GgufMetaError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn string(&mut self) -> Result<String, GgufMetaError> {
        let n = self.u64()?;
        if n > 1 << 30 {
            return Err(GgufMetaError::Malformed(format!("string length {n}")));
        }
        Ok(String::from_utf8_lossy(self.take(n as usize)?).into_owned())
    }
    fn scalar(&mut self, ty: u32) -> Result<Value, GgufMetaError> {
        Ok(match ty {
            0 => json!(self.take(1)?[0]),
            1 => json!(self.take(1)?[0] as i8),
            2 => json!(u16::from_le_bytes(self.take(2)?.try_into().unwrap())),
            3 => json!(i16::from_le_bytes(self.take(2)?.try_into().unwrap())),
            4 => json!(self.u32()?),
            5 => json!(i32::from_le_bytes(self.take(4)?.try_into().unwrap())),
            6 => json!(f32::from_le_bytes(self.take(4)?.try_into().unwrap())),
            7 => json!(self.take(1)?[0] != 0),
            8 => json!(self.string()?),
            10 => json!(self.u64()?),
            11 => json!(i64::from_le_bytes(self.take(8)?.try_into().unwrap())),
            12 => json!(f64::from_le_bytes(self.take(8)?.try_into().unwrap())),
            other => return Err(GgufMetaError::Malformed(format!("value type {other}"))),
        })
    }
    fn value(&mut self, ty: u32) -> Result<Value, GgufMetaError> {
        if ty != 9 {
            return self.scalar(ty);
        }
        let ety = self.u32()?;
        let len = self.u64()?;
        if ety == 9 {
            return Err(GgufMetaError::Malformed("nested arrays".into()));
        }
        let mut head = Vec::new();
        for i in 0..len {
            let v = self.scalar(ety)?;
            if (i as usize) < ARRAY_PREVIEW.max(8) {
                head.push(v);
            }
        }
        if len as usize <= ARRAY_PREVIEW {
            Ok(Value::Array(head))
        } else {
            head.truncate(8);
            Ok(json!({ "type": type_name(ety), "len": len, "head": head }))
        }
    }
}

fn type_name(t: u32) -> &'static str {
    match t {
        0 => "u8",
        1 => "i8",
        2 => "u16",
        3 => "i16",
        4 => "u32",
        5 => "i32",
        6 => "f32",
        7 => "bool",
        8 => "string",
        10 => "u64",
        11 => "i64",
        12 => "f64",
        _ => "?",
    }
}

/// Parse the metadata of a GGUF file prefix.
pub fn read_metadata(bytes: &[u8]) -> Result<GgufMeta, GgufMetaError> {
    let mut r = Rd { b: bytes, p: 0 };
    if r.take(4).map_err(|_| GgufMetaError::NotGguf)? != b"GGUF" {
        return Err(GgufMetaError::NotGguf);
    }
    let version = r.u32()?;
    let (tensor_count, kv_count) = match version {
        1 => (r.u32()? as u64, r.u32()? as u64),
        2 | 3 => (r.u64()?, r.u64()?),
        v => return Err(GgufMetaError::UnsupportedVersion(v)),
    };
    if kv_count > 1 << 20 {
        return Err(GgufMetaError::Malformed(format!("{kv_count} keys")));
    }
    let mut kv = BTreeMap::new();
    for _ in 0..kv_count {
        let key = r.string()?;
        let ty = r.u32()?;
        let v = r.value(ty)?;
        kv.insert(key, v);
    }
    Ok(GgufMeta {
        version,
        tensor_count,
        kv,
    })
}

/// Keys matching any filter (substring, case-insensitive); all when
/// `filters` is empty.
pub fn filter_keys(meta: &GgufMeta, filters: &[String]) -> BTreeMap<String, Value> {
    meta.kv
        .iter()
        .filter(|(k, _)| {
            filters.is_empty()
                || filters
                    .iter()
                    .any(|f| k.to_ascii_lowercase().contains(&f.to_ascii_lowercase()))
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// Build a GGUF header (tests and the fake-executable fixtures).
#[doc(hidden)]
pub fn build_header(kv: &[(&str, Value)]) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(b"GGUF");
    b.extend_from_slice(&3u32.to_le_bytes());
    b.extend_from_slice(&0u64.to_le_bytes());
    b.extend_from_slice(&(kv.len() as u64).to_le_bytes());
    let s = |b: &mut Vec<u8>, s: &str| {
        b.extend_from_slice(&(s.len() as u64).to_le_bytes());
        b.extend_from_slice(s.as_bytes());
    };
    for (k, v) in kv {
        s(&mut b, k);
        match v {
            Value::String(x) => {
                b.extend_from_slice(&8u32.to_le_bytes());
                s(&mut b, x);
            }
            Value::Number(n) => {
                b.extend_from_slice(&4u32.to_le_bytes());
                b.extend_from_slice(&(n.as_u64().unwrap_or(0) as u32).to_le_bytes());
            }
            Value::Array(a) => {
                b.extend_from_slice(&9u32.to_le_bytes());
                b.extend_from_slice(&8u32.to_le_bytes());
                b.extend_from_slice(&(a.len() as u64).to_le_bytes());
                for x in a {
                    s(&mut b, x.as_str().unwrap_or(""));
                }
            }
            _ => {
                b.extend_from_slice(&7u32.to_le_bytes());
                b.push(v.as_bool().unwrap_or(false) as u8);
            }
        }
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_scalars_strings_and_summarizes_long_arrays() {
        let vocab: Vec<Value> = (0..100).map(|i| json!(format!("t{i}"))).collect();
        let bytes = build_header(&[
            ("general.architecture", json!("qwen3moe")),
            ("qwen3moe.attention.key_length", json!(128)),
            ("qwen3moe.attention.head_count_kv", json!(4)),
            ("tokenizer.ggml.tokens", Value::Array(vocab)),
        ]);
        let m = read_metadata(&bytes).unwrap();
        assert_eq!(m.kv["qwen3moe.attention.key_length"], json!(128));
        assert_eq!(m.kv["tokenizer.ggml.tokens"]["len"], json!(100));
        let f = filter_keys(&m, &["key_length".into(), "head_count_kv".into()]);
        assert_eq!(f.len(), 2);
        assert_eq!(
            read_metadata(&bytes[..bytes.len() - 3]),
            Err(GgufMetaError::Incomplete)
        );
        assert_eq!(read_metadata(b"nope"), Err(GgufMetaError::NotGguf));
    }
}

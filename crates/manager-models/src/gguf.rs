//! GGUF header parsing: the tensor directory and the metadata the
//! memory-requirement calculation needs (see [`super::memory`]).
//!
//! A GGUF file opens with a header — magic, version, tensor and
//! metadata counts — followed by the metadata key/value pairs and
//! then one info record per tensor (name, dimensions, element type,
//! offset). Tensor data sizes follow from the dimensions and the
//! ggml element type's block geometry, so the total weight bytes and
//! the routed-expert share are exact sums over the directory, with
//! no need to read the tensor data itself. Only the header region is
//! parsed (a few MiB even for large models); the caller reads a
//! prefix of the file and retries with a longer prefix when the
//! parser reports [`GgufError::Incomplete`].

/// KV-cache geometry derived from GGUF metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KvGeometry {
    /// Layers that carry a KV cache. Hybrid linear-attention
    /// architectures (Qwen3-Next and relatives) keep KV in one of
    /// every `full_attention_interval` layers; the parser folds that
    /// interval into this count.
    pub kv_layers: u32,
    pub head_count_kv: u32,
    pub key_length: u32,
    pub value_length: u32,
}

impl KvGeometry {
    /// Bytes of KV cache per token, assuming F16 cache elements —
    /// llama.cpp's default cache type when no `--cache-type-k/v`
    /// flag overrides it. Saturating: the geometry fields come
    /// from the file header, and a header claiming u32-maximum
    /// geometry must saturate to "enormous", never wrap small.
    pub fn bytes_per_token(&self) -> u64 {
        (self.kv_layers as u64)
            .saturating_mul(self.head_count_kv as u64)
            .saturating_mul((self.key_length as u64).saturating_add(self.value_length as u64))
            .saturating_mul(2)
    }
}

/// Recurrent-state geometry derived from GGUF metadata, for the
/// hybrid SSM / gated-delta-net architectures whose layers carry
/// a recurrent state instead of (or alongside) a KV cache —
/// llama.cpp allocates that state on the GPU as its "rs cache"
/// (llama-memory-recurrent), sized per sequence and independent
/// of context length. The state has two parts per recurrent
/// layer, mirroring llama.cpp's `n_embd_r()` / `n_embd_s()`:
/// the convolution state `(d_conv - 1) * (d_inner + 2 * n_group *
/// d_state)` elements and the SSM state `d_state * d_inner`
/// elements, both held in F32 whatever the weight type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RsGeometry {
    /// Layers carrying a recurrent state: the block count minus
    /// the KV layers, the complement of the parser's KV-layer
    /// derivation, so the two counts always sum to the block
    /// count within this parser's own arithmetic.
    pub recr_layers: u32,
    /// Convolution-state elements per layer (`n_embd_r()`).
    pub embd_r: u32,
    /// SSM-state elements per layer (`n_embd_s()`).
    pub embd_s: u32,
}

impl RsGeometry {
    /// Total recurrent-state bytes for ONE sequence across all
    /// recurrent layers. Saturating, like [`KvGeometry`]: the
    /// fields come from the file header.
    pub fn bytes_per_seq(&self) -> u64 {
        (self.embd_r as u64)
            .saturating_add(self.embd_s as u64)
            .saturating_mul(4)
            .saturating_mul(self.recr_layers as u64)
    }
}

/// The memory layout of one GGUF file, from its header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GgufLayout {
    /// Sum of all tensor data sizes.
    pub tensor_bytes: u64,
    /// Sum of the routed-expert tensors (the tensors expert offload
    /// moves to the CPU buffer). Zero for dense models.
    pub expert_bytes: u64,
    /// Routed-expert bytes per MoE block, in ascending block order
    /// — one entry per block that carries expert tensors (the same
    /// blocks [`GgufLayout::moe_block_count`] counts). Partial
    /// `--n-cpu-moe N` offload moves the first N of these blocks;
    /// their true sum is what [`super::memory::memory_requirement`]
    /// prices from this, since per-block sizes are not uniform in
    /// dynamically-quantized files. Empty for dense models.
    pub expert_bytes_by_block: Vec<u64>,
    /// `<arch>.block_count` when present.
    pub block_count: Option<u32>,
    /// Number of blocks that carry routed-expert tensors, when any
    /// do. Partial CPU-MoE offload (`--n-cpu-moe N`) counts MoE
    /// layers, not total blocks — hybrid models (Nemotron: 24 MoE
    /// blocks of 53) need this count to price a partial offload.
    pub moe_block_count: Option<u32>,
    /// KV geometry when the metadata carries it completely.
    pub kv: Option<KvGeometry>,
    /// Recurrent-state geometry when the metadata carries the
    /// SSM dimensions (hybrid SSM / delta-net architectures).
    pub rs: Option<RsGeometry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GgufError {
    /// The byte slice ended before the header did; read a longer
    /// prefix and retry.
    Incomplete,
    /// The bytes are not a parseable GGUF header.
    Invalid(&'static str),
    /// A tensor uses a ggml element type this parser does not know.
    UnsupportedType(u32),
}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], GgufError> {
        let end = self.pos.checked_add(n).ok_or(GgufError::Incomplete)?;
        let slice = self.bytes.get(self.pos..end).ok_or(GgufError::Incomplete)?;
        self.pos = end;
        Ok(slice)
    }
    fn u8(&mut self) -> Result<u8, GgufError> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Result<u32, GgufError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, GgufError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn string(&mut self) -> Result<String, GgufError> {
        let n = self.u64()? as usize;
        let raw = self.take(n)?;
        Ok(String::from_utf8_lossy(raw).into_owned())
    }
    fn skip(&mut self, n: usize) -> Result<(), GgufError> {
        self.take(n)?;
        Ok(())
    }
}

/// One metadata value, reduced to what the layout needs: integers
/// keep their value, everything else is a marker (strings are kept
/// only for `general.architecture`, handled by the caller through
/// `want_string`).
enum MetaValue {
    Int(u64),
    Str(String),
    Other,
}

/// Deepest metadata-array nesting the parser will follow. Real
/// GGUF metadata nests at most a level or two (arrays of scalars);
/// each nesting level costs only ~12 bytes of input, so an
/// unbounded walk lets a crafted header recurse tens of thousands
/// of frames inside the prefix parse and abort the process
/// (a stack overflow is not a catchable panic).
const MAX_META_DEPTH: u32 = 32;

/// Read one metadata value of GGUF value type `ty`, consuming exactly
/// its encoded bytes (arrays are walked element by element — the
/// tokenizer arrays are large but length-prefixed, so skipping them
/// is linear in the header size, not the vocabulary's byte total
/// beyond the stored strings themselves).
fn read_meta_value(
    r: &mut Reader<'_>,
    ty: u32,
    keep_string: bool,
    depth: u32,
) -> Result<MetaValue, GgufError> {
    match ty {
        0 => Ok(MetaValue::Int(r.u8()? as u64)), // UINT8
        1 => Ok(MetaValue::Int(r.u8()? as u64)), // INT8 (sign irrelevant here)
        2 => {
            let lo = r.take(2)?;
            Ok(MetaValue::Int(
                u16::from_le_bytes(lo.try_into().unwrap()) as u64
            ))
        } // UINT16
        3 => {
            r.skip(2)?;
            Ok(MetaValue::Other)
        } // INT16
        4 => Ok(MetaValue::Int(r.u32()? as u64)), // UINT32
        5 => {
            r.skip(4)?;
            Ok(MetaValue::Other)
        } // INT32
        6 => {
            r.skip(4)?;
            Ok(MetaValue::Other)
        } // FLOAT32
        7 => {
            r.skip(1)?;
            Ok(MetaValue::Other)
        } // BOOL
        8 => {
            // STRING
            let s = r.string()?;
            Ok(if keep_string {
                MetaValue::Str(s)
            } else {
                MetaValue::Other
            })
        }
        9 => {
            // ARRAY
            if depth >= MAX_META_DEPTH {
                return Err(GgufError::Invalid("metadata array nested too deeply"));
            }
            let elem_ty = r.u32()?;
            let n = r.u64()?;
            for _ in 0..n {
                read_meta_value(r, elem_ty, false, depth + 1)?;
            }
            Ok(MetaValue::Other)
        }
        10 => Ok(MetaValue::Int(r.u64()?)), // UINT64
        11 => {
            r.skip(8)?;
            Ok(MetaValue::Other)
        } // INT64
        12 => {
            r.skip(8)?;
            Ok(MetaValue::Other)
        } // FLOAT64
        _ => Err(GgufError::Invalid("unknown metadata value type")),
    }
}

/// ggml element type geometry: (block size in elements, bytes per
/// block) by type id, per ggml.h's `ggml_type_size` table.
fn type_geometry(ty: u32) -> Option<(u64, u64)> {
    Some(match ty {
        0 => (1, 4),      // F32
        1 => (1, 2),      // F16
        2 => (32, 18),    // Q4_0
        3 => (32, 20),    // Q4_1
        6 => (32, 22),    // Q5_0
        7 => (32, 24),    // Q5_1
        8 => (32, 34),    // Q8_0
        9 => (32, 36),    // Q8_1
        10 => (256, 84),  // Q2_K
        11 => (256, 110), // Q3_K
        12 => (256, 144), // Q4_K
        13 => (256, 176), // Q5_K
        14 => (256, 210), // Q6_K
        15 => (256, 256), // Q8_K
        16 => (256, 66),  // IQ2_XXS
        17 => (256, 74),  // IQ2_XS
        18 => (256, 50),  // IQ3_XXS
        19 => (256, 52),  // IQ1_S
        20 => (32, 18),   // IQ4_NL
        21 => (256, 110), // IQ3_S
        22 => (256, 82),  // IQ2_S
        23 => (256, 136), // IQ4_XS
        24 => (1, 1),     // I8
        25 => (1, 2),     // I16
        26 => (1, 4),     // I32
        27 => (1, 8),     // I64
        28 => (1, 8),     // F64
        29 => (256, 56),  // IQ1_M
        30 => (1, 2),     // BF16
        39 => (32, 17),   // MXFP4
        _ => return None,
    })
}

/// Whether a tensor is a routed-expert weight — the tensors expert
/// offload (`-ot exps=CPU`, `--n-cpu-moe`) moves to system RAM.
/// Covers both GGUF naming styles: stacked expert tensors
/// (`blk.N.ffn_gate_exps.weight`) and per-expert tensors from older
/// conversions (`blk.N.ffn_gate.0.weight`). Shared experts
/// (`ffn_gate_shexp`) deliberately do not match: llama.cpp's `exps`
/// pattern does not move them either.
pub fn is_expert_tensor(name: &str) -> bool {
    if name.contains("exps") {
        return true;
    }
    for pat in ["ffn_gate.", "ffn_up.", "ffn_down."] {
        if let Some(idx) = name.find(pat) {
            let rest = &name[idx + pat.len()..];
            let head = rest.split('.').next().unwrap_or("");
            if !head.is_empty() && head.bytes().all(|b| b.is_ascii_digit()) {
                return true;
            }
        }
    }
    false
}

/// Parse a GGUF header prefix into its memory layout. `bytes` may be
/// exactly the header or a longer prefix of the file; tensor data is
/// never read.
pub fn parse_gguf_layout(bytes: &[u8]) -> Result<GgufLayout, GgufError> {
    let mut r = Reader { bytes, pos: 0 };
    if r.take(4)? != b"GGUF" {
        return Err(GgufError::Invalid("bad magic"));
    }
    let _version = r.u32()?;
    let tensor_count = r.u64()?;
    let kv_count = r.u64()?;

    let mut arch = String::new();
    let mut ints: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    for _ in 0..kv_count {
        let key = r.string()?;
        let ty = r.u32()?;
        let keep = key == "general.architecture";
        match read_meta_value(&mut r, ty, keep, 0)? {
            MetaValue::Int(v) => {
                ints.insert(key, v);
            }
            MetaValue::Str(s) => {
                if key == "general.architecture" {
                    arch = s;
                }
            }
            MetaValue::Other => {}
        }
    }
    let get = |suffix: &str| -> Option<u32> {
        if arch.is_empty() {
            return None;
        }
        // A UINT64 metadata value above u32::MAX is not a plausible
        // geometry field; truncating it (`as u32`) would silently
        // understate the KV cache by orders of magnitude. Treat it
        // as absent — the geometry is then unknown, which every
        // consumer degrades conservatively.
        ints.get(&format!("{arch}.{suffix}"))
            .and_then(|v| u32::try_from(*v).ok())
    };
    let block_count = get("block_count");
    let kv = match (
        block_count,
        get("attention.head_count_kv"),
        get("attention.key_length"),
        get("attention.value_length"),
    ) {
        (Some(blocks), Some(heads), Some(klen), Some(vlen)) => {
            let interval = get("full_attention_interval").unwrap_or(0);
            let kv_layers = if interval > 1 {
                blocks.div_ceil(interval)
            } else {
                blocks
            };
            Some(KvGeometry {
                kv_layers,
                head_count_kv: heads,
                key_length: klen,
                value_length: vlen,
            })
        }
        _ => None,
    };
    // Recurrent-state geometry: the SSM dimensions llama.cpp's
    // loader reads (`<arch>.ssm.*`). Present only on the hybrid
    // SSM / delta-net architectures; a dense model carries none
    // and gets no state term. The element widths follow
    // llama.cpp's hparams formulas (saturating, capped at u32 —
    // a header claiming more is not a plausible geometry and
    // saturates to "enormous" downstream, never wraps small).
    let rs = match (block_count, get("ssm.state_size"), get("ssm.inner_size")) {
        (Some(blocks), Some(d_state), Some(d_inner)) if d_state > 0 && d_inner > 0 => {
            let kv_layers = kv.map(|k| k.kv_layers).unwrap_or(0);
            let recr_layers = blocks.saturating_sub(kv_layers);
            if recr_layers == 0 {
                None
            } else {
                let d_conv = get("ssm.conv_kernel").unwrap_or(0);
                let n_group = get("ssm.group_count").unwrap_or(0);
                let cap = |v: u64| u32::try_from(v).unwrap_or(u32::MAX);
                let embd_s = cap((d_state as u64).saturating_mul(d_inner as u64));
                let embd_r = cap(
                    (if d_conv > 0 { d_conv - 1 } else { 0 } as u64).saturating_mul(
                        (d_inner as u64)
                            .saturating_add(2u64.saturating_mul(n_group as u64).saturating_mul(d_state as u64)),
                    ),
                );
                Some(RsGeometry {
                    recr_layers,
                    embd_r,
                    embd_s,
                })
            }
        }
        _ => None,
    };

    let mut tensor_bytes: u64 = 0;
    let mut expert_bytes: u64 = 0;
    let mut expert_by_block: std::collections::BTreeMap<u32, u64> =
        std::collections::BTreeMap::new();
    for _ in 0..tensor_count {
        let name = r.string()?;
        let n_dims = r.u32()?;
        // Checked: a header claiming dimensions whose product
        // overflows u64 (or whose byte size does) is not a real
        // file — release builds have no overflow checks, so
        // wrapping arithmetic would report a near-zero size and an
        // "exact" requirement that fits anywhere.
        let mut elements: u64 = 1;
        for _ in 0..n_dims {
            elements = elements
                .checked_mul(r.u64()?)
                .ok_or(GgufError::Invalid("tensor element count overflow"))?;
        }
        let ty = r.u32()?;
        let _offset = r.u64()?;
        let (block, block_bytes) = type_geometry(ty).ok_or(GgufError::UnsupportedType(ty))?;
        if !elements.is_multiple_of(block) {
            return Err(GgufError::Invalid("tensor element count not block-aligned"));
        }
        let size = (elements / block)
            .checked_mul(block_bytes)
            .ok_or(GgufError::Invalid("tensor size overflow"))?;
        tensor_bytes = tensor_bytes.saturating_add(size);
        if is_expert_tensor(&name) {
            expert_bytes = expert_bytes.saturating_add(size);
            if let Some(idx) = name
                .strip_prefix("blk.")
                .and_then(|rest| rest.split('.').next())
                .and_then(|s| s.parse::<u32>().ok())
            {
                let entry = expert_by_block.entry(idx).or_insert(0);
                *entry = entry.saturating_add(size);
            }
        }
    }
    let moe_block_count = if expert_by_block.is_empty() {
        None
    } else {
        Some(expert_by_block.len() as u32)
    };
    let expert_bytes_by_block: Vec<u64> = expert_by_block.into_values().collect();

    Ok(GgufLayout {
        tensor_bytes,
        expert_bytes,
        expert_bytes_by_block,
        block_count,
        moe_block_count,
        kv,
        rs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w_str(out: &mut Vec<u8>, s: &str) {
        out.extend_from_slice(&(s.len() as u64).to_le_bytes());
        out.extend_from_slice(s.as_bytes());
    }
    fn w_kv_u32(out: &mut Vec<u8>, key: &str, v: u32) {
        w_str(out, key);
        out.extend_from_slice(&4u32.to_le_bytes());
        out.extend_from_slice(&v.to_le_bytes());
    }
    fn w_kv_str(out: &mut Vec<u8>, key: &str, v: &str) {
        w_str(out, key);
        out.extend_from_slice(&8u32.to_le_bytes());
        w_str(out, v);
    }
    fn w_tensor(out: &mut Vec<u8>, name: &str, dims: &[u64], ty: u32) {
        w_str(out, name);
        out.extend_from_slice(&(dims.len() as u32).to_le_bytes());
        for d in dims {
            out.extend_from_slice(&d.to_le_bytes());
        }
        out.extend_from_slice(&ty.to_le_bytes());
        out.extend_from_slice(&0u64.to_le_bytes());
    }

    fn sample_header() -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(b"GGUF");
        b.extend_from_slice(&3u32.to_le_bytes());
        b.extend_from_slice(&4u64.to_le_bytes()); // tensors
        b.extend_from_slice(&5u64.to_le_bytes()); // metadata kvs
        w_kv_str(&mut b, "general.architecture", "testarch");
        w_kv_u32(&mut b, "testarch.block_count", 4);
        w_kv_u32(&mut b, "testarch.attention.head_count_kv", 2);
        w_kv_u32(&mut b, "testarch.attention.key_length", 64);
        w_kv_u32(&mut b, "testarch.attention.value_length", 64);
        w_tensor(&mut b, "token_embd.weight", &[10, 10], 0); // F32: 400 B
        w_tensor(&mut b, "blk.0.ffn_gate_exps.weight", &[256], 12); // Q4_K: 144 B
        w_tensor(&mut b, "blk.0.ffn_down.3.weight", &[32], 8); // Q8_0 per-expert: 34 B
        w_tensor(&mut b, "blk.0.ffn_gate_shexp.weight", &[16], 1); // F16 shared: 32 B, NOT expert
        b
    }

    #[test]
    fn parses_layout_and_expert_split() {
        let layout = parse_gguf_layout(&sample_header()).unwrap();
        assert_eq!(layout.tensor_bytes, 400 + 144 + 34 + 32);
        assert_eq!(layout.expert_bytes, 144 + 34);
        // Both expert tensors are in blk.0.
        assert_eq!(layout.expert_bytes_by_block, vec![144 + 34]);
        assert_eq!(layout.block_count, Some(4));
        // Both expert tensors live in blk.0 — one MoE block.
        assert_eq!(layout.moe_block_count, Some(1));
        let kv = layout.kv.unwrap();
        assert_eq!(kv.kv_layers, 4);
        assert_eq!(kv.bytes_per_token(), 4 * 2 * (64 + 64) * 2);
    }

    #[test]
    fn full_attention_interval_reduces_kv_layers() {
        let mut b = Vec::new();
        b.extend_from_slice(b"GGUF");
        b.extend_from_slice(&3u32.to_le_bytes());
        b.extend_from_slice(&0u64.to_le_bytes());
        b.extend_from_slice(&6u64.to_le_bytes());
        w_kv_str(&mut b, "general.architecture", "testarch");
        w_kv_u32(&mut b, "testarch.block_count", 48);
        w_kv_u32(&mut b, "testarch.attention.head_count_kv", 2);
        w_kv_u32(&mut b, "testarch.attention.key_length", 256);
        w_kv_u32(&mut b, "testarch.attention.value_length", 256);
        w_kv_u32(&mut b, "testarch.full_attention_interval", 4);
        let layout = parse_gguf_layout(&b).unwrap();
        assert_eq!(layout.kv.unwrap().kv_layers, 12);
        assert_eq!(layout.kv.unwrap().bytes_per_token(), 24_576);
    }

    #[test]
    fn ssm_metadata_yields_recurrent_state_geometry() {
        // A hybrid header in the Qwen3-Next family shape: 64
        // blocks, full attention every 4th layer (16 KV layers,
        // 48 recurrent), plus the SSM dimensions llama.cpp's
        // loader reads. The widths are the Whittle-MoE incident
        // model's family geometry: the formula's output for it
        // (598.5 MiB at 4 sequences) is the allocation llama.cpp
        // measured on Pete's machine for planner-moe.
        let mut b = Vec::new();
        b.extend_from_slice(b"GGUF");
        b.extend_from_slice(&3u32.to_le_bytes());
        b.extend_from_slice(&0u64.to_le_bytes());
        b.extend_from_slice(&10u64.to_le_bytes());
        w_kv_str(&mut b, "general.architecture", "testarch");
        w_kv_u32(&mut b, "testarch.block_count", 64);
        w_kv_u32(&mut b, "testarch.attention.head_count_kv", 2);
        w_kv_u32(&mut b, "testarch.attention.key_length", 256);
        w_kv_u32(&mut b, "testarch.attention.value_length", 256);
        w_kv_u32(&mut b, "testarch.full_attention_interval", 4);
        w_kv_u32(&mut b, "testarch.ssm.conv_kernel", 4);
        w_kv_u32(&mut b, "testarch.ssm.inner_size", 6144);
        w_kv_u32(&mut b, "testarch.ssm.state_size", 128);
        w_kv_u32(&mut b, "testarch.ssm.group_count", 16);
        let layout = parse_gguf_layout(&b).unwrap();
        assert_eq!(layout.kv.unwrap().kv_layers, 16);
        let rs = layout.rs.unwrap();
        assert_eq!(rs.recr_layers, 48);
        // n_embd_s = d_state * d_inner; n_embd_r = (d_conv - 1) *
        // (d_inner + 2 * n_group * d_state) — llama.cpp's formulas.
        assert_eq!(rs.embd_s, 128 * 6144);
        assert_eq!(rs.embd_r, 3 * (6144 + 2 * 16 * 128));
        // F32 elements: (30,720 + 786,432) * 4 B * 48 layers.
        assert_eq!(rs.bytes_per_seq(), 156_893_184);
    }

    #[test]
    fn no_ssm_metadata_no_recurrent_state() {
        // The dense sample header carries no ssm.* keys.
        let layout = parse_gguf_layout(&sample_header()).unwrap();
        assert_eq!(layout.rs, None);
        // SSM dimensions with every layer carrying KV (interval
        // 1) leave no recurrent layers: no state term either.
        let mut b = Vec::new();
        b.extend_from_slice(b"GGUF");
        b.extend_from_slice(&3u32.to_le_bytes());
        b.extend_from_slice(&0u64.to_le_bytes());
        b.extend_from_slice(&8u64.to_le_bytes());
        w_kv_str(&mut b, "general.architecture", "testarch");
        w_kv_u32(&mut b, "testarch.block_count", 4);
        w_kv_u32(&mut b, "testarch.attention.head_count_kv", 2);
        w_kv_u32(&mut b, "testarch.attention.key_length", 64);
        w_kv_u32(&mut b, "testarch.attention.value_length", 64);
        w_kv_u32(&mut b, "testarch.ssm.conv_kernel", 4);
        w_kv_u32(&mut b, "testarch.ssm.inner_size", 2048);
        w_kv_u32(&mut b, "testarch.ssm.state_size", 128);
        let layout = parse_gguf_layout(&b).unwrap();
        assert_eq!(layout.rs, None);
    }

    #[test]
    fn truncated_prefix_reports_incomplete() {
        let full = sample_header();
        assert_eq!(
            parse_gguf_layout(&full[..full.len() - 5]),
            Err(GgufError::Incomplete)
        );
        assert_eq!(parse_gguf_layout(&full[..2]), Err(GgufError::Incomplete));
    }

    #[test]
    fn bad_magic_is_invalid_not_incomplete() {
        assert_eq!(
            parse_gguf_layout(b"NOPE................"),
            Err(GgufError::Invalid("bad magic"))
        );
    }

    #[test]
    fn deeply_nested_metadata_array_is_invalid_not_a_stack_overflow() {
        // ARRAY-of-ARRAY nesting, one element per level: ~12 bytes
        // of input per recursion level. Past the depth cap the
        // parse must fail cleanly instead of recursing to abort.
        let mut b = Vec::new();
        b.extend_from_slice(b"GGUF");
        b.extend_from_slice(&3u32.to_le_bytes());
        b.extend_from_slice(&0u64.to_le_bytes()); // tensors
        b.extend_from_slice(&1u64.to_le_bytes()); // metadata kvs
        w_str(&mut b, "tokenizer.ggml.tokens");
        b.extend_from_slice(&9u32.to_le_bytes()); // value type ARRAY
        for _ in 0..64 {
            b.extend_from_slice(&9u32.to_le_bytes()); // element type ARRAY
            b.extend_from_slice(&1u64.to_le_bytes()); // count 1
        }
        b.extend_from_slice(&0u32.to_le_bytes()); // innermost: UINT8
        b.extend_from_slice(&1u64.to_le_bytes());
        b.push(0u8);
        assert_eq!(
            parse_gguf_layout(&b),
            Err(GgufError::Invalid("metadata array nested too deeply"))
        );
    }

    #[test]
    fn absurd_tensor_dimensions_are_invalid_not_wrapped() {
        // One F64 tensor claiming dims [2^61]: byte size 2^64.
        // Wrapping arithmetic would report a 0-byte tensor; the
        // parse must reject the header instead.
        let mut b = Vec::new();
        b.extend_from_slice(b"GGUF");
        b.extend_from_slice(&3u32.to_le_bytes());
        b.extend_from_slice(&1u64.to_le_bytes()); // tensors
        b.extend_from_slice(&0u64.to_le_bytes()); // metadata kvs
        w_tensor(&mut b, "huge.weight", &[1u64 << 61], 28); // F64
        assert_eq!(
            parse_gguf_layout(&b),
            Err(GgufError::Invalid("tensor size overflow"))
        );
        // The dimension product itself overflowing is rejected too.
        let mut b = Vec::new();
        b.extend_from_slice(b"GGUF");
        b.extend_from_slice(&3u32.to_le_bytes());
        b.extend_from_slice(&1u64.to_le_bytes());
        b.extend_from_slice(&0u64.to_le_bytes());
        w_tensor(&mut b, "huge.weight", &[1u64 << 40, 1u64 << 40], 0); // F32
        assert_eq!(
            parse_gguf_layout(&b),
            Err(GgufError::Invalid("tensor element count overflow"))
        );
    }

    #[test]
    fn oversized_geometry_metadata_is_absent_not_truncated() {
        // key_length as a UINT64 of 2^32 + 256: truncating to u32
        // would claim 256 and understate the KV cache ~16x. The
        // geometry must come back unknown instead.
        let mut b = Vec::new();
        b.extend_from_slice(b"GGUF");
        b.extend_from_slice(&3u32.to_le_bytes());
        b.extend_from_slice(&0u64.to_le_bytes());
        b.extend_from_slice(&5u64.to_le_bytes());
        w_kv_str(&mut b, "general.architecture", "testarch");
        w_kv_u32(&mut b, "testarch.block_count", 4);
        w_kv_u32(&mut b, "testarch.attention.head_count_kv", 2);
        w_str(&mut b, "testarch.attention.key_length");
        b.extend_from_slice(&10u32.to_le_bytes()); // UINT64
        b.extend_from_slice(&(2u64.pow(32) + 256).to_le_bytes());
        w_kv_u32(&mut b, "testarch.attention.value_length", 64);
        let layout = parse_gguf_layout(&b).unwrap();
        assert_eq!(layout.kv, None);
        // And KvGeometry's own math saturates rather than wraps.
        let geo = KvGeometry {
            kv_layers: u32::MAX,
            head_count_kv: u32::MAX,
            key_length: u32::MAX,
            value_length: u32::MAX,
        };
        assert_eq!(geo.bytes_per_token(), u64::MAX);
    }

    #[test]
    fn expert_name_rules() {
        assert!(is_expert_tensor("blk.0.ffn_gate_exps.weight"));
        assert!(is_expert_tensor("blk.0.ffn_up_exps.weight"));
        assert!(is_expert_tensor("blk.12.ffn_down.7.weight"));
        assert!(!is_expert_tensor("blk.0.ffn_gate_shexp.weight"));
        assert!(!is_expert_tensor("blk.0.ffn_gate_inp.weight"));
        assert!(!is_expert_tensor("blk.0.attn_q.weight"));
    }
}

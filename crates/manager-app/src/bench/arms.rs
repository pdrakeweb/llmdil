//! Sweep planning: GGUF facts, arm specs, placement, the lifecycle
//! interlock, temp-backend cleanup, and response scoring helpers.

use super::*;

// ---------------------------------------------------------------------------
// GGUF header facts for validation pruning (§3.2): the model's
// trained context length and the tokenizer's vocabulary size,
// read from the file's metadata. The app's layout parser does
// not carry these two keys, so this is a minimal metadata walk
// (integers and the tokens array's element count only).
// ---------------------------------------------------------------------------

pub(super) fn gguf_header_facts(path: &PathBuf) -> (Option<u32>, Option<u32>) {
    use std::io::Read;
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return (None, None),
    };
    // Headers with a full token array run to a few MB; cap the
    // walk so a hostile file cannot make it read unboundedly.
    let mut buf = Vec::new();
    let _ = file.by_ref().take(64 * 1024 * 1024).read_to_end(&mut buf);
    parse_gguf_facts(&buf)
}

pub(super) fn parse_gguf_facts(buf: &[u8]) -> (Option<u32>, Option<u32>) {
    walk_gguf_facts(buf).unwrap_or((None, None))
}

pub(super) fn walk_gguf_facts(buf: &[u8]) -> Option<(Option<u32>, Option<u32>)> {
    struct R<'a> {
        b: &'a [u8],
        p: usize,
    }
    impl<'a> R<'a> {
        fn take(&mut self, n: usize) -> Option<&'a [u8]> {
            let end = self.p.checked_add(n)?;
            let s = self.b.get(self.p..end)?;
            self.p = end;
            Some(s)
        }
        fn u32(&mut self) -> Option<u32> {
            Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
        }
        fn u64(&mut self) -> Option<u64> {
            Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
        }
        fn string(&mut self) -> Option<String> {
            let n = self.u64()? as usize;
            Some(String::from_utf8_lossy(self.take(n)?).into_owned())
        }
        /// Skip one metadata value of the given GGUF type,
        /// reporting the tokens-array element count when the
        /// value is an array (the caller wants exactly one).
        fn skip_value(&mut self, ty: u32) -> Option<Option<u64>> {
            match ty {
                0 | 1 | 7 => {
                    self.take(1)?;
                    Some(None)
                }
                2 | 3 => {
                    self.take(2)?;
                    Some(None)
                }
                4..=6 => {
                    self.take(4)?;
                    Some(None)
                }
                8 => {
                    let n = self.u64()? as usize;
                    self.take(n)?;
                    Some(None)
                }
                9 => {
                    let elem_ty = self.u32()?;
                    let count = self.u64()?;
                    for _ in 0..count {
                        self.skip_value(elem_ty)?;
                    }
                    Some(Some(count))
                }
                10..=12 => {
                    self.take(8)?;
                    Some(None)
                }
                _ => None,
            }
        }
        fn int_value(&mut self, ty: u32) -> Option<u64> {
            match ty {
                0 => Some(self.take(1)?[0] as u64),
                2 => Some(u16::from_le_bytes(self.take(2)?.try_into().ok()?) as u64),
                4 => Some(self.u32()? as u64),
                5 => Some(
                    self.take(4)
                        .map(|b| i32::from_le_bytes(b.try_into().unwrap()) as u64)?,
                ),
                10 => self.u64(),
                11 => Some(self.u64()?),
                _ => {
                    self.skip_value(ty)?;
                    None
                }
            }
        }
    }
    let mut r = R { b: buf, p: 0 };
    if r.take(4)? != b"GGUF" {
        return None;
    }
    let _version = r.u32();
    let _tensor_count = r.u64()?;
    let kv_count = r.u64()?;
    let mut arch: Option<String> = None;
    let mut ctx_len: Option<u32> = None;
    let mut vocab: Option<u32> = None;
    for _ in 0..kv_count {
        let key = match r.string() {
            Some(k) => k,
            None => break,
        };
        let ty = match r.u32() {
            Some(t) => t,
            None => break,
        };
        if key == "general.architecture" && ty == 8 {
            arch = r.string();
            let _ = &arch;
            continue;
        }
        if key == "tokenizer.ggml.tokens" && ty == 9 {
            // Read the array header, record the count, then
            // skip the elements.
            let elem_ty = match r.u32() {
                Some(t) => t,
                None => break,
            };
            let count = match r.u64() {
                Some(c) => c,
                None => break,
            };
            vocab = Some(count as u32);
            let mut ok = true;
            for _ in 0..count {
                if r.skip_value(elem_ty).is_none() {
                    ok = false;
                    break;
                }
            }
            if !ok {
                break;
            }
            continue;
        }
        if ty != 9 && key.ends_with(".context_length") {
            if let Some(v) = r.int_value(ty) {
                ctx_len = Some(v as u32);
            }
            continue;
        }
        if r.skip_value(ty).is_none() {
            break;
        }
    }
    let _ = arch;
    Some((ctx_len, vocab))
}

// ---------------------------------------------------------------------------
// Sweep request, interlock, preflight (design §3, §6.1–§6.3)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub(super) struct SweepRequest {
    pub(super) backend_id: String,
    pub(super) prompt_id: String,
    #[serde(default)]
    pub(super) temperature: f32,
    #[serde(default)]
    pub(super) confirm_at_06: bool,
    #[serde(default)]
    pub(super) correctness_control: bool,
    #[serde(default)]
    pub(super) stop_backend_ids: Vec<String>,
    #[serde(default)]
    pub(super) preset: Option<String>,
    #[serde(default)]
    pub(super) dimensions: Option<Vec<Vec<mb::ArmVariant>>>,
    #[serde(default)]
    pub(super) draft_model_id: Option<String>,
    #[serde(default)]
    pub(super) baseline_arm_index: usize,
}

/// The target backend's current axes, read from its saved
/// configuration: the base every sweep variant merges over.
pub(super) fn base_arm_spec(target: &BackendConfig) -> mb::ArmSpec {
    let raw = target.raw_flags.as_deref().unwrap_or("");
    let tokens: Vec<&str> = raw.split_whitespace().collect();
    let mut cache_k = mb::CacheType::F16;
    let mut cache_v = mb::CacheType::F16;
    let mut flash = mb::FlashAttn::Auto;
    let mut i = 0;
    while i < tokens.len() {
        let t = tokens[i];
        let (flag, inline_val) = match t.split_once('=') {
            Some((f, v)) => (f, Some(v.to_string())),
            None => (t, None),
        };
        let val = |i: &mut usize| -> Option<String> {
            if let Some(v) = &inline_val {
                return Some(v.clone());
            }
            *i += 1;
            tokens.get(*i).map(|s| s.to_string())
        };
        match flag {
            "--cache-type-k" | "-ctk" => {
                if let Some(v) = val(&mut i) {
                    cache_k = parse_cache_type(&v).unwrap_or(cache_k);
                }
            }
            "--cache-type-v" | "-ctv" => {
                if let Some(v) = val(&mut i) {
                    cache_v = parse_cache_type(&v).unwrap_or(cache_v);
                }
            }
            "--flash-attn" => {
                if let Some(v) = val(&mut i) {
                    flash = match v.as_str() {
                        "on" => mb::FlashAttn::On,
                        "off" => mb::FlashAttn::Off,
                        _ => mb::FlashAttn::Auto,
                    };
                }
            }
            _ => {}
        }
        i += 1;
    }
    // The target's effective expert offload, decoded from its
    // raw flags and re-spelled in the canonical form the app
    // itself writes: an MoE backend's arms record the position
    // they run at (the arm label names it), and the offload
    // dimension sweeps positions from this base. The launch
    // rewrite strips the base's expert tokens and appends this
    // expression, so the launched position is the same one —
    // only its spelling is canonical. `None` when the backend
    // offloads nothing: its arms carry no offload expression
    // and the base tokens pass through untouched.
    let offload_expr = manager_models::offload_flag_string(manager_models::expert_offload(
        target.raw_flags.as_deref(),
    ));
    mb::ArmSpec {
        cache_k,
        cache_v,
        flash_attn: flash,
        depth: 0,
        n_ctx: target.flags.n_ctx,
        spec: mb::SpecKind::None,
        ngram_n_min: None,
        draft_model_id: None,
        draft_n_max: None,
        draft_device: mb::DraftDevice::TargetGpu,
        draft_cache_k: mb::CacheType::Q8_0,
        draft_cache_v: mb::CacheType::Q8_0,
        offload_expr,
    }
}

pub(super) fn parse_cache_type(s: &str) -> Option<mb::CacheType> {
    match s {
        "f16" => Some(mb::CacheType::F16),
        "q8_0" => Some(mb::CacheType::Q8_0),
        "q4_0" => Some(mb::CacheType::Q4_0),
        _ => None,
    }
}

/// Does this backend run layers on the CPU (experts or all
/// layers)? The CPU-offload contention class of §6.2.
pub(super) fn cpu_offload(b: &BackendConfig) -> bool {
    !matches!(
        manager_models::expert_offload(b.raw_flags.as_deref()),
        manager_models::ExpertOffload::None
    ) || matches!(
        crate::backend_gpu_visibility(b),
        crate::GpuVisibility::CpuOnly
    )
}

/// A backend's GPU placement as the interlock judges it (§6.1).
///
/// This is deliberately NOT `crate::backend_gpu_visibility`:
/// that function reports `Undetermined` for any backend whose
/// device selection lives in raw flags and is not exactly
/// {GPU 0}, because the LAUNCH path will not remap a raw
/// selection it does not model (code-review 4.4) and launches it
/// unrestricted instead. For conflict purposes a parsed raw
/// selection is determinate evidence of where the backend runs:
/// a backend raw-pinned to GPU 1 (`--tensor-split 0,1`,
/// `--device CUDA1`, `--main-gpu 1`) shares no GPU with a GPU-0
/// target, and judging it Undetermined made it conflict with
/// every sweep on the machine. Only a selection that does not
/// parse stays Undetermined here, and an Undetermined placement
/// still conflicts with everything (conservative: a sweep that
/// cannot prove isolation does not run).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum BenchPlacement {
    CpuOnly,
    Gpus(std::collections::BTreeSet<u32>),
    Undetermined,
}

pub(crate) fn bench_placement(b: &BackendConfig) -> BenchPlacement {
    if crate::raw_flags_pin_cpu_only(b.raw_flags.as_deref()) {
        return BenchPlacement::CpuOnly;
    }
    if !b.flags.tensor_split.is_empty() {
        // A structured split remains the source of truth and a
        // raw selection yields to it, as in the launch path.
        let placed: std::collections::BTreeSet<u32> = b
            .flags
            .tensor_split
            .iter()
            .enumerate()
            .filter(|(_, f)| **f > 0.0)
            .map(|(i, _)| i as u32)
            .collect();
        return if placed.is_empty() {
            BenchPlacement::Undetermined
        } else {
            BenchPlacement::Gpus(placed)
        };
    }
    match crate::raw_device_selection(b.raw_flags.as_deref()) {
        crate::RawDeviceSelection::Set(set) => BenchPlacement::Gpus(set),
        // Unsplit with no raw selection: llama.cpp's main GPU.
        crate::RawDeviceSelection::None => {
            BenchPlacement::Gpus(std::collections::BTreeSet::from([0]))
        }
        crate::RawDeviceSelection::Unparseable => BenchPlacement::Undetermined,
    }
}

/// The running backends that conflict with a sweep against
/// `target` (§6.1/§6.2): any backend sharing a GPU with the
/// target's placement, the target itself, and — when the TARGET
/// runs layers on the CPU — any other CPU-offload backend (the
/// CPU is shared by every MoE backend's experts, §3.4/§6.2; the
/// rule binds on the target's offload, not on any running
/// backend's). An undetermined placement conflicts with
/// everything (conservative: a sweep that cannot prove isolation
/// does not run).
pub(super) fn interlock_conflicts(
    cfg: &AppConfig,
    running: &HashSet<String>,
    target: &BackendConfig,
) -> Vec<String> {
    let target_place = bench_placement(target);
    let target_cpu = cpu_offload(target);
    let mut out: Vec<String> = Vec::new();
    for b in &cfg.backends {
        if !running.contains(&b.id) {
            continue;
        }
        if b.id == target.id {
            out.push(b.id.clone());
            continue;
        }
        let place = bench_placement(b);
        let gpu_conflict = match (&target_place, &place) {
            (BenchPlacement::Gpus(a), BenchPlacement::Gpus(bset)) => {
                a.iter().any(|g| bset.contains(g))
            }
            (BenchPlacement::CpuOnly, _) | (_, BenchPlacement::CpuOnly) => false,
            _ => true, // Undetermined on either side
        };
        let cpu_conflict = target_cpu && cpu_offload(b);
        if gpu_conflict || cpu_conflict {
            out.push(b.id.clone());
        }
    }
    out
}

/// Guaranteed cleanup for one bench temp backend, on the
/// experiment pattern: stop it when live, unregister its spec,
/// remove its journal record. Runs outside the arm future, so
/// neither cancellation nor failure can skip it.
pub(super) async fn cleanup_bench_temp(state: &AppState, temp_id: &str) {
    {
        let mut sup = state.supervisor().await.lock().await;
        sup.reap_exited();
        let live = sup
            .status()
            .iter()
            .any(|s| s.id == temp_id && s.pid.is_some());
        if live {
            let _ = sup.stop(temp_id, false).await;
        }
        sup.unregister_spec(temp_id);
    }
    let mut journal = load_journal(state);
    if journal.remove_active(temp_id) {
        save_journal(state, &journal);
    }
}

/// POST /tokenize against a bench backend: the measured token
/// count of a text. None when the endpoint does not answer
/// (older builds) — callers fall back to a character estimate.
pub(super) async fn tokenize_count(base_url: &str, text: &str) -> Option<u32> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .ok()?;
    let resp = client
        .post(format!("{base_url}/tokenize"))
        .header("content-type", "application/json")
        .body(serde_json::to_string(&json!({ "content": text })).ok()?)
        .send()
        .await
        .ok()?;
    let text_body = resp.text().await.ok()?;
    let body: Value = serde_json::from_str(&text_body).ok()?;
    body.get("tokens")
        .and_then(|t| t.as_array())
        .map(|a| a.len() as u32)
}

/// Word-set overlap of a candidate output against the baseline
/// output (the correctness control's divergence measure, §2.1):
/// the share of the baseline's distinct words that also appear
/// in the candidate. Identical greedy outputs score 1.0.
pub(super) fn word_overlap(baseline: &str, candidate: &str) -> f64 {
    let base: HashSet<&str> = baseline.split_whitespace().collect();
    if base.is_empty() {
        return 1.0;
    }
    let cand: HashSet<&str> = candidate.split_whitespace().collect();
    let shared = base.intersection(&cand).count();
    shared as f64 / base.len() as f64
}

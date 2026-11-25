use super::*;
use crate::test_support::*;

/// P-4 (docs/code-review-v0.1.60.md): a header still `Incomplete`
/// at the largest prefix rung is cached as a failure (keyed by
/// length + mtime, so a changed file is parsed afresh).
#[test]
fn gguf_layout_incomplete_at_last_rung_is_cached() {
    let dir = unique_test_dir("llm-mgr-p4-gguf");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("crafted.gguf");
    // GGUF v3, 0 tensors, 1 metadata pair whose key claims 1000
    // bytes: incomplete at every prefix of this 64-byte file.
    let mut bytes = b"GGUF".to_vec();
    bytes.extend_from_slice(&3u32.to_le_bytes());
    bytes.extend_from_slice(&0u64.to_le_bytes());
    bytes.extend_from_slice(&1u64.to_le_bytes());
    bytes.extend_from_slice(&1000u64.to_le_bytes());
    bytes.resize(64, b'k');
    std::fs::write(&path, &bytes).unwrap();
    let entries_for = |p: &PathBuf| -> Vec<u64> {
        GGUF_LAYOUT_CACHE
            .get()
            .map(|c| {
                c.lock()
                    .unwrap()
                    .iter()
                    .filter(|((kp, _, _), v)| kp == p && v.is_none())
                    .map(|((_, len, _), _)| *len)
                    .collect()
            })
            .unwrap_or_default()
    };
    assert!(gguf_layout_cached_with(&path, &[16, 32]).is_none());
    assert_eq!(entries_for(&path), vec![64]);
    // A changed file (new length) is a new key, parsed afresh.
    bytes.resize(80, b'k');
    std::fs::write(&path, &bytes).unwrap();
    assert!(gguf_layout_cached_with(&path, &[16, 32]).is_none());
    let mut lens = entries_for(&path);
    lens.sort_unstable();
    assert_eq!(lens, vec![64, 80]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn backend_parallel_info_decodes_and_marks_custom_spellings() {
    let info = |raw: Option<&str>| {
        backend_parallel_info(&uncreated_backend(
            "b",
            "models/m.gguf",
            raw.map(str::to_string),
        ))
    };
    // Unset: no explicit count, and nothing to mark — the
    // field presents llama.cpp's default.
    let i = info(None);
    assert_eq!(i.slots, None);
    assert!(!i.custom);
    let i = info(Some("-ot exps=CPU"));
    assert_eq!(i.slots, None);
    assert!(!i.custom);
    // The canonical spelling the control itself writes.
    let i = info(Some("--parallel 2"));
    assert_eq!(i.slots, Some(2));
    assert!(!i.custom);
    let i = info(Some("--spec-type draft-mtp --parallel 1"));
    assert_eq!(i.slots, Some(1));
    assert!(!i.custom);
    // Non-canonical spellings of the same value: the -np and
    // = forms, and a canonical token in a non-canonical
    // position (the shared custom definition: the canonical
    // rewrite must reproduce the saved tokens exactly).
    let i = info(Some("-np 2"));
    assert_eq!(i.slots, Some(2));
    assert!(i.custom);
    let i = info(Some("--parallel=2"));
    assert_eq!(i.slots, Some(2));
    assert!(i.custom);
    let i = info(Some("--parallel 1 --spec-type draft-mtp"));
    assert_eq!(i.slots, Some(1));
    assert!(i.custom);
    // A malformed token decodes as unset but still marks: the
    // flags carry a parallel spelling the control does not
    // write.
    let i = info(Some("--parallel abc"));
    assert_eq!(i.slots, None);
    assert!(i.custom);
}

#[test]
fn offload_info_for_nemotron_backend_uses_catalog_pins() {
    let mut cfg = test_cfg();
    // No file on disk in the test config: the payload resolves
    // from the catalog pins (Catalog basis arithmetic).
    let b = serve_model(
        &mut cfg,
        "coder-fast",
        "nemotron-3.5-lightning-30b",
        "models/Nemotron.gguf",
        Some("--n-cpu-moe 16"),
    );
    let info = backend_offload_info(&cfg, &b).expect("nemotron is offload-capable");
    assert_eq!(info.moe_layers, 24);
    assert_eq!(info.layers, 16);
    assert_eq!(info.green_limit_layers, 16);
    assert_eq!(info.expert_bytes, 22_829_334_528);
    assert_eq!(info.total_bytes, 25_266_255_936);
    assert!((info.green_end - 16.0 / 24.0).abs() < 1e-9);
    assert!(info.yellow_end > info.green_end && info.yellow_end < 1.0);
    // The requirement under the same flags prices the partial
    // offload from the catalog pins: 16 of 24 MoE layers of
    // experts to system RAM (14,515 MiB), the rest of the
    // catalog's file size on the GPU (9,582 MiB).
    let req = backend_memory_requirement(&cfg, &b).unwrap();
    assert_eq!(req.sys_weights_mib, 14_515);
    assert_eq!(req.gpu_weights_mib, 9_582);
    assert_eq!(req.basis, manager_models::MemoryBasis::Catalog);
}

#[test]
fn catalog_arm_requirement_reports_kv_for_planner_mtp() {
    // The Oct 20, 2026 defect: planner-mtp's card showed no KV
    // figure. Its backend resolves through the catalog arm
    // whenever the GGUF header does not parse (here: no file
    // on disk at all, the pre-download shape) — the catalog
    // arm must still supply the KV figure from the catalog's
    // pinned geometry: 65,536 B/token, 2,048 MiB at the
    // backend's n_ctx 32768.
    let mut cfg = test_cfg();
    let mut b = serve_model(
        &mut cfg,
        "planner",
        "qwen3.8-27b-mtp",
        "models/Qwen3.8-27B-GSQ-RCO-IQ3_S-mtp.gguf",
        None,
    );
    b.flags.n_ctx = 32_768;
    let layout = resolve_weight_layout(&cfg, &b).unwrap();
    assert_eq!(layout.basis, manager_models::MemoryBasis::Catalog);
    assert_eq!(layout.kv_per_token, Some(65_536));
    let req = backend_memory_requirement(&cfg, &b).unwrap();
    assert_eq!(req.kv_mib, Some(2_048));
    assert_eq!(req.basis, manager_models::MemoryBasis::Catalog);
}

#[test]
fn catalog_arm_requirement_reports_kv_for_planner_and_coder() {
    // The same catalog-arm resolution for the base planner
    // (65,536 B/token — the figure its downloaded file's parse
    // reports, so the card's KV tile is identical before and
    // after download) and the coder (24,576 B/token, 768 MiB
    // at n_ctx 32768).
    let mut cfg = test_cfg();
    let mut b = serve_model(
        &mut cfg,
        "planner",
        "qwen3-27b-planner",
        "models/Qwen3.8-27B.gguf",
        None,
    );
    b.flags.n_ctx = 32_768;
    let req = backend_memory_requirement(&cfg, &b).unwrap();
    assert_eq!(req.kv_mib, Some(2_048));
    let mut cfg = test_cfg();
    let mut b = serve_model(
        &mut cfg,
        "coder",
        "qwen3-coder-30b",
        "models/Qwen3-Coder-Next-UD-Q4_K_M.gguf",
        Some("-ot exps=CPU"),
    );
    b.flags.n_ctx = 32_768;
    let req = backend_memory_requirement(&cfg, &b).unwrap();
    assert_eq!(req.kv_mib, Some(768));
}

/// A minimal GGUF header carrying complete KV geometry and
/// no tensors: `blocks` blocks, `heads_kv` KV heads, key/value
/// lengths `klen`/`vlen` under a test architecture.
fn gguf_kv_header_bytes(blocks: u32, heads_kv: u32, klen: u32, vlen: u32) -> Vec<u8> {
    fn w_str(out: &mut Vec<u8>, s: &str) {
        out.extend_from_slice(&(s.len() as u64).to_le_bytes());
        out.extend_from_slice(s.as_bytes());
    }
    fn w_kv_str(out: &mut Vec<u8>, key: &str, v: &str) {
        w_str(out, key);
        out.extend_from_slice(&8u32.to_le_bytes());
        w_str(out, v);
    }
    fn w_kv_u32(out: &mut Vec<u8>, key: &str, v: u32) {
        w_str(out, key);
        out.extend_from_slice(&4u32.to_le_bytes());
        out.extend_from_slice(&v.to_le_bytes());
    }
    let mut b = Vec::new();
    b.extend_from_slice(b"GGUF");
    b.extend_from_slice(&3u32.to_le_bytes());
    b.extend_from_slice(&0u64.to_le_bytes()); // tensor count
    b.extend_from_slice(&5u64.to_le_bytes()); // kv count
    w_kv_str(&mut b, "general.architecture", "testarch");
    w_kv_u32(&mut b, "testarch.block_count", blocks);
    w_kv_u32(&mut b, "testarch.attention.head_count_kv", heads_kv);
    w_kv_u32(&mut b, "testarch.attention.key_length", klen);
    w_kv_u32(&mut b, "testarch.attention.value_length", vlen);
    b
}

fn gguf_probe_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "llm-mgr-gguf-probe-{tag}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(dir.join("models")).unwrap();
    dir
}

#[test]
fn exact_arm_requirement_reports_the_gguf_kv_figure() {
    // A downloaded file that parses supplies the KV figure
    // from its own header (basis Exact) — for a model the
    // catalog does not carry at all, the header is the only
    // possible source: 4 blocks * 2 KV heads * (64 + 64) * 2 B
    // = 2,048 B/token, 64 MiB at n_ctx 32768.
    let dir = gguf_probe_dir("exact");
    std::fs::write(
        dir.join("models").join("user.gguf"),
        gguf_kv_header_bytes(4, 2, 64, 64),
    )
    .unwrap();
    let mut cfg = test_cfg();
    cfg.data_dir = dir.clone();
    register_test_model(&mut cfg, "user-kv-model", "models/user.gguf");
    let b = uncreated_backend("user-backend", "models/user.gguf", None);
    let layout = resolve_weight_layout(&cfg, &b).unwrap();
    assert_eq!(layout.basis, manager_models::MemoryBasis::Exact);
    assert_eq!(layout.kv_per_token, Some(2_048));
    let req = backend_memory_requirement(&cfg, &b).unwrap();
    assert_eq!(req.kv_mib, Some(64));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn gguf_layout_recovers_when_a_missing_or_unparseable_file_is_replaced() {
    // Neither a missing file nor an unparseable one pins the
    // path: once the file is replaced by a parseable header,
    // the layout resolves. (The unparseable bytes are a
    // different file version — different length — so this
    // exercises the re-read path every poll takes when a
    // download lands or a file is rewritten.)
    let dir = gguf_probe_dir("recover");
    let path = dir.join("models").join("probe.gguf");
    assert!(gguf_layout_cached(&path).is_none());
    std::fs::write(&path, b"not a gguf file at all").unwrap();
    assert!(gguf_layout_cached(&path).is_none());
    std::fs::write(&path, gguf_kv_header_bytes(4, 2, 64, 64)).unwrap();
    let layout = gguf_layout_cached(&path).expect("replaced file parses");
    assert_eq!(layout.kv.unwrap().bytes_per_token(), 2_048);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn offload_info_for_coder_backend_is_all_layers() {
    let mut cfg = test_cfg();
    let b = serve_model(
        &mut cfg,
        "coder",
        "qwen3-coder-30b",
        "models/Qwen3-Coder-Next-UD-Q4_K_M.gguf",
        Some("-ot exps=CPU"),
    );
    let info = backend_offload_info(&cfg, &b).expect("coder is offload-capable");
    assert_eq!(info.moe_layers, 48);
    assert_eq!(info.layers, 48);
    assert_eq!(info.green_end, 1.0);
    assert_eq!(info.yellow_end, 1.0);
}

#[test]
fn offload_info_absent_for_dense_model() {
    let mut cfg = test_cfg();
    let b = serve_model(
        &mut cfg,
        "planner",
        "qwen3-27b-planner",
        "models/Qwen3.8-27B.gguf",
        None,
    );
    assert!(backend_offload_info(&cfg, &b).is_none());
}

/// A GGUF header whose first `dense_lead` blocks are dense and
/// whose next `moe_blocks` blocks each carry one 1 MiB routed
/// expert tensor (262,144 F32 elements), so offloaded bytes
/// read directly as whole MiB.
fn gguf_lead_dense_header(dense_lead: u32, moe_blocks: u32) -> Vec<u8> {
    fn w_str(out: &mut Vec<u8>, s: &str) {
        out.extend_from_slice(&(s.len() as u64).to_le_bytes());
        out.extend_from_slice(s.as_bytes());
    }
    fn w_tensor(out: &mut Vec<u8>, name: &str, elems: u64) {
        w_str(out, name);
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&elems.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&0u64.to_le_bytes());
    }
    let total = dense_lead + moe_blocks;
    let mut b = Vec::new();
    b.extend_from_slice(b"GGUF");
    b.extend_from_slice(&3u32.to_le_bytes());
    b.extend_from_slice(&(total as u64).to_le_bytes());
    b.extend_from_slice(&2u64.to_le_bytes());
    w_str(&mut b, "general.architecture");
    b.extend_from_slice(&8u32.to_le_bytes());
    w_str(&mut b, "testarch");
    w_str(&mut b, "testarch.block_count");
    b.extend_from_slice(&4u32.to_le_bytes());
    b.extend_from_slice(&total.to_le_bytes());
    for i in 0..dense_lead {
        w_tensor(&mut b, &format!("blk.{i}.ffn_gate.weight"), 262_144);
    }
    for i in dense_lead..total {
        w_tensor(&mut b, &format!("blk.{i}.ffn_gate_exps.weight"), 262_144);
    }
    b
}

#[test]
fn requirement_prices_ot_block_ranges_and_n_cpu_moe_block_indices() {
    // P-5 and the --n-cpu-moe check (review item 11), through
    // the app's own requirement and slider table: 3 leading
    // dense blocks then 37 MoE blocks, 1 MiB of experts each.
    let dir = gguf_probe_dir("p5");
    std::fs::write(
        dir.join("models").join("lead-dense.gguf"),
        gguf_lead_dense_header(3, 37),
    )
    .unwrap();
    let mut cfg = test_cfg();
    cfg.data_dir = dir.clone();
    register_test_model(&mut cfg, "lead-dense-moe", "models/lead-dense.gguf");
    cfg.models.last_mut().unwrap().moe_layers = Some(37);
    cfg.models.last_mut().unwrap().default_offload_layers = Some(10);
    let req = |raw: &str| {
        let b = uncreated_backend(
            "lead-dense",
            "models/lead-dense.gguf",
            Some(raw.to_string()),
        );
        backend_memory_requirement(&cfg, &b).unwrap()
    };
    // Block-range -ot recipe: blocks 0..=19 match, of which
    // 3..=19 carry experts — 17 MiB, not all 37.
    let r = req(r#"-ot "blk\.([0-9]|1[0-9])\.ffn_.*_exps\.=CPU""#);
    assert_eq!(r.sys_weights_mib, 17);
    assert_eq!(r.gpu_weights_mib, 3 + 20);
    assert_eq!(r.basis, manager_models::MemoryBasis::Exact);
    // An uncompilable pattern: all on the GPU, Estimate.
    let r = req(r"-ot blk\.(?!0).*_exps=CPU");
    assert_eq!(r.sys_weights_mib, 0);
    assert_eq!(r.basis, manager_models::MemoryBasis::Estimate);
    // --n-cpu-moe 10 is a block-index bound: blocks 3..=9.
    assert_eq!(req("--n-cpu-moe 10").sys_weights_mib, 7);
    // The slider counts MoE layers: the saved bound 10 reads
    // back as the 7 MoE layers it moves, and the table entry
    // at that position agrees with the requirement.
    let b = uncreated_backend(
        "lead-dense",
        "models/lead-dense.gguf",
        Some("--n-cpu-moe 10".to_string()),
    );
    let info = backend_offload_info(&cfg, &b).unwrap();
    assert_eq!(info.layers, 7);
    assert_eq!(info.sys_bytes_by_position[7], 7 * 1024 * 1024);
    assert_eq!(info.sys_bytes_by_position[10], 10 * 1024 * 1024);
    assert_eq!(info.sys_bytes_by_position[37], 37 * 1024 * 1024);
    // Position 10 (10 MoE layers) writes bound 13.
    assert_eq!(
        info.offload_for(10),
        manager_models::ExpertOffload::CpuMoeLayers(13)
    );
    assert_eq!(req("--n-cpu-moe 13").sys_weights_mib, 10);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn offload_slider_converts_moe_layers_to_block_bounds() {
    // Review item 11: 2 leading dense blocks + 4 MoE blocks.
    let dir = gguf_probe_dir("item11");
    std::fs::write(
        dir.join("models").join("lead2.gguf"),
        gguf_lead_dense_header(2, 4),
    )
    .unwrap();
    let mut cfg = test_cfg();
    cfg.data_dir = dir.clone();
    register_test_model(&mut cfg, "lead2-moe", "models/lead2.gguf");
    cfg.models.last_mut().unwrap().moe_layers = Some(4);
    cfg.models.last_mut().unwrap().default_offload_layers = Some(2);
    let info_for = |raw: Option<&str>| {
        let b = uncreated_backend("lead2", "models/lead2.gguf", raw.map(str::to_string));
        backend_offload_info(&cfg, &b).unwrap()
    };
    // Slider "2 MoE layers" writes --n-cpu-moe 4 ...
    let info = info_for(None);
    assert_eq!(info.moe_layers, 4);
    let target = info.offload_for(2);
    assert_eq!(
        manager_models::set_expert_offload(None, target).as_deref(),
        Some("--n-cpu-moe 4")
    );
    // ... and reads back as 2, canonical.
    let info = info_for(Some("--n-cpu-moe 4"));
    assert_eq!(info.layers, 2);
    assert!(!info.custom);
    // The row tells the UI which flag value each position writes.
    assert_eq!(
        offload_row_json(&info, None)["n_cpu_moe_by_position"],
        json!([0, 3, 4, 5, 6])
    );
    // The model's declared default (2 MoE layers) is written
    // at the same bound when the file is present ...
    let model = cfg.models.last().unwrap().clone();
    assert_eq!(
        model_default_raw_flags_at(&cfg.data_dir, &model).as_deref(),
        Some("--n-cpu-moe 4")
    );
    // ... and as-is without a parsed layout (no file).
    assert_eq!(
        model_default_raw_flags(&model).as_deref(),
        Some("--n-cpu-moe 2")
    );
    // An existing config written with the old semantics
    // (`--n-cpu-moe 2`, inside the dense prefix) displays what
    // llama.cpp really does — no MoE layer — and is marked
    // custom, since the slider would write it differently.
    let info = info_for(Some("--n-cpu-moe 2"));
    assert_eq!(info.layers, 0);
    assert!(info.custom);
    let info = info_for(Some("--n-cpu-moe 3"));
    assert_eq!(info.layers, 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn catalog_pin_wins_over_a_record_declaration() {
    let mut cfg = test_cfg();
    // A Nemotron record carrying a conflicting declaration
    // (full offload): the catalog's 16-of-24 pin still wins,
    // for the creation default and the slider's green limit.
    let b = serve_model(
        &mut cfg,
        "coder-fast",
        "nemotron-3.5-lightning-30b",
        "models/Nemotron.gguf",
        None,
    );
    let m = cfg
        .models
        .iter_mut()
        .find(|m| m.id == "nemotron-3.5-lightning-30b")
        .unwrap();
    m.moe_layers = Some(24);
    m.default_offload_layers = Some(24);
    let model = cfg
        .models
        .iter()
        .find(|m| m.id == "nemotron-3.5-lightning-30b")
        .unwrap()
        .clone();
    assert_eq!(
        model_default_raw_flags(&model).as_deref(),
        Some("--n-cpu-moe 16")
    );
    let info = backend_offload_info(&cfg, &b).expect("nemotron is offload-capable");
    assert_eq!(info.green_limit_layers, 16);
}

#[test]
fn moe_cache_info_and_requirement_pricing() {
    // The declared-MoE fixture (the offload slider's own): a
    // 48-block MoE GGUF on disk and a model record declaring
    // the count and a default offload.
    let dir = std::env::temp_dir().join(format!(
        "llm-mgr-moe-cache-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(dir.join("models")).unwrap();
    std::fs::write(
        dir.join("models").join("user-moe.gguf"),
        gguf_header_bytes(48, 0),
    )
    .unwrap();
    let mut cfg = test_cfg();
    cfg.data_dir = dir.clone();
    register_test_model(&mut cfg, "user-moe-27b", "models/user-moe.gguf");
    cfg.models.last_mut().unwrap().moe_layers = Some(48);
    cfg.models.last_mut().unwrap().default_offload_layers = Some(16);

    // Info: eligibility is the offload slider's — present
    // for the MoE backend, absent for a dense one.
    let b = uncreated_backend(
        "moe-backend",
        "models/user-moe.gguf",
        Some("--n-cpu-moe 16 --moe-cache-mib 4096".to_string()),
    );
    let info = backend_moe_cache_info(&cfg, &b).expect("MoE backend carries cache state");
    assert_eq!(info.mib, Some(4096));
    assert!(!info.custom);
    assert_eq!(info.offload_layers, 16);
    assert_eq!(info.moe_layers, 48);
    // An `=` spelling decodes to the same state, marked custom.
    let b_eq = uncreated_backend(
        "moe-backend",
        "models/user-moe.gguf",
        Some("--n-cpu-moe 16 --moe-cache-mib=4096".to_string()),
    );
    let info_eq = backend_moe_cache_info(&cfg, &b_eq).unwrap();
    assert_eq!(info_eq.mib, Some(4096));
    assert!(info_eq.custom);
    // No token: off, not custom.
    let b_off = uncreated_backend(
        "moe-backend",
        "models/user-moe.gguf",
        Some("--n-cpu-moe 16".to_string()),
    );
    let info_off = backend_moe_cache_info(&cfg, &b_off).unwrap();
    assert_eq!(info_off.mib, None);
    assert!(!info_off.custom);
    // A dense backend (the default config's planner) has none.
    let dense = cfg
        .backends
        .iter()
        .find(|x| x.id == "planner")
        .unwrap()
        .clone();
    assert!(backend_moe_cache_info(&cfg, &dense).is_none());

    // Pricing: the requirement carries the cache, residency
    // grows by exactly the cache on the single GPU, and the
    // start gate refuses the over-card cache with the numbers
    // while admitting the same requirement with room.
    let req_off = backend_memory_requirement(&cfg, &b_off).unwrap();
    assert_eq!(req_off.moe_cache_mib, 0);
    let req_on = backend_memory_requirement(&cfg, &b).unwrap();
    assert_eq!(req_on.moe_cache_mib, 4096);
    assert_eq!(req_on.moe_cache_offload_layers, 16);
    assert_eq!(req_on.moe_cache_moe_layers, 48);
    let res_off = manager_optimize::residency_per_gpu(&req_off, &[], 1)[0];
    let res_on = manager_optimize::residency_per_gpu(&req_on, &[], 1)[0];
    assert_eq!(res_on, res_off + 4096);
    let failures = vram_gate_failures(
        &[0],
        &[(0, res_on)],
        &[vram_reading(0, res_on - 1, res_on - 1)],
        &[],
    );
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].class, VramGateClass::ExceedsTotal);
    assert_eq!(failures[0].required_mib, res_on);
    let failures = vram_gate_failures(
        &[0],
        &[(0, res_on)],
        &[vram_reading(0, res_on + 1024, res_on + 1024)],
        &[],
    );
    assert!(failures.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn declared_moe_model_gets_offload_info_once_its_file_parses() {
    // The slider's figures split by source: the layer count
    // and expert bytes come from the downloaded GGUF's parse;
    // the green limit — the one input that used to be
    // catalog-only — comes from the record's declaration.
    let dir = std::env::temp_dir().join(format!(
        "llm-mgr-declared-moe-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(dir.join("models")).unwrap();
    std::fs::write(
        dir.join("models").join("user-moe.gguf"),
        gguf_header_bytes(48, 0),
    )
    .unwrap();
    let mut cfg = test_cfg();
    cfg.data_dir = dir.clone();
    register_test_model(&mut cfg, "user-moe-27b", "models/user-moe.gguf");
    cfg.models.last_mut().unwrap().moe_layers = Some(48);
    cfg.models.last_mut().unwrap().default_offload_layers = Some(16);
    let b = uncreated_backend(
        "moe-backend",
        "models/user-moe.gguf",
        Some("--n-cpu-moe 16".to_string()),
    );
    let info = backend_offload_info(&cfg, &b).expect("declared MoE model is offload-capable");
    assert_eq!(info.moe_layers, 48);
    assert_eq!(info.green_limit_layers, 16);
    assert_eq!(info.layers, 16);
    assert!((info.green_end - 16.0 / 48.0).abs() < 1e-9);
    assert!(info.restrictions.is_empty());
    // Without the declaration the same parse yields no slider:
    // the green limit has no source.
    cfg.models.last_mut().unwrap().moe_layers = None;
    cfg.models.last_mut().unwrap().default_offload_layers = None;
    assert!(backend_offload_info(&cfg, &b).is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn user_declared_moe_model_round_trip() {
    // Pete's shape: a user-added 27B-class MoE model the
    // catalog does not carry, declared at add time.
    let (state, dir) = advanced_test_state(test_cfg());
    let sink = test_sink();
    let r = op_model_add(
        &state,
        &sink,
        "qwen3.8-27b-moe".to_string(),
        "https://example.com/Qwen3.8-27B-MoE.gguf".to_string(),
        None,
        None,
        Some(27.0),
        Some(48),
        Some(16),
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(r["model_id"], "qwen3.8-27b-moe");
    let saved = manager_config::load(&state.config_path).unwrap();
    let m = saved
        .models
        .iter()
        .find(|m| m.id == "qwen3.8-27b-moe")
        .unwrap();
    assert_eq!(m.moe_layers, Some(48));
    assert_eq!(m.default_offload_layers, Some(16));
    // The downloaded file lands: a 48-block MoE GGUF.
    std::fs::create_dir_all(dir.join("models")).unwrap();
    std::fs::write(
        dir.join("models").join("Qwen3.8-27B-MoE.gguf"),
        gguf_header_bytes(48, 0),
    )
    .unwrap();
    // Backend creation applies the declared default.
    let r = op_backend_add(
        &state,
        &sink,
        "moe-backend".to_string(),
        "qwen3.8-27b-moe".to_string(),
        8199,
        "moe".to_string(),
        true,
        true,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(r["backend_id"], "moe-backend");
    let saved = manager_config::load(&state.config_path).unwrap();
    let b = saved
        .backends
        .iter()
        .find(|b| b.id == "moe-backend")
        .unwrap()
        .clone();
    assert_eq!(b.raw_flags.as_deref(), Some("--n-cpu-moe 16"));
    // The add form's preview and the Edit Flags payload agree:
    // the control exists, ranged over the parsed layer count,
    // greened at the declared default.
    let preview = op_backend_add_preview(
        &state,
        &sink,
        "qwen3.8-27b-moe".to_string(),
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    let off = preview["offload"]
        .as_object()
        .expect("preview carries offload");
    assert_eq!(off["moe_layers"], 48);
    assert_eq!(off["green_limit_layers"], 16);
    assert_eq!(off["layers"], 16);
    let cfg_now = state.config.read().await.clone();
    let info = backend_offload_info(&cfg_now, &b).expect("offload info for the new backend");
    assert_eq!(info.moe_layers, 48);
    assert_eq!(info.green_limit_layers, 16);
    // The preview's requirement summary prices exactly what the
    // created backend's card prices (same configuration: the
    // catalog default offload at the 32K creation context).
    let req = backend_memory_requirement(&cfg_now, &b).expect("requirement for the new backend");
    let mem = preview["memory"]
        .as_object()
        .expect("preview carries memory");
    assert_eq!(mem["gpu_ram_mib"], json!(req.gpu_weights_mib));
    assert_eq!(mem["sys_ram_mib"], json!(req.sys_weights_mib));
    assert!(
        req.sys_weights_mib > 0,
        "partial offload holds experts in system RAM"
    );
    assert_eq!(mem["kv_ram_mib"], json!(req.kv_mib));
    assert!(mem["per_gpu"].is_array());
    // A larger context draft re-prices the KV term, never the
    // weights.
    let wide = op_backend_add_preview(
        &state,
        &sink,
        "qwen3.8-27b-moe".to_string(),
        None,
        Some(65536),
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(wide["memory"]["gpu_ram_mib"], mem["gpu_ram_mib"]);
    assert_eq!(wide["memory"]["sys_ram_mib"], mem["sys_ram_mib"]);
    if let (Some(a), Some(b2)) = (
        mem["kv_ram_mib"].as_u64(),
        wide["memory"]["kv_ram_mib"].as_u64(),
    ) {
        assert!(b2 > a, "KV at 64K exceeds KV at 32K");
    }
    // per_gpu: one entry per GPU the split uses, carrying its
    // share and the start gate's residency for it; an unsplit
    // placement loads on the first GPU.
    let gpus = test_gpus();
    let mut split_b = b.clone();
    split_b.flags.tensor_split = vec![0.75, 0.0, 0.25];
    let m = add_preview_memory_json(&cfg_now, &split_b, None, None, &gpus);
    let per = m["per_gpu"].as_array().unwrap();
    assert_eq!(per.len(), 2);
    assert_eq!(per[0]["gpu"], 0);
    assert_eq!(per[1]["gpu"], 2);
    assert_eq!(per[1]["total_mib"], 16384);
    let res = manager_optimize::residency_per_gpu(&req, &split_b.flags.tensor_split, 3);
    assert_eq!(per[0]["required_mib"], json!(res[0]));
    assert_eq!(per[1]["required_mib"], json!(res[2]));
    let mut unsplit = b.clone();
    unsplit.flags.tensor_split = Vec::new();
    let m = add_preview_memory_json(&cfg_now, &unsplit, None, None, &gpus);
    let per = m["per_gpu"].as_array().unwrap();
    assert_eq!(per.len(), 1);
    assert_eq!(per[0]["gpu"], 0);
    assert_eq!(per[0]["share"], 1.0);
    // The slider op rewrites only the offload tokens: partial,
    // full, and off — a sibling -ot assignment and an unrelated
    // flag survive every rewrite.
    op_backend_set_offload(&state, &sink, "moe-backend".to_string(), 24, None)
        .await
        .unwrap();
    let saved = manager_config::load(&state.config_path).unwrap();
    let flags = saved
        .backends
        .iter()
        .find(|b| b.id == "moe-backend")
        .unwrap()
        .raw_flags
        .clone()
        .unwrap();
    assert_eq!(flags, "--n-cpu-moe 24");
    // A sibling -ot assignment and an unrelated flag reach the
    // record the way the app allows: written as text in
    // advanced mode, carried verbatim when the backend returns
    // to structured mode.
    op_backend_set_flags_mode(&state, &sink, "moe-backend".to_string(), true)
        .await
        .unwrap();
    op_backend_set_raw_flags(
        &state,
        &sink,
        "moe-backend".to_string(),
        Some("--mlock -ot ffn_down_shexp=CPU --n-cpu-moe 24".to_string()),
    )
    .await
    .unwrap();
    op_backend_set_flags_mode(&state, &sink, "moe-backend".to_string(), false)
        .await
        .unwrap();
    op_backend_set_offload(&state, &sink, "moe-backend".to_string(), 48, None)
        .await
        .unwrap();
    let saved = manager_config::load(&state.config_path).unwrap();
    let flags = saved
        .backends
        .iter()
        .find(|b| b.id == "moe-backend")
        .unwrap()
        .raw_flags
        .clone()
        .unwrap();
    assert!(flags.contains("-ot exps=CPU"), "{flags}");
    assert!(flags.contains("-ot ffn_down_shexp=CPU"), "{flags}");
    assert!(flags.contains("--mlock"), "{flags}");
    assert!(!flags.contains("--n-cpu-moe"), "{flags}");
    op_backend_set_offload(&state, &sink, "moe-backend".to_string(), 0, None)
        .await
        .unwrap();
    let saved = manager_config::load(&state.config_path).unwrap();
    let flags = saved
        .backends
        .iter()
        .find(|b| b.id == "moe-backend")
        .unwrap()
        .raw_flags
        .clone()
        .unwrap();
    assert!(!flags.contains("--n-cpu-moe"), "{flags}");
    assert!(!flags.contains("-ot exps=CPU"), "{flags}");
    assert!(flags.contains("-ot ffn_down_shexp=CPU"), "{flags}");
    assert!(flags.contains("--mlock"), "{flags}");
    // Editing the declaration moves the default; clearing it
    // removes the control (the green limit loses its source).
    let r = op_model_set_offload_spec(
        &state,
        &sink,
        "qwen3.8-27b-moe".to_string(),
        Some(48),
        Some(48),
    )
    .await
    .unwrap();
    assert_eq!(r["default_offload_layers"], 48);
    let saved = manager_config::load(&state.config_path).unwrap();
    let m = saved
        .models
        .iter()
        .find(|m| m.id == "qwen3.8-27b-moe")
        .unwrap();
    assert_eq!(model_default_raw_flags(m).as_deref(), Some("-ot exps=CPU"));
    op_model_set_offload_spec(&state, &sink, "qwen3.8-27b-moe".to_string(), None, None)
        .await
        .unwrap();
    let saved = manager_config::load(&state.config_path).unwrap();
    let b = saved
        .backends
        .iter()
        .find(|b| b.id == "moe-backend")
        .unwrap()
        .clone();
    assert!(backend_offload_info(&saved, &b).is_none());
    // Invalid declarations refuse at both write paths and
    // persist nothing.
    assert!(matches!(
        op_model_set_offload_spec(
            &state,
            &sink,
            "qwen3.8-27b-moe".to_string(),
            Some(48),
            Some(49),
        )
        .await,
        Err(ApiError::InvalidInput(_))
    ));
    assert!(matches!(
        op_model_add(
            &state,
            &sink,
            "bad-moe".to_string(),
            "https://example.com/bad.gguf".to_string(),
            None,
            None,
            Some(9.0),
            Some(48),
            None,
            None,
            None,
        )
        .await,
        Err(ApiError::InvalidInput(_))
    ));
    let saved = manager_config::load(&state.config_path).unwrap();
    assert!(!saved.models.iter().any(|m| m.id == "bad-moe"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn user_declared_mtp_model_round_trip() {
    // A user-added model whose GGUF carries an MTP head the
    // curated catalog does not know about, declared at add
    // time — the declaration path that gives it the MTP
    // draft control (backend_mtp_info / backend_set_mtp
    // consume the EFFECTIVE capability).
    let (state, dir) = advanced_test_state(test_cfg());
    let sink = test_sink();
    let r = op_model_add(
        &state,
        &sink,
        "qwen3.6-27b-mtp".to_string(),
        "https://example.com/Qwen3.6-27B-mtp.gguf".to_string(),
        None,
        None,
        Some(27.0),
        None,
        None,
        Some(true),
        None,
    )
    .await
    .unwrap();
    assert_eq!(r["model_id"], "qwen3.6-27b-mtp");
    let saved = manager_config::load(&state.config_path).unwrap();
    let m = saved
        .models
        .iter()
        .find(|m| m.id == "qwen3.6-27b-mtp")
        .unwrap();
    assert_eq!(m.mtp, Some(true));
    // The declaration is stored in the config file; the
    // catalog pin field of the payload is null (the catalog
    // knows neither the id nor the URL).
    let text = std::fs::read_to_string(&state.config_path).unwrap();
    assert!(text.contains("\"mtp\": true"), "{text}");
    let listed = op_list_models(&state, &sink).await.unwrap();
    let row = listed["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["id"] == "qwen3.6-27b-mtp")
        .unwrap()
        .clone();
    assert_eq!(row["mtp"], true);
    assert_eq!(row["catalog_mtp"], serde_json::Value::Null);
    // A backend serving it gets the MTP control, and
    // backend_set_mtp canonicalizes the spec tokens.
    std::fs::create_dir_all(dir.join("models")).unwrap();
    std::fs::write(
        dir.join("models").join("Qwen3.6-27B-mtp.gguf"),
        gguf_header_bytes(0, 4),
    )
    .unwrap();
    op_backend_add(
        &state,
        &sink,
        "mtp-backend".to_string(),
        "qwen3.6-27b-mtp".to_string(),
        8197,
        "mtp".to_string(),
        true,
        true,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    let saved = manager_config::load(&state.config_path).unwrap();
    let b = saved
        .backends
        .iter()
        .find(|b| b.id == "mtp-backend")
        .unwrap()
        .clone();
    assert!(backend_mtp_info(&saved, &b).is_some());
    op_backend_set_mtp(&state, &sink, "mtp-backend".to_string(), Some(2))
        .await
        .unwrap();
    let saved = manager_config::load(&state.config_path).unwrap();
    let flags = saved
        .backends
        .iter()
        .find(|b| b.id == "mtp-backend")
        .unwrap()
        .raw_flags
        .clone()
        .unwrap();
    assert!(flags.contains("--spec-type draft-mtp"), "{flags}");
    assert!(flags.contains("--spec-draft-n-max 2"), "{flags}");
    // Declaring the model not capable removes the control
    // and the op — and does NOT rewrite the flags already on
    // the backend (the offload declaration's precedent).
    let r = op_model_set_mtp(&state, &sink, "qwen3.6-27b-mtp".to_string(), Some(false))
        .await
        .unwrap();
    assert_eq!(r["mtp"], false);
    let saved = manager_config::load(&state.config_path).unwrap();
    let b = saved
        .backends
        .iter()
        .find(|b| b.id == "mtp-backend")
        .unwrap()
        .clone();
    assert!(backend_mtp_info(&saved, &b).is_none());
    assert!(matches!(
        op_backend_set_mtp(&state, &sink, "mtp-backend".to_string(), Some(1)).await,
        Err(ApiError::InvalidInput(_))
    ));
    // Clearing the declaration outright behaves the same,
    // drops the field from the stored config entirely, and
    // still leaves the backend's flags untouched.
    op_model_set_mtp(&state, &sink, "qwen3.6-27b-mtp".to_string(), None)
        .await
        .unwrap();
    let saved = manager_config::load(&state.config_path).unwrap();
    assert_eq!(
        saved
            .models
            .iter()
            .find(|m| m.id == "qwen3.6-27b-mtp")
            .unwrap()
            .mtp,
        None
    );
    let text = std::fs::read_to_string(&state.config_path).unwrap();
    assert!(!text.contains("\"mtp\":"), "{text}");
    let flags_after = saved
        .backends
        .iter()
        .find(|b| b.id == "mtp-backend")
        .unwrap()
        .raw_flags
        .clone()
        .unwrap();
    assert_eq!(flags_after, flags);
    // An unknown model refuses with NotFound.
    assert!(matches!(
        op_model_set_mtp(&state, &sink, "nope".to_string(), Some(true)).await,
        Err(ApiError::NotFound(_))
    ));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn mtp_declaration_does_not_displace_a_catalog_pin() {
    let (state, dir) = advanced_test_state(test_cfg());
    let sink = test_sink();
    // Seed the catalog models into the config (the default
    // test config carries none until the catalog sync runs).
    op_list_models(&state, &sink).await.unwrap();
    // The catalog pins the coder NOT MTP-capable: a stored
    // declaration of true is shadowed by the pin.
    op_model_set_mtp(&state, &sink, "qwen3-coder-30b".to_string(), Some(true))
        .await
        .unwrap();
    // And the pinned-capable planner build stays capable
    // with a declaration of false stored on it.
    op_model_set_mtp(&state, &sink, "qwen3.8-27b-mtp".to_string(), Some(false))
        .await
        .unwrap();
    let saved = manager_config::load(&state.config_path).unwrap();
    let mut probe = saved.backends[0].clone();
    let coder_model = saved
        .models
        .iter()
        .find(|m| m.id == "qwen3-coder-30b")
        .unwrap();
    probe.model_file = coder_model.gguf_path.clone();
    assert!(backend_mtp_info(&saved, &probe).is_none());
    let mtp_model = saved
        .models
        .iter()
        .find(|m| m.id == "qwen3.8-27b-mtp")
        .unwrap();
    probe.model_file = mtp_model.gguf_path.clone();
    assert!(backend_mtp_info(&saved, &probe).is_some());
    // list_models reports pin and declaration side by side.
    let listed = op_list_models(&state, &sink).await.unwrap();
    let rows = listed["models"].as_array().unwrap();
    let coder_row = rows.iter().find(|v| v["id"] == "qwen3-coder-30b").unwrap();
    assert_eq!(coder_row["catalog_mtp"], false);
    assert_eq!(coder_row["mtp"], true);
    let mtp_row = rows.iter().find(|v| v["id"] == "qwen3.8-27b-mtp").unwrap();
    assert_eq!(mtp_row["catalog_mtp"], true);
    assert_eq!(mtp_row["mtp"], false);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn dense_user_model_gets_no_offload_control() {
    let (state, dir) = advanced_test_state(test_cfg());
    let sink = test_sink();
    op_model_add(
        &state,
        &sink,
        "dense-7b".to_string(),
        "https://example.com/dense-7b.gguf".to_string(),
        None,
        None,
        Some(7.0),
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    std::fs::create_dir_all(dir.join("models")).unwrap();
    std::fs::write(
        dir.join("models").join("dense-7b.gguf"),
        gguf_header_bytes(0, 4),
    )
    .unwrap();
    op_backend_add(
        &state,
        &sink,
        "dense-backend".to_string(),
        "dense-7b".to_string(),
        8198,
        "moe".to_string(),
        true,
        true,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    let saved = manager_config::load(&state.config_path).unwrap();
    let b = saved
        .backends
        .iter()
        .find(|b| b.id == "dense-backend")
        .unwrap()
        .clone();
    assert_eq!(b.raw_flags, None);
    assert!(backend_offload_info(&saved, &b).is_none());
    let preview = op_backend_add_preview(
        &state,
        &sink,
        "dense-7b".to_string(),
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    assert!(preview["offload"].is_null());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn offload_info_payload_fields_for_coder_and_nemotron() {
    // sys_bytes_by_position is exact for Coder-Next (uniform
    // expert blocks): index 0 is empty, index 48 is the full
    // expert bytes.
    let mut cfg = test_cfg();
    let b = serve_model(
        &mut cfg,
        "coder",
        "qwen3-coder-30b",
        "models/Qwen3-Coder-Next-UD-Q4_K_M.gguf",
        Some("--n-cpu-moe 46"),
    );
    let info = backend_offload_info(&cfg, &b).expect("coder info");
    assert_eq!(info.sys_bytes_by_position.len(), 49);
    assert_eq!(info.sys_bytes_by_position[0], 0);
    assert_eq!(info.sys_bytes_by_position[48], info.expert_bytes);
    assert!(!info.custom, "canonical spelling is not custom");
    assert_eq!(info.restrictions.len(), 1);

    // A padded-but-equivalent spelling is not custom either;
    // a spelling the canonical form would not produce is.
    let b = serve_model(
        &mut cfg,
        "coder",
        "qwen3-coder-30b",
        "models/Qwen3-Coder-Next-UD-Q4_K_M.gguf",
        Some("  --n-cpu-moe   46  "),
    );
    let info = backend_offload_info(&cfg, &b).expect("coder info");
    assert!(!info.custom, "whitespace-only difference is not custom");

    let b = serve_model(
        &mut cfg,
        "coder-fast",
        "nemotron-3.5-lightning-30b",
        "models/Nemotron.gguf",
        Some("--n-cpu-moe=16"),
    );
    let info = backend_offload_info(&cfg, &b).expect("nemotron info");
    assert!(
        info.custom,
        "the `=` spelling is not the canonical form for the position"
    );
    assert_eq!(info.sys_bytes_by_position.len(), 25);
    assert_eq!(info.restrictions.len(), 1);
}

#[test]
fn backend_cache_info_distinguishes_pre_feature_and_custom() {
    let cfg = test_cfg();
    let mut b = backend(&cfg, "planner");
    b.raw_flags = None;
    let info = backend_cache_info(&b);
    assert!(!info.present);
    assert!(!info.custom);
    assert_eq!(info.selection, manager_models::CacheSelection::default());
    b.raw_flags = Some("--parallel 1".to_string());
    assert!(!backend_cache_info(&b).present);
    b.raw_flags = Some("--cache-type-k q8_0 --cache-type-v q8_0 --flash-attn on".to_string());
    let info = backend_cache_info(&b);
    assert!(info.present);
    assert!(!info.custom);
    assert_eq!(info.selection.k, manager_models::CacheType::Q8_0);
    // A short-spelling saved form is present but custom.
    b.raw_flags = Some("-ctk q8_0".to_string());
    let info = backend_cache_info(&b);
    assert!(info.present);
    assert!(info.custom);
}

/// The previews price an unsaved MoE position and placement (G02, G03)
/// with the figures the saved configuration would get, refuse what the
/// save refuses, and write nothing.
#[tokio::test]
async fn previews_price_an_unsaved_position_and_placement() {
    let mut cfg = test_cfg();
    serve_model(
        &mut cfg,
        "coder",
        "qwen3-coder-30b",
        "models/Qwen3-Coder-Next-UD-Q4_K_M.gguf",
        Some("--n-cpu-moe 40"),
    );
    let (state, dir) = advanced_test_state(cfg);
    let sink = test_sink();
    let gpus = test_gpus();
    lock_telemetry(&state.telemetry().await).set_inventory(gpus.clone());
    let cache = |layers: Option<u32>, gpu_indices: Option<Vec<u32>>| {
        op_backend_cache_preview(
            &state,
            &sink,
            "coder".to_string(),
            None,
            None,
            None,
            None,
            None,
            None,
            layers,
            gpu_indices,
        )
    };
    let add = |layers: Option<u32>| {
        op_backend_add_preview(
            &state,
            &sink,
            "qwen3-coder-30b".to_string(),
            Some(vec![gpus[0].index]),
            None,
            None,
            layers,
        )
    };
    let saved = cache(None, None).await.unwrap();
    let same = cache(Some(40), None).await.unwrap();
    assert_eq!(saved, same, "the saved position, spelled out");
    let fewer = cache(Some(20), None).await.unwrap();
    assert!(fewer["weights_mib"].as_u64() > saved["weights_mib"].as_u64());
    assert!(fewer["sys_weights_mib"].as_u64() < saved["sys_weights_mib"].as_u64());
    // The add form's preview agrees, position by position.
    for (layers, want) in [(20, &fewer), (40, &saved)] {
        let p = add(Some(layers)).await.unwrap();
        assert_eq!(p["offload"]["layers"], layers);
        assert_eq!(p["memory"]["gpu_ram_mib"], want["weights_mib"]);
        assert_eq!(p["memory"]["sys_ram_mib"], want["sys_weights_mib"]);
        assert_eq!(p["cache"]["weights_mib"], want["weights_mib"]);
    }
    // A placement: the named GPUs, evenly.
    let placed = cache(None, Some(vec![gpus[0].index, gpus[1].index]))
        .await
        .unwrap();
    let targets = placed["targets"].as_array().unwrap();
    assert_eq!(
        targets.iter().map(|t| t["gpu"].clone()).collect::<Vec<_>>(),
        [json!(gpus[0].index), json!(gpus[1].index)]
    );
    for bad in [
        cache(Some(49), None).await,
        cache(None, Some(vec![99])).await,
    ] {
        assert!(matches!(bad, Err(ApiError::InvalidInput(_))), "{bad:?}");
    }
    assert!(matches!(
        add(Some(49)).await,
        Err(ApiError::InvalidInput(_))
    ));
    let live = state.config.read().await;
    let b = live.backends.iter().find(|b| b.id == "coder").unwrap();
    assert_eq!(b.raw_flags.as_deref(), Some("--n-cpu-moe 40"));
    assert!(!state.config_path.exists(), "nothing was saved");
    drop(live);
    let _ = std::fs::remove_dir_all(&dir);
}

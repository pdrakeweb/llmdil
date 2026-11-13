use super::*;
use crate::test_support::*;

#[tokio::test]
async fn backend_set_moe_cache_round_trip_and_refusals() {
    let (state, dir) = advanced_test_state(test_cfg());
    let sink = test_sink();
    op_model_add(
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
    std::fs::create_dir_all(dir.join("models")).unwrap();
    std::fs::write(
        dir.join("models").join("Qwen3.8-27B-MoE.gguf"),
        gguf_header_bytes(48, 0),
    )
    .unwrap();
    op_backend_add(
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
    // The declared default offload (16) is in place; the cache
    // op rewrites only its own token.
    let r = op_backend_set_moe_cache(&state, &sink, "moe-backend".to_string(), Some(4096), None)
        .await
        .unwrap();
    assert_eq!(r["mib"], 4096);
    assert_eq!(r["changed"], true);
    let saved = manager_config::load(&state.config_path).unwrap();
    let b = saved
        .backends
        .iter()
        .find(|b| b.id == "moe-backend")
        .unwrap();
    assert_eq!(
        b.raw_flags.as_deref(),
        Some("--n-cpu-moe 16 --moe-cache-mib 4096")
    );
    // An explicit size and Auto at once is ambiguous input.
    let r = op_backend_set_moe_cache(
        &state,
        &sink,
        "moe-backend".to_string(),
        Some(4096),
        Some(true),
    )
    .await;
    assert!(matches!(r, Err(ApiError::InvalidInput(_))), "{r:?}");
    // Offload 0 refuses a nonzero cache with the reason;
    // turning the cache off stays allowed, so a stranded
    // token can always be cleared.
    op_backend_set_offload(&state, &sink, "moe-backend".to_string(), 0, None)
        .await
        .unwrap();
    let r =
        op_backend_set_moe_cache(&state, &sink, "moe-backend".to_string(), Some(8192), None).await;
    match r {
        Err(ApiError::InvalidInput(msg)) => {
            assert!(msg.contains("offloads 0 MoE layers"), "{msg}")
        }
        other => panic!("expected the offload-0 refusal, got {other:?}"),
    }
    let r = op_backend_set_moe_cache(&state, &sink, "moe-backend".to_string(), None, None)
        .await
        .unwrap();
    assert_eq!(r["mib"], Value::Null);
    let saved = manager_config::load(&state.config_path).unwrap();
    let b = saved
        .backends
        .iter()
        .find(|b| b.id == "moe-backend")
        .unwrap();
    assert!(
        !b.raw_flags.as_deref().unwrap_or("").contains("moe-cache"),
        "{:?}",
        b.raw_flags
    );
    // Auto cannot price anything in this environment (no GPU
    // telemetry): it refuses rather than guessing a size.
    op_backend_set_offload(&state, &sink, "moe-backend".to_string(), 16, None)
        .await
        .unwrap();
    let r =
        op_backend_set_moe_cache(&state, &sink, "moe-backend".to_string(), None, Some(true)).await;
    assert!(matches!(r, Err(ApiError::InvalidInput(_))), "{r:?}");
    // A row (tensor) split refuses a nonzero cache: the
    // pinned llama.cpp rejects the cache under tensor
    // parallelism.
    {
        let mut cfg = state.config.write().await;
        let b = cfg
            .backends
            .iter_mut()
            .find(|b| b.id == "moe-backend")
            .unwrap();
        b.flags.split_mode = manager_config::SplitMode::Row;
    }
    let r =
        op_backend_set_moe_cache(&state, &sink, "moe-backend".to_string(), Some(4096), None).await;
    match r {
        Err(ApiError::InvalidInput(msg)) => {
            assert!(msg.contains("row (tensor) split"), "{msg}")
        }
        other => panic!("expected the row-split refusal, got {other:?}"),
    }
    // A dense backend has no expert cache to set.
    let r = op_backend_set_moe_cache(&state, &sink, "planner".to_string(), Some(4096), None).await;
    match r {
        Err(ApiError::InvalidInput(msg)) => {
            assert!(msg.contains("does not support an expert cache"), "{msg}")
        }
        other => panic!("expected the dense refusal, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// --- KV cache / flash attention integration (feature/backend-kv-fa) ---

fn planner_cache_layout() -> WeightLayout {
    WeightLayout {
        total_bytes: 12_120_016_960,
        expert_bytes: None,
        expert_bytes_by_block: None,
        expert_tensors: None,
        raw_flags: None,
        moe_layers: None,
        kv_geometry: Some(manager_models::KvGeometry {
            kv_layers: 16,
            head_count_kv: 4,
            key_length: 256,
            value_length: 256,
        }),
        kv_per_token: Some(65_536),
        rs_bytes_per_seq: Some(156_893_184),
        basis: manager_models::MemoryBasis::Exact,
        catalog: None,
        green_limit_layers: None,
    }
}

#[test]
fn selected_cache_types_price_the_requirement() {
    // Pricing delta at the integration seam: the same layout
    // priced through kv_bytes_per_token_for (the function
    // backend_memory_requirement uses) at each selection.
    let layout = planner_cache_layout();
    assert_eq!(kv_bytes_per_token_for(&layout, None), Some(65_536));
    assert_eq!(
        kv_bytes_per_token_for(
            &layout,
            Some("--cache-type-k f16 --cache-type-v f16 --flash-attn on")
        ),
        Some(65_536),
        "an explicit f16 selection prices exactly as the pre-feature default"
    );
    assert_eq!(
        kv_bytes_per_token_for(
            &layout,
            Some("--cache-type-k q8_0 --cache-type-v q8_0 --flash-attn on")
        ),
        Some(34_816)
    );
    assert_eq!(
        kv_bytes_per_token_for(
            &layout,
            Some("--cache-type-k q8_0 --cache-type-v q4_0 --flash-attn on")
        ),
        Some(26_624)
    );
    assert_eq!(
        kv_bytes_per_token_for(
            &layout,
            Some("--cache-type-k q4_0 --cache-type-v q4_0 --flash-attn on")
        ),
        Some(18_432)
    );
    // The requirement totals differ by exactly the KV deltas
    // at 32K context, 1 slot: f16 2048 MiB, q8/q8 1088, q4/q4 576.
    let kv_mib = |raw: Option<&str>| {
        manager_models::memory_requirement(
            layout.total_bytes,
            layout.expert_bytes,
            None,
            layout.basis,
            manager_models::ExpertOffload::None,
            kv_bytes_per_token_for(&layout, raw),
            layout.moe_layers,
            32_768,
            layout.rs_bytes_per_seq,
            1,
        )
        .kv_mib
    };
    assert_eq!(kv_mib(None), Some(2_048));
    assert_eq!(
        kv_mib(Some("--cache-type-k q8_0 --cache-type-v q8_0")),
        Some(1_088)
    );
    assert_eq!(
        kv_mib(Some("--cache-type-k q4_0 --cache-type-v q4_0")),
        Some(576)
    );
    // No geometry: the f16 figure is used unchanged, never zero.
    let mut no_geo = planner_cache_layout();
    no_geo.kv_geometry = None;
    assert_eq!(
        kv_bytes_per_token_for(&no_geo, Some("--cache-type-k q4_0")),
        Some(65_536)
    );
}

#[test]
fn cache_selection_resolver_validates_the_wire_values() {
    let s = |v: &str| Some(v.to_string());
    assert_eq!(resolve_cache_selection(None, None, None).unwrap(), None);
    let sel = resolve_cache_selection(s("q8_0"), s("q4_0"), s("on"))
        .unwrap()
        .unwrap();
    assert_eq!(sel.k, manager_models::CacheType::Q8_0);
    assert_eq!(sel.v, manager_models::CacheType::Q4_0);
    assert_eq!(sel.flash_attn, manager_models::FlashAttn::On);
    // Partial sets are refused.
    assert!(resolve_cache_selection(s("q8_0"), None, None).is_err());
    assert!(resolve_cache_selection(None, s("q8_0"), s("on")).is_err());
    // Unoffered types and bad FA spellings are refused.
    assert!(resolve_cache_selection(s("q5_0"), s("q8_0"), s("on")).is_err());
    assert!(resolve_cache_selection(s("q8_0"), s("q8_0"), s("banana")).is_err());
    // The pairing llama.cpp refuses is refused here too.
    assert!(resolve_cache_selection(s("q8_0"), s("q8_0"), s("off")).is_err());
    assert!(resolve_cache_selection(s("f16"), s("f16"), s("off")).is_ok());
}

#[test]
fn cache_preview_reports_best_and_honest_no_fit() {
    let layout = planner_cache_layout();
    let targets = (Vec::new(), vec![16_384u64], vec![0u32], vec![None]);
    let sel_f16 = manager_models::CacheSelection::default();
    // 64K: preview prices f16, best is the q8_0 rung.
    let p = cache_preview_json(
        &layout,
        manager_models::ExpertOffload::None,
        sel_f16,
        65_536,
        1,
        0,
        Some(&targets),
    );
    assert_eq!(p["kv_mib"], 4_096);
    assert_eq!(p["fits_current"], false);
    assert_eq!(p["best"]["k"], "q8_0");
    assert_eq!(p["best"]["v"], "q8_0");
    assert_eq!(p["headroom_reserve_percent"], 5);
    // The single target carries the whole total as its
    // required share, with its own verdict (over the 5%
    // headroom limit here, so false like fits_current).
    assert_eq!(p["targets"][0]["gpu"], 0);
    assert_eq!(p["targets"][0]["total_mib"], 16_384);
    assert_eq!(p["targets"][0]["required_mib"], 11_559 + 4_096 + 150);
    assert_eq!(p["targets"][0]["fits"], false);
    // The three-state classification judges total VRAM, not
    // the headroom reserve: under total is "fits" here even
    // though the headroom verdict above is false, and the
    // driver model rides along (unreported in this fixture).
    assert_eq!(p["targets"][0]["fit_state"], "fits");
    assert_eq!(p["targets"][0]["paged_mib"], 0);
    assert_eq!(p["targets"][0]["driver_model"], Value::Null);
    // Split across two targets: shares follow the split and
    // each target's verdict is its own — the small card
    // refuses its half while the large one takes it.
    let split_targets = (
        vec![0.5, 0.5],
        vec![16_384u64, 4_096],
        vec![0u32, 3],
        vec![None, None],
    );
    let p = cache_preview_json(
        &layout,
        manager_models::ExpertOffload::None,
        sel_f16,
        32_768,
        1,
        0,
        Some(&split_targets),
    );
    let half = (11_559 + 2_048 + 150) / 2 + (11_559 + 2_048 + 150) % 2;
    assert_eq!(p["targets"][0]["gpu"], 0);
    assert_eq!(p["targets"][0]["required_mib"], half);
    assert_eq!(p["targets"][0]["fits"], true);
    assert_eq!(p["targets"][1]["gpu"], 3);
    assert_eq!(
        p["targets"][1]["required_mib"],
        (11_559 + 2_048 + 150) - half
    );
    assert_eq!(p["targets"][1]["fits"], false);
    // Over total with no WDDM driver model reported: the
    // three-state verdict is a hard does_not_fit, no paging.
    assert_eq!(p["targets"][1]["fit_state"], "does_not_fit");
    assert_eq!(p["targets"][1]["paged_mib"], 0);
    assert_eq!(p["fits_current"], false);
    // 256K: no rung fits; best is null and the ladder's lowest
    // rung carries the honest numbers (nothing is applied).
    let p = cache_preview_json(
        &layout,
        manager_models::ExpertOffload::None,
        sel_f16,
        262_144,
        1,
        0,
        Some(&targets),
    );
    assert_eq!(p["best"], Value::Null);
    assert_eq!(p["ladder"][3]["total_mib"], 11_559 + 4_608 + 150);
    assert_eq!(p["ladder"][3]["fits"], false);
}

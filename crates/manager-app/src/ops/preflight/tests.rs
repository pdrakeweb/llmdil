use super::*;
use crate::test_support::*;

/// PS-12 (docs/code-review-v0.1.60.md): a poisoned
/// `verifying_downloads` lock must not panic the start preflight;
/// the gate still reports the in-flight verification.
#[tokio::test]
async fn start_preflight_tolerates_poisoned_verifying_lock() {
    let mut cfg = test_cfg();
    cfg.backends
        .iter_mut()
        .find(|b| b.id == "coder")
        .unwrap()
        .enabled = true;
    let (state, dir) = advanced_test_state(cfg);
    // Seed the catalog so the coder backend's model is a known row.
    ensure_catalog_models(&state).await;
    let cfg = state.config.read().await.clone();
    let b = cfg
        .backends
        .iter()
        .find(|b| b.id == "coder")
        .unwrap()
        .clone();
    let target = resolve_under_data_dir(&cfg.data_dir, &b.model_file);
    let model_id = cfg
        .models
        .iter()
        .find(|m| resolve_under_data_dir(&cfg.data_dir, &m.gguf_path) == target)
        .expect("the coder backend's model is in the catalog")
        .id
        .clone();
    state
        .downloads
        .verifying
        .lock()
        .unwrap()
        .insert(model_id.clone());
    let poisoner = Arc::clone(&state);
    let _ = std::thread::spawn(move || {
        let _guard = poisoner.downloads.verifying.lock().unwrap();
        panic!("poison the verifying_downloads lock");
    })
    .join();
    assert!(state.downloads.verifying.is_poisoned());
    let err = backend_start_preflight_for(&state, &cfg, &b, false)
        .await
        .expect_err("a model under verification refuses the start");
    assert!(
        err.to_string().contains("verification is still running"),
        "{err}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

fn vram_reading_down(index: u32, total_mib: u64, free_mib: u64) -> VramGateReading {
    VramGateReading {
        index,
        total_mib,
        free_mib,
        down: true,
        driver_model: None,
    }
}

fn vram_reading_driver(
    index: u32,
    total_mib: u64,
    free_mib: u64,
    driver_model: DriverModel,
) -> VramGateReading {
    VramGateReading {
        index,
        total_mib,
        free_mib,
        down: false,
        driver_model: Some(driver_model),
    }
}

#[test]
fn vram_gate_passes_when_requirement_fits_free() {
    // 12 GiB required on GPU 1, 15 GiB free of 16 GiB: fits.
    let failures = vram_gate_failures(
        &[1],
        &[(1, 12 * 1024)],
        &[vram_reading(1, 16 * 1024, 15 * 1024)],
        &[],
    );
    assert!(failures.is_empty());
}

#[test]
fn vram_gate_refuses_when_free_is_short() {
    // The planner-moe shape: ~15.5 GiB required on the P100,
    // ~11 GiB free because another backend is resident. Under
    // the card total, so the cause is insufficient free memory.
    let failures = vram_gate_failures(
        &[1],
        &[(1, 15_862)],
        &[vram_reading(1, 16_384, 11_245)],
        &[],
    );
    assert_eq!(failures.len(), 1);
    let f = &failures[0];
    assert_eq!(f.index, 1);
    assert_eq!(f.required_mib, 15_862);
    assert_eq!(f.free_mib, 11_245);
    assert_eq!(f.total_mib, 16_384);
    assert_eq!(f.class, VramGateClass::InsufficientFree);
}

#[test]
fn preflight_requirement_includes_recurrent_state_cache() {
    // The planner-moe incident end to end through the
    // production requirement: Whittle-MoE-27B Q8_0 file
    // (27,350 MiB of tensors, 13,536 MiB of them routed
    // experts, all offloaded), KV 65,536 B/token at n_ctx
    // 32768, recurrent-state geometry 156,893,184 B per
    // sequence at the auto slot count of 4. llama.cpp's own
    // figures for this configuration total 16,460 MiB
    // (13,813.54 weights + 598.50 state + 2,048 KV) — over a
    // 16,384 MiB card's total even when the card is empty, so
    // the gate must refuse with the exceeds-total class. The
    // weights+KV subtotal alone (15,862 MiB) is exactly the
    // figure the pre-rs-term requirement produced when the
    // start was wrongly allowed.
    let mib_bytes = |m: u64| m * manager_models::MIB;
    let req = manager_models::memory_requirement(
        mib_bytes(27_350),
        Some(mib_bytes(13_536)),
        None,
        manager_models::MemoryBasis::Exact,
        manager_models::ExpertOffload::AllExperts,
        Some(65_536),
        Some(64),
        32_768,
        Some(156_893_184),
        4,
    );
    assert_eq!(req.gpu_weights_mib, 13_814);
    assert_eq!(req.kv_mib, Some(2_048));
    assert_eq!(req.rs_mib, Some(599));
    let per_gpu = manager_optimize::residency_per_gpu(&req, &[0.0, 1.0], 2);
    assert_eq!(per_gpu, vec![0, 16_461]);
    let required: Vec<(u32, u64)> = per_gpu
        .iter()
        .enumerate()
        .filter(|(_, mib)| **mib > 0)
        .map(|(i, mib)| (i as u32, *mib))
        .collect();
    let failures = vram_gate_failures(&[1], &required, &[vram_reading(1, 16_384, 16_384)], &[]);
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].class, VramGateClass::ExceedsTotal);
    assert_eq!(failures[0].required_mib, 16_461);
    assert!(failures[0].required_mib >= 16_460);
}

#[test]
fn vram_gate_classifies_requirement_over_card_total() {
    // More required than the card holds at all: a different
    // cause class than a busy card, even with every byte free.
    let failures = vram_gate_failures(
        &[0],
        &[(0, 17 * 1024)],
        &[vram_reading(0, 16 * 1024, 16 * 1024)],
        &[],
    );
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].class, VramGateClass::ExceedsTotal);
    assert_eq!(failures[0].free_mib, 16 * 1024);
}

#[test]
fn vram_gate_split_flags_only_the_short_gpu() {
    // Split across two GPUs: GPU 0 has room, GPU 1 does not.
    // Only the short GPU is named.
    let failures = vram_gate_failures(
        &[0, 1],
        &[(0, 8 * 1024), (1, 8 * 1024)],
        &[
            vram_reading(0, 16 * 1024, 16 * 1024),
            vram_reading(1, 16 * 1024, 4 * 1024),
        ],
        &[],
    );
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].index, 1);
}

#[test]
fn vram_gate_credit_covers_a_restart() {
    // A restart of a backend already holding most of the card:
    // measured free is tiny, but its own residency is credited
    // back (the restart frees it before allocating), so the
    // same configuration passes its own gate.
    let failures = vram_gate_failures(
        &[1],
        &[(1, 15 * 1024)],
        &[vram_reading(1, 16 * 1024, 1024)],
        &[(1, 14_500)],
    );
    assert!(failures.is_empty());
    // A requirement grown past what the credit covers still
    // fails: 15 GiB required, 1 GiB free + 13 GiB credited.
    let failures = vram_gate_failures(
        &[1],
        &[(1, 15 * 1024)],
        &[vram_reading(1, 16 * 1024, 1024)],
        &[(1, 13 * 1024)],
    );
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].free_mib, 1024 + 13 * 1024);
}

#[test]
fn vram_gate_down_gpu_is_unavailable_not_short() {
    // The thermal-offline shape: the placed GPU fell out of
    // the telemetry polls and is retained flagged down with
    // last-known figures that would otherwise "fit". The
    // class is unavailable, not a memory verdict — the
    // figures are not trustworthy.
    let failures = vram_gate_failures(
        &[1],
        &[(1, 12 * 1024)],
        &[vram_reading_down(1, 16 * 1024, 15 * 1024)],
        &[],
    );
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].class, VramGateClass::GpuUnavailable);
    assert_eq!(failures[0].index, 1);
}

#[test]
fn vram_gate_missing_gpu_in_live_snapshot_is_unavailable() {
    // Telemetry reads GPU 0 fine but has never reported the
    // placed GPU 1 (the app started while the card was
    // offline): unavailable, not a skip.
    let failures = vram_gate_failures(
        &[1],
        &[(1, 12 * 1024)],
        &[vram_reading(0, 16 * 1024, 16 * 1024)],
        &[],
    );
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].class, VramGateClass::GpuUnavailable);
    assert_eq!(failures[0].index, 1);
}

#[test]
fn vram_gate_zero_total_is_unavailable() {
    // A reading whose total memory is zero is a device not
    // responding with real figures, not a very full card.
    let failures = vram_gate_failures(&[0], &[(0, 1024)], &[vram_reading(0, 0, 0)], &[]);
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].class, VramGateClass::GpuUnavailable);
}

#[test]
fn vram_gate_wddm_over_total_is_confirmable_with_paged_amount() {
    // Pete's configuration (Oct 23, 2026): 17,224 MiB required
    // on a 16,376 MiB WDDM GPU (the expert cache is 4,096 of
    // it — without the cache the same backend needs 13,128
    // and passes outright). The failure carries the paged
    // excess, and the failure set is confirmable.
    let failures = vram_gate_failures(
        &[0],
        &[(0, 17_224)],
        &[vram_reading_driver(0, 16_376, 15_098, DriverModel::Wddm)],
        &[],
    );
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].class, VramGateClass::ExceedsTotal);
    assert_eq!(failures[0].paged_mib, 848);
    assert!(failures_are_wddm_overcommit(&failures));
    // Zero cache changes nothing about the gate's other
    // shapes: the uncached requirement fits free and passes.
    let failures = vram_gate_failures(
        &[0],
        &[(0, 13_128)],
        &[vram_reading_driver(0, 16_376, 15_098, DriverModel::Wddm)],
        &[],
    );
    assert!(failures.is_empty());
}

#[test]
fn vram_gate_tcc_or_unknown_over_total_is_not_confirmable() {
    // The same excess on a TCC GPU (the P100 posture) is a
    // hard refusal: no paged amount, not confirmable. An
    // unreported driver model is judged the same way.
    for reading in [
        vram_reading_driver(0, 16_376, 15_098, DriverModel::Tcc),
        vram_reading(0, 16_376, 15_098),
    ] {
        let failures = vram_gate_failures(&[0], &[(0, 17_224)], &[reading], &[]);
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].class, VramGateClass::ExceedsTotal);
        assert_eq!(failures[0].paged_mib, 0);
        assert!(!failures_are_wddm_overcommit(&failures));
    }
    // Insufficient free UNDER the total stays a hard refusal
    // even on WDDM: paging is judged against total VRAM, not
    // against transient free pressure.
    let failures = vram_gate_failures(
        &[0],
        &[(0, 15_000)],
        &[vram_reading_driver(0, 16_376, 10_000, DriverModel::Wddm)],
        &[],
    );
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].class, VramGateClass::InsufficientFree);
    assert!(!failures_are_wddm_overcommit(&failures));
    // A mixed set (one WDDM over-total, one unavailable) is
    // not confirmable as a whole.
    let failures = vram_gate_failures(
        &[0, 1],
        &[(0, 17_224), (1, 1024)],
        &[
            vram_reading_driver(0, 16_376, 15_098, DriverModel::Wddm),
            vram_reading_down(1, 16_376, 15_098),
        ],
        &[],
    );
    assert_eq!(failures.len(), 2);
    assert!(!failures_are_wddm_overcommit(&failures));
}

#[test]
fn requirement_breakdown_itemizes_the_expert_cache() {
    // The refusal text's arithmetic: the itemized terms sum
    // to the gate's required figure, expert cache included.
    let req = manager_models::MemoryRequirement {
        gpu_weights_mib: 10_865,
        sys_weights_mib: 0,
        kv_mib: Some(1_638),
        rs_mib: Some(614),
        moe_cache_mib: 4_096,
        moe_cache_offload_layers: 48,
        moe_cache_moe_layers: 64,
        basis: manager_models::MemoryBasis::Exact,
    };
    assert_eq!(
        requirement_breakdown(&req),
        "weights 10865 MiB + KV cache 1638 MiB + state cache 614 MiB + expert cache 4096 MiB"
    );
    // Zero cache: no expert-cache term, nothing else changes.
    let req = manager_models::MemoryRequirement {
        moe_cache_mib: 0,
        ..req
    };
    assert_eq!(
        requirement_breakdown(&req),
        "weights 10865 MiB + KV cache 1638 MiB + state cache 614 MiB"
    );
}

#[test]
fn vram_gate_empty_snapshot_judges_nothing() {
    // Telemetry has nothing at all (not started, no NVML):
    // the gate makes no claim in either direction.
    let failures = vram_gate_failures(&[1], &[(1, 12 * 1024)], &[], &[]);
    assert!(failures.is_empty());
}

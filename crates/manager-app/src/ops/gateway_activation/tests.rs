use super::*;
use crate::test_support::*;

/// PS-10 self-heal: a cancel that saved the restored config but
/// stopped before clearing the journal leaves the configuration
/// equal to the snapshot; the next cancel clears the stale
/// journal instead of refusing forever.
#[tokio::test]
async fn activation_cancel_clears_a_stale_journal() {
    let cfg = test_cfg();
    let (state, dir) = advanced_test_state(cfg);
    let current = state.config.read().await.clone();
    let mut after = current.clone();
    after.gateway.port += 1;
    let configured: std::collections::HashSet<String> =
        current.backends.iter().map(|b| b.id.clone()).collect();
    activation::record_change(
        &dir,
        &current,
        &after,
        "gateway_config",
        "cancelled before the crash".to_string(),
        "gateway",
        Vec::new(),
        &std::collections::HashMap::new(),
        &configured,
        &std::collections::HashSet::new(),
    )
    .unwrap();
    activation::record_offload_override(&dir, "coder", 24, vec![(8, 9)]).unwrap();
    let r = op_gateway_activation_cancel(&state, &test_sink())
        .await
        .unwrap();
    assert_eq!(r["cancelled"], true, "{r}");
    assert_eq!(r["already_restored"], true, "{r}");
    let journal = activation::load_journal(&dir);
    assert!(journal.entries.is_empty());
    assert!(journal.snapshot.is_none());
    assert_eq!(journal.offload_overrides.len(), 1, "overrides are kept");
    let _ = std::fs::remove_dir_all(&dir);
}

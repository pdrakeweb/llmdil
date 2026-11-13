use super::*;
use crate::test_support::*;

/// PS-5 (docs/code-review-v0.1.60.md): changing the exposure of a
/// persisted MCP server that has no built-in local-process spec
/// (the curated `brave_search`) must not panic.
#[tokio::test]
async fn mcp_set_exposure_on_curated_server_does_not_panic() {
    let (state, dir) = advanced_test_state(test_cfg());
    let sink = test_sink();
    let id = state
        .config
        .read()
        .await
        .mcp
        .servers
        .iter()
        .map(|s| s.id.clone())
        .find(|id| !manager_mcp::default_servers().iter().any(|d| &d.id == id))
        .expect("a persisted server outside the built-in local specs");
    let v = op_mcp_set_exposure(&state, &sink, id.clone(), "disabled".to_string())
        .await
        .unwrap();
    assert_eq!(v["persisted"], true);
    let err = op_mcp_set_exposure(&state, &sink, id.clone(), "local".to_string())
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("no local process definition"),
        "{err}"
    );
    // The exposure itself was persisted.
    let cfg = state.config.read().await;
    let s = cfg.mcp.servers.iter().find(|s| s.id == id).unwrap();
    assert_eq!(s.exposure, manager_config::McpExposure::Local);
    drop(cfg);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn mcp_signup_url_prefers_stored_then_curated_seed() {
    let cfg = test_cfg();
    let brave = cfg
        .mcp
        .servers
        .iter()
        .find(|s| s.id == "brave_search")
        .expect("default config seeds brave_search");
    // The seeded entry carries the curated URL itself.
    assert_eq!(
        mcp_signup_url(brave).as_deref(),
        Some(manager_config::BRAVE_SEARCH_SIGNUP_URL)
    );
    // A config seeded before the field existed (no stored URL)
    // still resolves the curated seed's URL by id — the
    // mcp_start_preflight prompt depends on this for installs
    // that predate the field.
    let mut legacy = brave.clone();
    legacy.signup_url = None;
    assert_eq!(
        mcp_signup_url(&legacy).as_deref(),
        Some(manager_config::BRAVE_SEARCH_SIGNUP_URL)
    );
    // A stored URL wins over the curated seed's.
    let mut custom = brave.clone();
    custom.signup_url = Some("https://example.com/keys".to_string());
    assert_eq!(
        mcp_signup_url(&custom).as_deref(),
        Some("https://example.com/keys")
    );
    // A blank stored URL counts as unset and falls back.
    let mut blank = brave.clone();
    blank.signup_url = Some("  ".to_string());
    assert_eq!(
        mcp_signup_url(&blank).as_deref(),
        Some(manager_config::BRAVE_SEARCH_SIGNUP_URL)
    );
    // An unknown server with no stored URL has no link.
    let mut other = brave.clone();
    other.id = "not_a_curated_server".to_string();
    other.signup_url = None;
    assert_eq!(mcp_signup_url(&other), None);
}

#[test]
fn mcp_credential_hint_prefers_stored_then_seed() {
    let cfg = test_cfg();
    let find = |id: &str| {
        cfg.mcp
            .servers
            .iter()
            .find(|s| s.id == id)
            .unwrap_or_else(|| panic!("default config seeds {id}"))
            .clone()
    };
    // The seeded entries carry their hints themselves — the
    // legacy pair's as well as the curated pair's (the signup
    // fallback only covers the curated pair; the hint fallback
    // covers the whole seeded registry).
    assert_eq!(
        mcp_credential_hint(&find("github")).as_deref(),
        Some("GitHub personal access token — created under GitHub Settings, Developer settings, Personal access tokens")
    );
    assert_eq!(
        mcp_credential_hint(&find("brave_search")).as_deref(),
        Some("Brave Search API key — the subscription key from the Brave Search API page")
    );
    // A config persisted before the field existed (no stored
    // hint) still resolves the seed's hint by id — existing
    // installs' credential fields stay labelled.
    let mut legacy = find("gdrive");
    legacy.credential_hint = None;
    assert_eq!(
        mcp_credential_hint(&legacy).as_deref(),
        Some("Path to the Google OAuth credentials JSON file — downloaded from Google Cloud Console, APIs & Services, Credentials; store the file path, not the file contents")
    );
    // A stored hint wins over the seed's, and a blank stored
    // hint counts as unset and falls back.
    let mut custom = find("github");
    custom.credential_hint = Some("Example token".to_string());
    assert_eq!(
        mcp_credential_hint(&custom).as_deref(),
        Some("Example token")
    );
    custom.credential_hint = Some("  ".to_string());
    assert_eq!(
        mcp_credential_hint(&custom).as_deref(),
        Some("GitHub personal access token — created under GitHub Settings, Developer settings, Personal access tokens")
    );
    // A server with no credential (web_fetch) and an unknown
    // id both resolve to no hint.
    assert_eq!(mcp_credential_hint(&find("web_fetch")), None);
    let mut other = find("github");
    other.id = "not_a_seeded_server".to_string();
    other.credential_hint = None;
    assert_eq!(mcp_credential_hint(&other), None);
}

//! Component-update tests: mocked release feeds (no network) and a
//! recording update environment (no processes, no installs).

use super::*;
use std::collections::HashMap;
use std::sync::Mutex;

// -- fakes -----------------------------------------------------------------

/// Canned release feed: url → body or error; unknown urls fail.
#[derive(Default)]
struct FakeFeed {
    bodies: HashMap<String, Result<String, String>>,
}

impl FakeFeed {
    fn with(mut self, url: &str, body: Result<&str, &str>) -> Self {
        self.bodies.insert(
            url.to_string(),
            body.map(str::to_string).map_err(str::to_string),
        );
        self
    }
}

#[async_trait]
impl ReleaseFeed for FakeFeed {
    async fn get_text(&self, url: &str) -> Result<String, String> {
        self.bodies
            .get(url)
            .cloned()
            .unwrap_or_else(|| Err(format!("no route to {url}")))
    }
}

fn github_body() -> String {
    json!([
        { "tag_name": "b12400", "draft": true, "prerelease": true,
          "assets": [{ "name": "llama-b12400-bin-win-cuda-12.4-x64.zip" }] },
        { "tag_name": "b12340", "draft": false, "prerelease": true,
          "assets": [{ "name": "llama-b12340-bin-win-cuda-12.4-x64.zip" },
                     { "name": "cudart-llama-bin-win-cuda-12.4-x64.zip" }] },
        { "tag_name": "b12350", "draft": false, "prerelease": true,
          "assets": [{ "name": "llama-b12350-bin-win-cuda-13.5-x64.zip" }] },
        { "tag_name": "v0.9.0", "draft": false, "prerelease": false, "assets": [] },
        { "tag_name": "b11000", "draft": false, "prerelease": true,
          "assets": [{ "name": "llama-b11000-bin-win-cuda-12.4-x64.zip" }] }
    ])
    .to_string()
}

fn pypi_body() -> String {
    let file = |yanked: bool| json!([{ "filename": "x.whl", "yanked": yanked }]);
    json!({
        "info": { "version": "1.109.0" },
        "releases": {
            "1.105.1": file(false),
            "1.106.0": file(false),
            "1.107.0": file(false),
            "1.108.0rc1": file(false),
            "1.109.0.dev1": file(false),
            "1.108.2": file(true),
            "1.108.1": file(false),
            "1.110.0": [],
        }
    })
    .to_string()
}

fn full_feed() -> FakeFeed {
    FakeFeed::default()
        .with(LLAMACPP_RELEASES_URL, Ok(&github_body()))
        .with(LITELLM_PYPI_URL, Ok(&pypi_body()))
        .with(
            MXC_NPM_LATEST_URL,
            Ok(r#"{"name":"@microsoft/mxc-sdk","version":"1.0.1"}"#),
        )
}

/// Records every environment call in order.
#[derive(Default)]
struct FakeEnv {
    calls: Mutex<Vec<String>>,
    running: Vec<String>,
    gateway_running: bool,
    installed: Mutex<HashMap<String, String>>,
    /// component → version the llama.cpp install step leaves behind.
    installs_to: HashMap<String, String>,
    /// step id → how many runs fail.
    fail_steps: Mutex<HashMap<String, usize>>,
    smoke_fail: Option<String>,
    /// The llama.cpp install step waits for cancellation, then fails.
    block_until_cancel: bool,
    litellm_pin: Mutex<String>,
}

impl FakeEnv {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
    fn push(&self, c: String) {
        self.calls.lock().unwrap().push(c);
    }
}

#[async_trait]
impl UpdateEnv for FakeEnv {
    async fn running_backends(&self) -> Vec<String> {
        self.running.clone()
    }
    async fn stop_backend(&self, id: &str) -> Result<(), String> {
        self.push(format!("stop_backend {id}"));
        Ok(())
    }
    async fn start_backend(&self, id: &str) -> Result<(), String> {
        self.push(format!("start_backend {id}"));
        Ok(())
    }
    async fn smoke_backend(&self, id: &str, _c: &CancellationToken) -> Result<(), String> {
        self.push(format!("smoke {id}"));
        if self.smoke_fail.as_deref() == Some(id) {
            return Err("HTTP 500".to_string());
        }
        Ok(())
    }
    async fn gateway_running(&self) -> bool {
        self.gateway_running
    }
    async fn stop_gateway(&self) -> Result<(), String> {
        self.push("stop_gateway".to_string());
        Ok(())
    }
    async fn run_step(
        &self,
        step_id: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<(), String> {
        self.push(format!("run_step {step_id}"));
        if self.block_until_cancel && step_id == manager_wizard::steps::STEP_INSTALL_LLAMACPP {
            if let Some(c) = cancel {
                c.cancelled().await;
            }
            return Err("cancelled".to_string());
        }
        {
            let mut fails = self.fail_steps.lock().unwrap();
            if let Some(n) = fails.get_mut(step_id) {
                if *n > 0 {
                    *n -= 1;
                    return Err(format!("{step_id} failed"));
                }
            }
        }
        if step_id == manager_wizard::steps::STEP_START_GATEWAY {
            // The step installs whatever is pinned.
            let pin = self.litellm_pin.lock().unwrap().clone();
            self.installed
                .lock()
                .unwrap()
                .insert(LITELLM_COMPONENT.to_string(), pin);
        } else if let Some(v) = self.installs_to.get(LLAMACPP_COMPONENT) {
            self.installed
                .lock()
                .unwrap()
                .insert(LLAMACPP_COMPONENT.to_string(), v.clone());
        }
        Ok(())
    }
    async fn set_litellm_pin(&self, version: &str) -> Result<(), String> {
        self.push(format!("set_pin {version}"));
        *self.litellm_pin.lock().unwrap() = version.to_string();
        Ok(())
    }
    async fn installed_version(&self, component: &str) -> Option<String> {
        self.installed.lock().unwrap().get(component).cloned()
    }
    async fn backup_install(&self, component: &str) -> Result<(), String> {
        self.push(format!("backup {component}"));
        Ok(())
    }
    async fn restore_install(&self, component: &str) -> Result<(), String> {
        self.push(format!("restore {component}"));
        Ok(())
    }
    async fn discard_backup(&self, component: &str) -> Result<(), String> {
        self.push(format!("discard {component}"));
        Ok(())
    }
    fn progress(&self, _component: &str, _message: &str) {}
}

fn llama_plan() -> ApplyPlan {
    ApplyPlan {
        updates: vec![PlannedUpdate {
            component: LLAMACPP_COMPONENT.to_string(),
            from: Some("b11000".to_string()),
            to: "b11658".to_string(),
        }],
        skipped: Vec::new(),
    }
}

fn litellm_plan(to: &str) -> ApplyPlan {
    ApplyPlan {
        updates: vec![PlannedUpdate {
            component: LITELLM_COMPONENT.to_string(),
            from: Some("1.105.1".to_string()),
            to: to.to_string(),
        }],
        skipped: Vec::new(),
    }
}

fn llama_env() -> FakeEnv {
    FakeEnv {
        running: vec!["a".to_string(), "b".to_string()],
        installs_to: HashMap::from([(LLAMACPP_COMPONENT.to_string(), "b11658".to_string())]),
        ..Default::default()
    }
}

fn litellm_env(gateway_running: bool) -> FakeEnv {
    let env = FakeEnv {
        gateway_running,
        ..Default::default()
    };
    *env.litellm_pin.lock().unwrap() = "1.105.1".to_string();
    env.installed
        .lock()
        .unwrap()
        .insert(LITELLM_COMPONENT.to_string(), "1.105.1".to_string());
    env
}

fn unique_dir(prefix: &str) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "{prefix}-{}-{n}-{}",
        std::process::id(),
        Utc::now().timestamp_nanos_opt().unwrap_or_default()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_markers(data_dir: &Path, llama: Option<&str>, litellm: Option<&str>) {
    if let Some(tag) = llama {
        let d = llamacpp_dir(data_dir);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join(".installed.json"), json!({ "tag": tag }).to_string()).unwrap();
    }
    if let Some(v) = litellm {
        let p = manager_wizard::gateway_spawn_state_path(data_dir);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let state = json!({
            "fingerprint": {
                "port": 4000, "bind_host": "0.0.0.0", "postgres_port": null,
                "master_key_hash": "", "openrouter_key_hash": ""
            },
            "yaml_sha256": "",
            "litellm_version": v,
        });
        std::fs::write(p, state.to_string()).unwrap();
    }
    let pg = manager_wizard::steps::postgres_root(data_dir);
    std::fs::create_dir_all(&pg).unwrap();
    std::fs::write(
        pg.join(".installed.json"),
        json!({ "version": "18.6-1" }).to_string(),
    )
    .unwrap();
}

fn by_id<'a>(reports: &'a [ComponentReport], id: &str) -> &'a ComponentReport {
    reports.iter().find(|r| r.id == id).unwrap()
}

fn pinned(version: &str) -> manager_config::ComponentUpdate {
    manager_config::ComponentUpdate {
        pinned_version: Some(version.to_string()),
        auto_update: false,
        last_checked: None,
    }
}

// -- parsers ---------------------------------------------------------------

#[test]
fn llamacpp_feed_picks_newest_windows_cuda124_build() {
    assert_eq!(
        latest_llamacpp_build(&github_body()).unwrap().as_deref(),
        Some("b12340"),
        "drafts, cuda-13-only builds and non-build tags are ignored"
    );
    assert!(latest_llamacpp_build("not json").is_err());
}

#[test]
fn litellm_feed_picks_newest_served_final_release() {
    // 1.110.0 has no files, 1.108.2 is yanked, rc/dev are not final,
    // 1.107.0 is known-bad: 1.108.1 is the answer.
    let body = pypi_body();
    assert_eq!(
        latest_litellm_release(&body).unwrap().as_deref(),
        Some("1.108.1")
    );
    assert_eq!(
        litellm_release_status(&body, "1.108.2").unwrap(),
        LitellmRelease::Yanked
    );
    assert_eq!(
        litellm_release_status(&body, "1.99.9").unwrap(),
        LitellmRelease::Missing
    );
    assert_eq!(
        litellm_release_status(&body, "1.106.0").unwrap(),
        LitellmRelease::Available
    );
}

#[test]
fn version_order_is_numeric() {
    assert!(version_newer("b11658", "b9999"));
    assert!(version_newer("1.105.10", "1.105.9"));
    assert!(version_newer("18.7-1", "18.6-1"));
    assert!(!version_newer("1.105.1", "1.105.1"));
    assert!(is_final_release("1.106.0"));
    assert!(!is_final_release("1.106.0rc1"));
    assert!(!is_final_release("1.106"));
}

#[test]
fn backup_and_restore_round_trip_the_install_dir() {
    let dir = unique_dir("upd-backup");
    write_markers(&dir, Some("b11000"), None);
    backup_dir(&dir).unwrap();
    assert!(!llamacpp_dir(&dir).exists());
    // A failed install leaves a partial dir behind; restore replaces it.
    std::fs::create_dir_all(llamacpp_dir(&dir)).unwrap();
    std::fs::write(llamacpp_dir(&dir).join("partial.zip"), b"x").unwrap();
    restore_dir(&dir).unwrap();
    assert_eq!(
        installed_version(&dir, LLAMACPP_COMPONENT).as_deref(),
        Some("b11000")
    );
    assert!(!llamacpp_dir(&dir).join("partial.zip").exists());
    assert!(!llamacpp_backup_dir(&dir).exists());
    let _ = std::fs::remove_dir_all(&dir);
}

// -- check -----------------------------------------------------------------

#[tokio::test]
async fn check_reports_versions_pins_and_update_availability() {
    let dir = unique_dir("upd-check");
    write_markers(&dir, Some("b11000"), Some("1.105.1"));
    let cfg = AppConfig::default_config();
    let reports = check_components(&cfg, &dir, &full_feed()).await;
    assert_eq!(reports.len(), 4);

    let llama = by_id(&reports, LLAMACPP_COMPONENT);
    assert_eq!(llama.installed_version.as_deref(), Some("b11000"));
    assert_eq!(llama.latest_available.as_deref(), Some("b12340"));
    assert_eq!(
        llama.target_version.as_deref(),
        Some(LlamaCppPin::default_pin().tag.as_str())
    );
    assert!(!llama.pinned);
    assert!(llama.update_available && llama.applyable);
    assert!(
        llama.note.as_deref().unwrap().contains("b12340"),
        "a newer upstream build is reported as needing a verified pin: {:?}",
        llama.note
    );

    let litellm = by_id(&reports, LITELLM_COMPONENT);
    assert_eq!(litellm.installed_version.as_deref(), Some("1.105.1"));
    assert_eq!(litellm.latest_available.as_deref(), Some("1.108.1"));
    assert!(litellm.pinned, "LiteLLM is always pinned (F1)");
    assert_eq!(litellm.pinned_version.as_deref(), Some("1.105.1"));
    assert!(litellm.update_available);

    let pg = by_id(&reports, POSTGRES_COMPONENT);
    assert_eq!(pg.installed_version.as_deref(), Some("18.6-1"));
    assert!(!pg.update_available && !pg.applyable);
    assert!(pg.source.is_none());

    let mxc = by_id(&reports, MXC_COMPONENT);
    assert_eq!(mxc.installed_version, None);
    assert_eq!(mxc.latest_available.as_deref(), Some("1.0.1"));
    assert!(!mxc.applyable);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn check_reports_network_errors_per_component() {
    let dir = unique_dir("upd-check-net");
    write_markers(&dir, Some("b11000"), None);
    let feed = FakeFeed::default()
        .with(LLAMACPP_RELEASES_URL, Err("request failed: dns error"))
        .with(LITELLM_PYPI_URL, Ok(&pypi_body()))
        .with(MXC_NPM_LATEST_URL, Err("HTTP 503"));
    let reports = check_components(&AppConfig::default_config(), &dir, &feed).await;
    let llama = by_id(&reports, LLAMACPP_COMPONENT);
    assert_eq!(
        llama.check_error.as_deref(),
        Some("request failed: dns error")
    );
    assert_eq!(llama.latest_available, None);
    // The local half of the check still holds.
    assert!(llama.update_available);
    assert_eq!(
        by_id(&reports, MXC_COMPONENT).check_error.as_deref(),
        Some("HTTP 503")
    );
    let litellm = by_id(&reports, LITELLM_COMPONENT);
    assert!(litellm.check_error.is_none());
    assert_eq!(
        litellm.installed_version, None,
        "not measured, never guessed"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn check_marks_a_mismatched_llamacpp_pin_not_applyable() {
    let dir = unique_dir("upd-check-pin");
    write_markers(&dir, Some("b11000"), None);
    let mut cfg = AppConfig::default_config();
    cfg.updates
        .components
        .insert(LLAMACPP_COMPONENT.to_string(), pinned("b11000"));
    let reports = check_components(&cfg, &dir, &full_feed()).await;
    let llama = by_id(&reports, LLAMACPP_COMPONENT);
    assert!(llama.pinned);
    assert!(llama.update_available, "available is still shown");
    assert!(!llama.applyable, "the install step would refuse the pin");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn check_all_caches_and_stamps_last_checked() {
    let dir = unique_dir("upd-check-op");
    write_markers(&dir, Some("b11000"), Some("1.105.1"));
    let state = Arc::new(AppState::new(
        AppConfig::default_config(),
        dir.join("config.json"),
        dir.clone(),
    ));
    let payload = check_all_with(&state, &full_feed()).await.unwrap();
    assert_eq!(payload["components"].as_array().unwrap().len(), 4);
    assert!(dir.join("updates").join("last-check.json").exists());
    {
        let cfg = state.config.read().await;
        assert!(cfg.updates.components[LITELLM_COMPONENT]
            .last_checked
            .is_some());
        assert!(
            !cfg.updates.components.contains_key(LLAMACPP_COMPONENT),
            "the check never creates component entries"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// B18: the last check survives navigation and restarts, with its time,
/// and its rows follow what is installed and pinned now.
#[tokio::test]
async fn list_serves_the_last_check_against_the_current_install() {
    let dir = unique_dir("upd-list");
    write_markers(&dir, Some("b11000"), Some("1.105.1"));
    let state = Arc::new(AppState::new(
        AppConfig::default_config(),
        dir.join("config.json"),
        dir.clone(),
    ));
    let sink: Arc<dyn EventSink> = Arc::new(crate::NoopSink);
    let row = |v: &Value, id: &str| -> Value {
        v["components"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == id)
            .unwrap()
            .clone()
    };
    // Never checked: installed versions, no upstream claims.
    let v = op_list(&state, &sink).await.unwrap();
    assert!(v["checked_at"].is_null());
    let llama = row(&v, LLAMACPP_COMPONENT);
    assert_eq!(llama["installed_version"], "b11000");
    assert!(llama["latest_available"].is_null());
    assert!(llama["check_error"].is_null());

    let mut feed = full_feed();
    feed = feed.with(MXC_NPM_LATEST_URL, Err("HTTP 503"));
    let checked = check_all_with(&state, &feed).await.unwrap();
    // The install moves after the check.
    write_markers(&dir, Some("b11500"), Some("1.105.1"));
    let v = op_list(&state, &sink).await.unwrap();
    assert_eq!(v["checked_at"], checked["checked_at"]);
    let llama = row(&v, LLAMACPP_COMPONENT);
    assert_eq!(llama["installed_version"], "b11500");
    assert_eq!(llama["latest_available"], "b12340");
    assert_eq!(row(&v, LITELLM_COMPONENT)["latest_available"], "1.108.1");
    assert_eq!(row(&v, MXC_COMPONENT)["check_error"], "HTTP 503");
    for id in COMPONENTS {
        assert_eq!(row(&v, id)["auto_update_supported"], false, "{id}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// B18: a LiteLLM that is not installed is reported so, never as
/// updatable.
#[tokio::test]
async fn litellm_not_installed_is_not_updatable() {
    let dir = unique_dir("upd-litellm-absent");
    let cfg = AppConfig::default_config();
    let reports = check_components(&cfg, &dir, &full_feed()).await;
    let litellm = by_id(&reports, LITELLM_COMPONENT);
    assert_eq!(litellm.latest_available.as_deref(), Some("1.108.1"));
    assert!(!litellm.update_available && !litellm.applyable);
    let note = litellm.note.as_deref().unwrap();
    assert!(note.contains("not installed"), "{note}");
    // Installed but never measured says that instead.
    std::fs::create_dir_all(dir.join("python").join("venv")).unwrap();
    let reports = check_components(&cfg, &dir, &full_feed()).await;
    let note = by_id(&reports, LITELLM_COMPONENT).note.clone().unwrap();
    assert!(note.contains("not measured"), "{note}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// B18: a pin names a managed component and a version that can be
/// installed (or already is).
#[tokio::test]
async fn pins_are_validated() {
    let dir = unique_dir("upd-pin");
    write_markers(&dir, Some("b11000"), None);
    let verified = LlamaCppPin::default_pin().tag;
    for (component, version) in [
        (LLAMACPP_COMPONENT, verified.as_str()),
        (LLAMACPP_COMPONENT, "b11000"),
        (LITELLM_COMPONENT, "1.108.1"),
    ] {
        validate_pin(&dir, component, Some(version)).unwrap();
    }
    validate_pin(&dir, POSTGRES_COMPONENT, None).unwrap();
    for (component, version) in [
        (LLAMACPP_COMPONENT, "b99999"),
        (LLAMACPP_COMPONENT, "latest"),
        (LITELLM_COMPONENT, "1.2"),
        (LITELLM_COMPONENT, "1.108.0rc1"),
        (LITELLM_COMPONENT, LITELLM_KNOWN_BAD[0]),
        (POSTGRES_COMPONENT, "99.0-1"),
        (MXC_COMPONENT, "x"),
    ] {
        let err = validate_pin(&dir, component, Some(version)).unwrap_err();
        assert!(matches!(err, ApiError::InvalidInput(_)), "{version}: {err}");
    }
    let err = validate_pin(&dir, "nonsense", None).unwrap_err();
    assert!(matches!(err, ApiError::NotFound(_)), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

// -- planning --------------------------------------------------------------

#[tokio::test]
async fn apply_all_skips_pinned_components() {
    let dir = unique_dir("upd-plan-all");
    write_markers(&dir, Some("b11000"), Some("1.105.1"));
    let mut cfg = AppConfig::default_config();
    let plan = plan_apply(&cfg, &dir, ALL_COMPONENTS, None, &full_feed())
        .await
        .unwrap();
    assert_eq!(plan.updates.len(), 1);
    assert_eq!(plan.updates[0].component, LLAMACPP_COMPONENT);
    assert!(plan
        .skipped
        .iter()
        .any(|s| s.component == LITELLM_COMPONENT && s.reason.contains("pinned")));

    cfg.updates.components.insert(
        LLAMACPP_COMPONENT.to_string(),
        pinned(&LlamaCppPin::default_pin().tag),
    );
    let plan = plan_apply(&cfg, &dir, ALL_COMPONENTS, None, &full_feed())
        .await
        .unwrap();
    assert!(
        plan.updates.is_empty(),
        "a pinned llama.cpp is not auto-applied"
    );
    assert!(plan
        .skipped
        .iter()
        .any(|s| s.component == LLAMACPP_COMPONENT && s.reason.starts_with("pinned")));
    // ...but an explicit per-component apply of the pinned (verified)
    // version is allowed.
    let plan = plan_apply(&cfg, &dir, LLAMACPP_COMPONENT, None, &full_feed())
        .await
        .unwrap();
    assert_eq!(plan.updates.len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn litellm_apply_requires_an_explicit_acceptable_version() {
    let dir = unique_dir("upd-plan-litellm");
    write_markers(&dir, None, Some("1.105.1"));
    let cfg = AppConfig::default_config();
    let feed = full_feed();
    for (version, why) in [
        (None, "no version"),
        (Some("1.107.0"), "known bad"),
        (Some("1.108.2"), "yanked"),
        (Some("1.99.9"), "missing"),
        (Some("1.108.0rc1"), "not final"),
    ] {
        let err = plan_apply(&cfg, &dir, LITELLM_COMPONENT, version, &feed)
            .await
            .unwrap_err();
        assert!(matches!(err, ApiError::InvalidInput(_)), "{why}: {err}");
    }
    let plan = plan_apply(&cfg, &dir, LITELLM_COMPONENT, Some("1.106.0"), &feed)
        .await
        .unwrap();
    assert_eq!(
        plan.updates,
        vec![PlannedUpdate {
            component: LITELLM_COMPONENT.to_string(),
            from: Some("1.105.1".to_string()),
            to: "1.106.0".to_string(),
        }]
    );
    // PyPI unreachable: the install itself decides.
    assert!(plan_apply(
        &cfg,
        &dir,
        LITELLM_COMPONENT,
        Some("1.106.0"),
        &FakeFeed::default()
    )
    .await
    .is_ok());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn report_only_and_unknown_components_are_refused() {
    let dir = unique_dir("upd-plan-other");
    write_markers(&dir, Some("b11000"), None);
    let cfg = AppConfig::default_config();
    let feed = FakeFeed::default();
    for c in [POSTGRES_COMPONENT, MXC_COMPONENT] {
        let err = plan_apply(&cfg, &dir, c, None, &feed).await.unwrap_err();
        assert!(matches!(err, ApiError::Unsupported(_)), "{c}: {err}");
    }
    let err = plan_apply(&cfg, &dir, "gpu-driver", None, &feed)
        .await
        .unwrap_err();
    assert!(matches!(err, ApiError::NotFound(_)));
    let err = plan_apply(&cfg, &dir, LLAMACPP_COMPONENT, Some("b12340"), &feed)
        .await
        .unwrap_err();
    assert!(
        matches!(err, ApiError::InvalidInput(_)),
        "no verified pin: {err}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// -- execution -------------------------------------------------------------

#[tokio::test]
async fn llamacpp_apply_stops_installs_restarts_and_smokes_in_order() {
    let env = llama_env();
    let results = execute_plan(&env, &llama_plan(), &CancellationToken::new()).await;
    assert_eq!(
        results[0].outcome,
        UpdateOutcome::Updated,
        "{:?}",
        results[0]
    );
    assert_eq!(results[0].rolled_back, None);
    assert_eq!(
        env.calls(),
        vec![
            "stop_backend a",
            "stop_backend b",
            "backup llamacpp",
            "run_step install-llamacpp",
            "start_backend a",
            "smoke a",
            "start_backend b",
            "smoke b",
            "discard llamacpp",
        ]
    );
}

#[tokio::test]
async fn llamacpp_install_failure_restores_the_previous_build() {
    let env = llama_env();
    env.fail_steps
        .lock()
        .unwrap()
        .insert("install-llamacpp".to_string(), 1);
    let results = execute_plan(&env, &llama_plan(), &CancellationToken::new()).await;
    assert_eq!(results[0].outcome, UpdateOutcome::Failed);
    assert_eq!(results[0].rolled_back, Some(true));
    assert_eq!(
        env.calls(),
        vec![
            "stop_backend a",
            "stop_backend b",
            "backup llamacpp",
            "run_step install-llamacpp",
            "restore llamacpp",
            "start_backend a",
            "start_backend b",
        ]
    );
}

#[tokio::test]
async fn llamacpp_wrong_installed_tag_rolls_back() {
    let mut env = llama_env();
    env.installs_to
        .insert(LLAMACPP_COMPONENT.to_string(), "b11000".to_string());
    let results = execute_plan(&env, &llama_plan(), &CancellationToken::new()).await;
    assert_eq!(results[0].outcome, UpdateOutcome::Failed);
    assert!(
        results[0].message.contains("expected b11658"),
        "{}",
        results[0].message
    );
    assert!(env.calls().contains(&"restore llamacpp".to_string()));
}

#[tokio::test]
async fn llamacpp_smoke_failure_rolls_back_and_restarts_on_the_old_build() {
    let mut env = llama_env();
    env.smoke_fail = Some("b".to_string());
    let results = execute_plan(&env, &llama_plan(), &CancellationToken::new()).await;
    assert_eq!(results[0].outcome, UpdateOutcome::Failed);
    assert_eq!(results[0].rolled_back, Some(true));
    assert_eq!(
        env.calls(),
        vec![
            "stop_backend a",
            "stop_backend b",
            "backup llamacpp",
            "run_step install-llamacpp",
            "start_backend a",
            "smoke a",
            "start_backend b",
            "smoke b",
            // rollback: stop what the update started, restore,
            // restart what ran before.
            "stop_backend a",
            "stop_backend b",
            "restore llamacpp",
            "start_backend a",
            "start_backend b",
        ]
    );
    assert!(!env.calls().contains(&"discard llamacpp".to_string()));
}

#[tokio::test]
async fn litellm_apply_moves_the_pin_and_restarts_the_gateway() {
    let env = litellm_env(true);
    let results = execute_plan(&env, &litellm_plan("1.106.0"), &CancellationToken::new()).await;
    assert_eq!(
        results[0].outcome,
        UpdateOutcome::Updated,
        "{:?}",
        results[0]
    );
    assert_eq!(
        env.calls(),
        vec!["stop_gateway", "set_pin 1.106.0", "run_step start-gateway"]
    );
    // F1 follow-up: a pin the checked-in hash lock does not cover is
    // reported as not hash-verified, in the log and the result message.
    // (The lock only ever covers DEFAULT_LITELLM_PIN.)
    assert_ne!(manager_config::DEFAULT_LITELLM_PIN, "1.106.0");
    assert!(manager_wizard::gateway_lock::gateway_hash_lock_warning("1.106.0").is_some());
    assert!(
        results[0].message.contains("not hash-verified"),
        "{:?}",
        results[0]
    );
    assert!(
        results[0]
            .log
            .iter()
            .any(|l| l.starts_with("warning:") && l.contains("not hash-verified")),
        "{:?}",
        results[0]
    );
}

#[tokio::test]
async fn litellm_apply_leaves_a_stopped_gateway_stopped() {
    let env = litellm_env(false);
    let results = execute_plan(&env, &litellm_plan("1.106.0"), &CancellationToken::new()).await;
    assert_eq!(results[0].outcome, UpdateOutcome::Updated);
    assert_eq!(
        env.calls(),
        vec![
            "stop_gateway",
            "set_pin 1.106.0",
            "run_step start-gateway",
            "stop_gateway"
        ]
    );
}

#[tokio::test]
async fn litellm_failure_restores_the_previous_pin_and_reinstalls() {
    let env = litellm_env(true);
    env.fail_steps
        .lock()
        .unwrap()
        .insert("start-gateway".to_string(), 1);
    let results = execute_plan(&env, &litellm_plan("1.106.0"), &CancellationToken::new()).await;
    assert_eq!(results[0].outcome, UpdateOutcome::Failed);
    assert_eq!(results[0].rolled_back, Some(true));
    assert_eq!(
        env.calls(),
        vec![
            "stop_gateway",
            "set_pin 1.106.0",
            "run_step start-gateway",
            "stop_gateway",
            "set_pin 1.105.1",
            "run_step start-gateway",
        ]
    );
    assert_eq!(*env.litellm_pin.lock().unwrap(), "1.105.1");
    assert_eq!(
        env.installed.lock().unwrap()[LITELLM_COMPONENT],
        "1.105.1",
        "the previous version is reinstalled"
    );
}

#[tokio::test]
async fn cancellation_mid_install_rolls_back_and_skips_the_rest() {
    let env = Arc::new(FakeEnv {
        block_until_cancel: true,
        ..llama_env()
    });
    let mut plan = llama_plan();
    plan.updates.extend(litellm_plan("1.106.0").updates);
    let token = CancellationToken::new();
    let task = {
        let env = env.clone();
        let token = token.clone();
        tokio::spawn(async move { execute_plan(env.as_ref(), &plan, &token).await })
    };
    // Let the run reach the blocking install step.
    for _ in 0..400 {
        if env.calls().iter().any(|c| c == "run_step install-llamacpp") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    token.cancel();
    let results = task.await.unwrap();
    assert_eq!(results[0].outcome, UpdateOutcome::Cancelled);
    assert_eq!(results[0].rolled_back, Some(true));
    assert_eq!(results[1].outcome, UpdateOutcome::Cancelled);
    assert_eq!(results[1].rolled_back, None, "never started");
    let calls = env.calls();
    assert!(calls.contains(&"restore llamacpp".to_string()), "{calls:?}");
    assert!(
        !calls.iter().any(|c| c.starts_with("set_pin")),
        "the LiteLLM update never started: {calls:?}"
    );
}

#[tokio::test]
async fn apply_op_runs_as_a_task_and_records_history() {
    let dir = unique_dir("upd-apply-op");
    write_markers(&dir, Some("b11000"), Some("1.105.1"));
    let state = Arc::new(AppState::new(
        AppConfig::default_config(),
        dir.join("config.json"),
        dir.clone(),
    ));
    let sink: Arc<dyn EventSink> = Arc::new(crate::NoopSink);
    let env: Arc<dyn UpdateEnv> = Arc::new(llama_env());
    let payload = apply_with(
        &state,
        &sink,
        LLAMACPP_COMPONENT.to_string(),
        None,
        &full_feed(),
        move |_| env,
    )
    .await
    .unwrap();
    let task_id = payload["task_id"].as_str().unwrap().to_string();
    let mut status = TaskStatus::Running;
    for _ in 0..400 {
        status = state.tasks.records.lock().await[&task_id].status;
        if status != TaskStatus::Running {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(status, TaskStatus::Done);
    let detail = state.tasks.records.lock().await[&task_id].detail.clone();
    assert_eq!(detail["results"][0]["outcome"], "updated");
    let history: Vec<Value> = serde_json::from_str(
        &std::fs::read_to_string(dir.join("updates").join("history.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0]["task_id"], task_id);

    // With llama.cpp pinned and LiteLLM always pinned, "all" has
    // nothing to do: no task is recorded.
    state
        .config
        .write()
        .await
        .updates
        .components
        .insert(LLAMACPP_COMPONENT.to_string(), pinned("b11000"));
    let payload = apply_with(
        &state,
        &sink,
        ALL_COMPONENTS.to_string(),
        None,
        &full_feed(),
        |_| Arc::new(FakeEnv::default()) as Arc<dyn UpdateEnv>,
    )
    .await
    .unwrap();
    assert!(payload["task_id"].is_null());
    assert!(payload["plan"]["updates"].as_array().unwrap().is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

/// The backend test reports a non-2xx answer in its payload instead of
/// failing, so the post-update check must read the payload: accepting
/// any `Ok` kept a broken build instead of rolling back.
#[test]
fn smoke_verdict_needs_a_2xx_answer_with_generated_text() {
    let r = |status: u16, content: Value, reasoning: Value| {
        smoke_verdict(json!({
            "status": status, "content": content, "reasoning_content": reasoning,
            "prompt_tokens": null, "completion_tokens": null, "latency_ms": 5
        }))
    };
    assert!(r(200, json!("OK"), Value::Null).is_ok());
    // A model that only got as far as reasoning still generated text.
    assert!(r(200, json!(""), json!("The user wants")).is_ok());
    let err = r(500, Value::Null, Value::Null).unwrap_err();
    assert!(err.contains("HTTP 500"), "{err}");
    assert!(r(503, Value::Null, Value::Null).is_err());
    assert!(r(500, json!("OK"), Value::Null).is_err());
    let err = r(200, json!(" "), Value::Null).unwrap_err();
    assert!(err.contains("no generated text"), "{err}");
    // A payload without a status is never an answer.
    assert!(smoke_verdict(json!({})).is_err());
}

/// The same through the real backend test against a stub server: an
/// HTTP 500 with a JSON error body is an `Ok` from the op and must
/// still fail the check.
#[tokio::test]
async fn a_backend_answering_http_500_fails_the_post_update_check() {
    use crate::test_support::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 8192];
        let _ = sock.read(&mut buf).await;
        let body = r#"{"error":{"message":"failed to decode"}}"#;
        let resp = format!(
            "HTTP/1.1 500 x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = sock.write_all(resp.as_bytes()).await;
    });
    let mut cfg = test_cfg();
    let id = cfg.backends[0].id.clone();
    cfg.backends[0].port = port;
    let (state, dir) = advanced_test_state(cfg);
    let v = crate::op_test_backend(&state, &test_sink(), id, "Reply with OK.".into())
        .await
        .expect("a non-2xx answer is reported in the payload");
    assert_eq!(v["status"], 500);
    assert!(smoke_verdict(v).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

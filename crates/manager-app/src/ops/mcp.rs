//! MCP server and VS Code operations.

use crate::*;

/// MCP server list with live status (spec §14 `mcp_list`), built
/// from the persisted registry. Includes credential presence per
/// env key (Windows Credential Manager lookup; values never read)
/// with dual-target acceptance for the legacy servers (§10.1).
pub async fn op_mcp_list(state: &Arc<AppState>, _sink: &Arc<dyn EventSink>) -> ApiResult {
    let registry = state.mcp_registry().await;
    let servers = state.mcp_servers().await;
    let (statuses, process_states) = {
        let mut sup = state.supervisor().await.lock().await;
        // Reap first so a Local server that exited on its own shows
        // as failed instead of running (PS-5 crash visibility).
        sup.reap_exited();
        let states: HashMap<String, ProcessState> = servers
            .iter()
            .map(|s| (s.id.clone(), mcp_local::process_state(&sup, &s.id)))
            .collect();
        (manager_mcp::list_status(&sup, &servers), states)
    };
    let running: HashMap<String, bool> =
        statuses.iter().map(|s| (s.id.clone(), s.running)).collect();
    // Presence checks are blocking cmdkey calls; run off the async worker.
    let servers_for_presence = servers.clone();
    let found_targets: Vec<Option<String>> = tokio::task::spawn_blocking(move || {
        servers_for_presence
            .iter()
            .map(|s| manager_mcp::credential_target_found(s, cmdkey_target_exists))
            .collect()
    })
    .await
    .map_err(|e| ApiError::Mcp(format!("presence check failed: {e}")))?;
    let entries: Vec<McpListEntry> = registry
        .iter()
        .zip(servers.iter())
        .zip(found_targets.iter())
        .map(|((cfg, spec), found)| {
            let present = found.is_some();
            McpListEntry {
                id: cfg.id.clone(),
                display_name: spec.name.clone(),
                name: spec.name.clone(),
                transport: cfg.transport,
                exposure: cfg.exposure,
                mode: spec.mode,
                command: cfg.command.clone(),
                args: cfg.args.clone(),
                url: cfg.url.clone(),
                env_keys: cfg.env_keys.clone(),
                cred_ref: cfg.cred_ref.clone(),
                signup_url: mcp_signup_url(cfg),
                credential_hint: mcp_credential_hint(cfg),
                allowed_tools: cfg.allowed_tools.clone(),
                metered: cfg.metered,
                rpm_limit: cfg.rpm_limit,
                running: running.get(&cfg.id).copied().unwrap_or(false),
                process_state: process_states
                    .get(&cfg.id)
                    .copied()
                    .unwrap_or(ProcessState::Stopped),
                credential_present: present,
                credential_status: if present {
                    "present".to_string()
                } else {
                    "missing".to_string()
                },
                credential_target_found: found.clone(),
                credential_presence: cfg.env_keys.iter().map(|k| (k.clone(), present)).collect(),
            }
        })
        .collect();
    Ok(serde_json::to_value(&entries)?)
}

/// The registration / API-key page for a server's credential:
/// its own stored `signup_url` when set, else the curated seed
/// entry's for the same id — configurations seeded before the
/// field existed carry the curated servers without it, and they
/// must still surface the link.
pub(crate) fn mcp_signup_url(server: &manager_config::McpServerConfig) -> Option<String> {
    server
        .signup_url
        .clone()
        .filter(|u| !u.trim().is_empty())
        .or_else(|| {
            manager_config::curated_mcp_servers()
                .into_iter()
                .find(|c| c.id == server.id)
                .and_then(|c| c.signup_url)
        })
}

/// The operator-facing description of a server's credential —
/// what kind of credential to store and where it comes from:
/// its own stored `credential_hint` when set, else the seeded
/// registry entry's for the same id. Configurations persisted
/// before the field existed carry the seeded servers without
/// it, and their credential fields must still be labelled
/// (Pete, 2026-10-18). The fallback consults the full seeded
/// registry (not only the curated pair) because the legacy
/// servers' hints live in the legacy seed.
pub(crate) fn mcp_credential_hint(server: &manager_config::McpServerConfig) -> Option<String> {
    server
        .credential_hint
        .clone()
        .filter(|h| !h.trim().is_empty())
        .or_else(|| {
            manager_config::seeded_mcp_servers()
                .into_iter()
                .find(|c| c.id == server.id)
                .and_then(|c| c.credential_hint)
        })
}

/// The Gateway-exposed MCP servers a gateway start would omit for
/// want of a credential (Pete, 2026-10-18): the data behind the
/// start-gateway credential prompt. Each entry is a server that
/// declares env keys and names a credential target, where the
/// target is absent from Credential Manager — storing it (via
/// `mcp_store_credential`) before the start makes the start's
/// arming pass include the server in that same start; skipping
/// leaves it out, which is the start's standing behavior.
/// Desktop-only by design: non-interactive starts (service mode,
/// supervision, the REST activation apply) never consult this —
/// they omit unarmed servers silently, as before.
pub async fn op_mcp_start_preflight(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
) -> ApiResult {
    let cfg = state.config.read().await.clone();
    let presence = mcp_credential_presence(&[&cfg]).await;
    let servers: Vec<Value> = cfg
        .mcp
        .servers
        .iter()
        .filter(|s| s.exposure == manager_config::McpExposure::Gateway && !s.env_keys.is_empty())
        .filter_map(|s| {
            let cred_ref = s.cred_ref.as_deref().filter(|r| !r.is_empty())?;
            if presence.contains(cred_ref) {
                return None;
            }
            Some(json!({
                "id": s.id,
                "display_name": s.display_name,
                "cred_ref": cred_ref,
                "env_keys": s.env_keys,
                "signup_url": mcp_signup_url(s),
                "credential_hint": mcp_credential_hint(s),
                "metered": s.metered,
            }))
        })
        .collect();
    Ok(json!({ "servers": servers }))
}

/// Check whether a Windows Credential Manager target exists.
/// Presence only — the value is never read. Off Windows, always false.
pub(crate) fn cmdkey_target_exists(target: &str) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let mut cmd = std::process::Command::new("cmdkey");
        cmd.arg(format!("/list:{target}"));
        // CREATE_NO_WINDOW: this probe runs from a GUI process.
        cmd.creation_flags(0x08000000);
        cmd.output()
            .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).contains(target))
            .unwrap_or(false)
    }
    #[cfg(not(windows))]
    {
        let _ = target;
        false
    }
}

/// Connection test for one MCP server (spec §14 `mcp_test_connection`),
/// keyed on the server's persisted EXPOSURE (MCP design §8):
/// Gateway servers are tested against the running gateway (see
/// [`mcp_test_gateway_connection`]); Local keeps the supervised-
/// process liveness check and http servers the URL probe; Disabled
/// servers report not-ok.
pub async fn op_mcp_test_connection(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
) -> ApiResult {
    let registry = state.mcp_registry().await;
    let entry = registry
        .iter()
        .find(|s| s.id == id)
        .ok_or_else(|| ApiError::NotFound(format!("mcp server {id}")))?;
    if entry.exposure == manager_config::McpExposure::Gateway {
        return mcp_test_gateway_connection(state, entry).await;
    }
    let servers = state.mcp_servers().await;
    let server = servers
        .iter()
        .find(|s| s.id == id)
        .ok_or_else(|| ApiError::NotFound(format!("mcp server {id}")))?;
    match server.mode {
        manager_mcp::McpMode::Disabled => {
            Ok(json!({ "id": id, "ok": false, "detail": "server disabled" }))
        }
        manager_mcp::McpMode::Local => {
            // A Local entry that cannot run locally (PS-5: http
            // transport, empty command, ...) says why.
            if let Err(e) = manager_mcp::local::local_launch(entry) {
                return Ok(json!({ "id": id, "ok": false, "detail": e.to_string() }));
            }
            let running = {
                let mut sup = state.supervisor().await.lock().await;
                sup.reap_exited();
                manager_mcp::list_status(&sup, std::slice::from_ref(server))
                    .into_iter()
                    .next()
                    .map(|s| s.running)
                    .unwrap_or(false)
            };
            Ok(json!({
                "id": id,
                "ok": running,
                "detail": if running { "supervised process running" } else { "supervised process not running" },
            }))
        }
        manager_mcp::McpMode::Remote => {
            let url = server.url.clone().ok_or_else(|| {
                ApiError::Mcp(format!("remote server {id} has no url configured"))
            })?;
            let probe = HttpHealthProbe::new(url, Duration::from_secs(5));
            match probe.probe().await {
                Ok(manager_supervisor::HealthStatus::Healthy) => {
                    Ok(json!({ "id": id, "ok": true, "detail": "http probe healthy" }))
                }
                Ok(other) => Ok(json!({ "id": id, "ok": false, "detail": format!("{other:?}") })),
                Err(e) => Ok(json!({ "id": id, "ok": false, "detail": e.to_string() })),
            }
        }
    }
}

/// Install Continue.dev + Cline via the wizard's `vscode-extensions` step
/// (spec §9, §14 `vscode_install_extensions`). Runs `code
/// --install-extension` for the pinned extensions through the real system
/// ops; returns the step outcome.
pub async fn op_vscode_install_extensions(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
) -> ApiResult {
    let task_id = state
        .record_task("vscode_install_extensions", json!({}))
        .await;
    let cfg = state.config.read().await.clone();
    let data_dir = state.data_dir.clone();
    let ctx = WizardContext::new(
        cfg,
        data_dir,
        RunMode::Install,
        Arc::new(RealSystemOps::default()),
    );
    let mut wizard = match Wizard::new(
        vec![Box::new(VscodeExtensions::default())],
        RunMode::Install,
    ) {
        Ok(w) => w,
        Err(e) => {
            state
                .finish_task(
                    &task_id,
                    TaskStatus::Failed,
                    json!({ "error": e.to_string() }),
                )
                .await;
            return Err(e.into());
        }
    };
    // task_abort stops the install (the `code` child is killed).
    let cancel_token = state.tasks.controls.token(&task_id);
    let report = wizard
        .run_all_until(&ctx, std::pin::pin!(cancel_token.cancelled()))
        .await;
    let (done, total) = report.progress();
    state
        .finish_task(
            &task_id,
            if cancel_token.is_cancelled() {
                TaskStatus::Aborted
            } else {
                TaskStatus::Done
            },
            json!({ "done": done, "total": total }),
        )
        .await;
    Ok(serde_json::to_value(&report)?)
}

/// User's home directory (`USERPROFILE` on Windows, `HOME` elsewhere).
pub(crate) fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

/// Write the Continue config pointing at the local gateway (spec §9, §14
/// `vscode_write_config`): the `config.yaml` + `.env` pair current
/// Continue loads (the legacy `config.json` write was retired in
/// v0.1.14 — with a config.yaml present the JSON is inert, and its
/// stale literal key is a hazard if the YAML is removed). When
/// `backup` is true, an existing `~/.continue/config.yaml` is first
/// renamed to a timestamped backup (a legacy `config.json` is backed
/// up alongside it), and the `.env` key line is merged in place; when
/// false, the files are only staged under the data dir and the
/// canonical files are left untouched. Returns old/new model lists
/// so the UI can show the diff before apply.
pub async fn op_vscode_write_config(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    backup: bool,
) -> ApiResult {
    let cfg = state.config.read().await;
    // The editor client authenticates with the gateway master key;
    // ensure (generate + store on first use) so the written config
    // works against the key-guarded gateway. The key itself goes to
    // ~/.continue/.env — config.yaml references it as
    // ${{ secrets.LITELLM_API_KEY }} and carries no literal secret.
    let master_key = manager_wizard::ensure_gateway_master_key(&RealSystemOps::default()).await?;
    let new_yaml = continue_config_yaml(&cfg);
    let new_models: Value = serde_yaml::from_str::<Value>(&new_yaml)
        .map_err(|e| ApiError::Config(e.to_string()))?
        .get("models")
        .cloned()
        .unwrap_or(Value::Null);
    drop(cfg);

    let staged_dir = state.data_dir.join("vscode");
    std::fs::create_dir_all(&staged_dir).map_err(ApiError::from)?;
    let staged = staged_dir.join("continue-config.yaml");
    std::fs::write(&staged, new_yaml.as_bytes()).map_err(ApiError::from)?;
    std::fs::write(
        staged_dir.join("continue.env"),
        format!("LITELLM_API_KEY={master_key}\n"),
    )
    .map_err(ApiError::from)?;

    let mut out = json!({
        "staged": staged,
        "canonical": Value::Null,
        "backup_path": Value::Null,
        "legacy_backup_path": Value::Null,
        "old_models": Value::Null,
        "new_models": new_models,
    });
    if !backup {
        return Ok(out);
    }
    let Some(home) = home_dir() else {
        return Ok(out);
    };
    let dir = home.join(".continue");
    std::fs::create_dir_all(&dir).map_err(ApiError::from)?;
    let canonical = dir.join("config.yaml");
    let legacy = dir.join("config.json");
    let ts_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let mut old_models = Value::Null;
    let mut backup_path = Value::Null;
    if canonical.exists() {
        let existing = std::fs::read_to_string(&canonical).map_err(ApiError::from)?;
        old_models = serde_yaml::from_str::<Value>(&existing)
            .ok()
            .and_then(|v| v.get("models").cloned())
            .unwrap_or(Value::Null);
        let bak = dir.join(format!("config.yaml.bak-{ts_ms}"));
        std::fs::rename(&canonical, &bak).map_err(ApiError::from)?;
        backup_path = serde_json::to_value(&bak)?;
    } else if legacy.exists() {
        // No YAML yet: the diff's old side is the legacy JSON config.
        let existing = std::fs::read_to_string(&legacy).map_err(ApiError::from)?;
        old_models = serde_json::from_str::<Value>(&existing)
            .ok()
            .and_then(|v| v.get("models").cloned())
            .unwrap_or(Value::Null);
    }
    std::fs::write(&canonical, new_yaml.as_bytes()).map_err(ApiError::from)?;
    // The .env secret: merged by key, never backed up or replaced.
    let env_path = dir.join(".env");
    let existing_env = std::fs::read_to_string(&env_path).unwrap_or_default();
    std::fs::write(
        &env_path,
        merge_env_contents(&existing_env, "LITELLM_API_KEY", &master_key),
    )
    .map_err(ApiError::from)?;
    // A legacy config.json is inert once config.yaml exists, but if
    // the YAML is ever removed the JSON — with its stale literal
    // key — becomes live again. Back it aside with the same backup.
    let mut legacy_backup_path = Value::Null;
    if legacy.exists() {
        let bak = dir.join(format!("config.json.bak-{ts_ms}"));
        std::fs::rename(&legacy, &bak).map_err(ApiError::from)?;
        legacy_backup_path = serde_json::to_value(&bak)?;
    }
    out["canonical"] = serde_json::to_value(&canonical)?;
    out["backup_path"] = backup_path;
    out["legacy_backup_path"] = legacy_backup_path;
    out["old_models"] = old_models;
    Ok(out)
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct McpListEntry {
    pub id: String,
    pub display_name: String,
    /// Same value as `display_name` (the name `McpServerStatus`
    /// historically carried), kept for back-compat readers.
    pub name: String,
    pub transport: manager_config::McpTransport,
    pub exposure: manager_config::McpExposure,
    /// Derived runtime mode (Gateway maps to Remote; see
    /// `manager_mcp::server_spec_from_config`).
    pub mode: manager_mcp::McpMode,
    pub command: Option<String>,
    pub args: Vec<String>,
    pub url: Option<String>,
    pub env_keys: Vec<String>,
    pub cred_ref: Option<String>,
    /// Registration / API-key page for the server's credential,
    /// resolved by [`mcp_signup_url`] (stored value, else the
    /// curated seed's).
    pub signup_url: Option<String>,
    /// What credential the operator stores for the server (kind
    /// and where it comes from), resolved by
    /// [`mcp_credential_hint`] (stored value, else the seeded
    /// registry's for the same id). The MCP Servers view renders
    /// it as the credential field's label.
    pub credential_hint: Option<String>,
    pub allowed_tools: Option<Vec<String>>,
    pub metered: bool,
    pub rpm_limit: Option<u32>,
    /// Live supervised-process state from `list_status`; only
    /// Local servers are ever spawned, so this is meaningful
    /// (and only ever true) for them.
    pub running: bool,
    /// The supervised process's state (`stopped` | `running` |
    /// `failed` ...): `failed` once a Local server that exited on its
    /// own was reaped (PS-5 crash visibility; additive field).
    pub process_state: ProcessState,
    /// Credential presence over the accepted targets (see
    /// `credential_target_found`): true when any accepted target
    /// exists.
    pub credential_present: bool,
    /// `"present"` | `"missing"` — the §9 card wording.
    pub credential_status: String,
    /// Which accepted Credential Manager target was found, when
    /// one was (dual-target acceptance, design §10.1: for the two
    /// legacy servers either the unified wizard target or the
    /// legacy `llm-manager/mcp/<id>` target counts).
    pub credential_target_found: Option<String>,
    /// Per-env-key presence, as the pre-Phase-1 payload carried it
    /// (values are never read).
    pub credential_presence: Vec<(String, bool)>,
}

/// Set an MCP server's exposure (replaces `mcp_set_mode`, MCP
/// Phase 1). Exposure is persisted in `AppConfig.mcp` through the
/// validated save path (`manager_config::save` on a clone; only a
/// successful save installs it in memory — the pattern of
/// `backend_update_flags`). Transitioning INTO Local spawns the
/// supervised process (the old set_mode behaviour); LEAVING Local
/// stops it. Gateway servers are never spawned here — the gateway
/// hosts them (Phase 2).
pub async fn op_mcp_set_exposure(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
    exposure: String,
) -> ApiResult {
    let exposure = match exposure.as_str() {
        "gateway" => manager_config::McpExposure::Gateway,
        "local" => manager_config::McpExposure::Local,
        "disabled" => manager_config::McpExposure::Disabled,
        other => {
            return Err(ApiError::Mcp(format!(
                "unknown mcp exposure {other:?}; expected gateway | local | disabled"
            )))
        }
    };
    let (previous_exposure, entry) = {
        let mut cfg = state.config.write().await;
        let mut new_cfg = cfg.clone();
        let server = new_cfg
            .mcp
            .servers
            .iter_mut()
            .find(|s| s.id == id)
            .ok_or_else(|| ApiError::NotFound(format!("mcp server {id}")))?;
        let previous = server.exposure;
        server.exposure = exposure;
        let entry = server.clone();
        manager_config::save(&new_cfg, &state.config_path).map_err(ApiError::from)?;
        *cfg = new_cfg;
        (previous, entry)
    };
    // The local process is derived from the registry entry (PS-5,
    // docs/code-review-v0.1.60.md; `mcp_local`), so every persisted
    // server — built-in, curated or operator-added — is handled the
    // same way. Entering Local runs every spawn check; an entry that
    // cannot run locally keeps its saved exposure and says why.
    if exposure == manager_config::McpExposure::Local
        && previous_exposure != manager_config::McpExposure::Local
    {
        mcp_local::start(state, &entry).await.map_err(|e| match e {
            mcp_local::StartError::Spec(e) => ApiError::Mcp(format!(
                "exposure saved, but mcp server {id} has no local process definition \
                 that can run ({e}), so it was not started"
            )),
            mcp_local::StartError::Spawn(e) => e,
        })?;
    } else if previous_exposure == manager_config::McpExposure::Local
        && exposure != manager_config::McpExposure::Local
    {
        mcp_local::stop(state, &id).await?;
    }
    Ok(json!({ "id": id, "exposure": exposure, "persisted": true }))
}

/// Create or update an MCP server in the persisted registry
/// (upsert by id). The argument is the `McpServerConfig` shape as
/// JSON (§3.1). Persistence goes through `manager_config::save`,
/// which validates the whole config first — a §3.2 violation is
/// returned as a command error and nothing is stored (neither on
/// disk nor in memory).
pub async fn op_mcp_save_server(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    server: manager_config::McpServerConfig,
) -> ApiResult {
    let id = server.id.clone();
    // PS-5: a Local definition must be one the app can spawn (stdio,
    // non-empty allowlisted command, no banned arg, no relative or
    // install-dir-escaping command path); refused before anything is
    // stored. Runtime state (credential, command on PATH) is reported
    // in-band by `reconcile_after_save`.
    if server.exposure == manager_config::McpExposure::Local {
        manager_mcp::local::local_launch(&server).map_err(|e| {
            ApiError::InvalidInput(format!("mcp server {id} cannot run locally: {e}"))
        })?;
    }
    let previous = {
        let mut cfg = state.config.write().await;
        let mut new_cfg = cfg.clone();
        let previous = match new_cfg.mcp.servers.iter_mut().find(|s| s.id == id) {
            Some(existing) => Some(std::mem::replace(existing, server.clone())),
            None => {
                new_cfg.mcp.servers.push(server.clone());
                None
            }
        };
        manager_config::save(&new_cfg, &state.config_path).map_err(ApiError::from)?;
        *cfg = new_cfg;
        previous
    };
    mcp_local::reconcile_after_save(state, previous.as_ref(), &server).await
}

/// Remove an MCP server from the persisted registry. If it was
/// Local and running, it is stopped first; then the removal is
/// persisted through the validated save path.
pub async fn op_mcp_delete_server(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
) -> ApiResult {
    if !state.mcp_registry().await.iter().any(|s| s.id == id) {
        return Err(ApiError::NotFound(format!("mcp server {id}")));
    }
    // Any live process for the id is stopped first, whatever the
    // entry's exposure (PS-5: no orphan outlives its definition).
    mcp_local::forget(state, &id).await?;
    let mut cfg = state.config.write().await;
    let mut new_cfg = cfg.clone();
    new_cfg.mcp.servers.retain(|s| s.id != id);
    manager_config::save(&new_cfg, &state.config_path).map_err(ApiError::from)?;
    *cfg = new_cfg;
    Ok(json!({ "id": id, "deleted": true, "persisted": true }))
}

/// Store an MCP server's credential in Windows Credential Manager
/// under the server's declared `cred_ref` target (MCP design §9:
/// the MCP view's credential Store action). The value goes straight
/// to the ops-layer `credential_write` — the same mechanism the
/// stored gateway master key uses — and is never logged, returned,
/// or persisted anywhere else.
pub async fn op_mcp_store_credential(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
    value: String,
) -> ApiResult {
    if value.trim().is_empty() {
        return Err(ApiError::InvalidInput(
            "credential value must not be empty".to_string(),
        ));
    }
    let target = {
        let cfg = state.config.read().await;
        let server = cfg
            .mcp
            .servers
            .iter()
            .find(|s| s.id == id)
            .ok_or_else(|| ApiError::NotFound(format!("mcp server {id}")))?;
        server
            .cred_ref
            .clone()
            .filter(|r| !r.trim().is_empty())
            .ok_or_else(|| {
                ApiError::InvalidInput(format!("mcp server {id} declares no credential target"))
            })?
    };
    RealSystemOps::default()
        .credential_write(&target, &value)
        .await
        .map_err(ApiError::from)?;
    Ok(json!({ "id": id, "stored": true }))
}

/// The Gateway-exposure connection test (MCP design §8): the same
/// two calls the start-gateway step's post-start load check makes —
/// `GET /v1/mcp/server` (is the server registered with the
/// gateway?) and `GET /mcp-rest/tools/list?server_id=<id>` (does
/// its tools list answer without error?) — against the running
/// gateway, authenticated with the stored master key. Pass
/// requires both. The result carries the tool names when listed.
///
/// Phase 3 (§8's explicit-Test addition): when the tested server is
/// the curated `brave_search` AND the two-call test passes, this
/// explicit test — and only this explicit test; the start-time load
/// check never does it — performs ONE real tool call
/// (`POST /mcp-rest/tools/call`, fixed harmless arguments from
/// `manager_wizard::brave_search_test_call_body`) and the payload
/// gains `real_call_ok` / `real_call_detail` (the detail is a
/// capped excerpt, never the full search response). For
/// `web_fetch` a real call is NEVER performed, even here (§8): its
/// test stays registration + tools list, and its detail says so.
pub(crate) async fn mcp_test_gateway_connection(
    state: &AppState,
    entry: &manager_config::McpServerConfig,
) -> ApiResult {
    let id = entry.id.clone();
    let (host, port) = {
        let cfg = state.config.read().await;
        (
            manager_wizard::steps::gateway_probe_host(&cfg),
            cfg.gateway.port,
        )
    };
    let master = RealSystemOps::default()
        .credential_read(manager_wizard::LITELLM_MASTER_KEY_CRED_TARGET)
        .await
        .filter(|k| !k.is_empty());
    let Some(master) = master else {
        return Ok(json!({
            "id": id,
            "ok": false,
            "registered": false,
            "tools": [],
            "detail": "gateway master key not available in Windows Credential Manager; start the gateway once so the app stores one",
        }));
    };
    let master_for_call = master.clone();
    let (list, tools) = {
        let host = host.clone();
        let server_id = id.clone();
        tokio::task::spawn_blocking(move || {
            let list = gateway_http_get(&host, port, "/v1/mcp/server", &master);
            let tools = gateway_http_get(
                &host,
                port,
                &format!("/mcp-rest/tools/list?server_id={server_id}"),
                &master,
            );
            (list, tools)
        })
        .await
        .unwrap_or((None, None))
    };
    let mut problems: Vec<String> = Vec::new();
    let registered = match &list {
        Some((200, body)) => {
            if manager_wizard::parse_mcp_registered_server_ids(body)
                .iter()
                .any(|r| r == &id)
            {
                true
            } else {
                problems.push("not in the gateway's registered MCP server list".to_string());
                false
            }
        }
        Some((status, body)) => {
            let detail = manager_wizard::mcp_response_error(body)
                .unwrap_or_else(|| format!("HTTP {status}"));
            problems.push(format!("GET /v1/mcp/server failed: {detail}"));
            false
        }
        None => {
            problems.push(format!("gateway unreachable at http://{host}:{port}"));
            false
        }
    };
    let mut tool_names: Vec<String> = Vec::new();
    let tools_ok = match &tools {
        Some((200, body)) => match manager_wizard::mcp_response_error(body) {
            Some(e) => {
                problems.push(format!("tools list returned an error: {e}"));
                false
            }
            None => {
                tool_names = manager_wizard::parse_mcp_tool_names(body);
                true
            }
        },
        Some((status, body)) => {
            let detail = manager_wizard::mcp_response_error(body)
                .unwrap_or_else(|| format!("HTTP {status}"));
            problems.push(format!("tools list failed: {detail}"));
            false
        }
        None => {
            if list.is_some() {
                problems.push("tools list request failed: gateway unreachable".to_string());
            }
            false
        }
    };
    let ok = registered && tools_ok;
    let mut detail = if ok {
        format!(
            "registered with the gateway; {} tool(s) listed",
            tool_names.len()
        )
    } else {
        problems.join("; ")
    };
    let mut payload = json!({
        "id": id,
        "ok": ok,
        "registered": registered,
        "tools": tool_names,
        "detail": detail,
    });
    // Phase 3 real-call behaviour (§8) — explicit test only.
    if id == "web_fetch" {
        // NEVER a real call for web_fetch, even from this explicit
        // test: a model-chosen URL fetch is the §5.2 SSRF surface,
        // and the test's fixed arguments would still exercise it.
        detail = format!("{detail}; real call skipped for web_fetch by design");
        payload["detail"] = json!(detail);
        payload["real_call_skipped"] = json!(true);
    } else if id == "brave_search" && ok {
        // One real, metered call with fixed harmless arguments.
        // The body is built by the one constructor in
        // manager-wizard (see its doc for the verified REST shape).
        let body = manager_wizard::brave_search_test_call_body();
        let call = {
            let host = host.clone();
            let master = master_for_call.clone();
            tokio::task::spawn_blocking(move || {
                gateway_http_post(&host, port, "/mcp-rest/tools/call", &master, &body)
            })
            .await
            .unwrap_or(None)
        };
        let (call_ok, call_detail) = match &call {
            Some((status, resp_body)) => {
                manager_wizard::summarize_mcp_tool_call_response(*status, resp_body)
            }
            None => (
                false,
                "real tool call request failed: gateway unreachable".to_string(),
            ),
        };
        payload["real_call_ok"] = json!(call_ok);
        payload["real_call_detail"] = json!(call_detail);
    }
    Ok(payload)
}

#[cfg(test)]
mod tests;

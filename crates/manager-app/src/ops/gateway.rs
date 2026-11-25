//! Gateway config, groups, status, routing, and the activation journal
//! helpers shared by every journaled edit.

use crate::*;

/// Current gateway configuration (from AppConfig). Each group object
/// additionally carries its derived enabled state — `enabled` and
/// `enabled_members`, computed by manager-gateway (the one place the
/// rule lives): a group is enabled exactly when at least one member
/// backend is enabled, and only enabled groups reach the generated
/// gateway config. The state is derived, never stored, so the stored
/// `GatewayConfig` serialization is unchanged.
pub async fn op_get_gateway_config(state: &Arc<AppState>, _sink: &Arc<dyn EventSink>) -> ApiResult {
    let cfg = state.config.read().await;
    let gw: &GatewayConfig = &cfg.gateway;
    let mut v = serde_json::to_value(gw)?;
    if let Some(groups) = v.get_mut("groups").and_then(Value::as_array_mut) {
        for (group, group_json) in cfg.gateway.groups.iter().zip(groups.iter_mut()) {
            let enabled_members = manager_gateway::group_enabled_member_count(group, &cfg);
            group_json["enabled"] = json!(enabled_members > 0);
            group_json["enabled_members"] = json!(enabled_members);
        }
    }
    // The global routing strategy of the generated gateway config (the
    // default of groups without their own), computed by the generator's own rule (additive field;
    // null when no config would be generated). Clients display it
    // and never re-derive it (capability audit §13.2).
    v["effective_strategy"] =
        serde_json::to_value(manager_gateway::effective_global_strategy(&cfg))?;
    // Per included group, the strategy really applied to it: its own, or
    // the global one when it has a single member or the pinned LiteLLM
    // has no per-group strategies.
    v["group_strategies"] = group_strategies_json(&manager_gateway::applied_group_strategies(&cfg));
    Ok(v)
}

fn group_strategies_json(groups: &[(String, manager_config::RoutingStrategy)]) -> Value {
    groups
        .iter()
        .map(|(group, strategy)| json!({ "group": group, "strategy": strategy }))
        .collect()
}

/// Merge a JSON patch into the gateway config and persist: the patch is
/// merged into a clone of the config, the clone is saved via
/// `manager_config::save` (which validates before writing), and only a
/// successful save installs the clone in memory. `persisted` is true
/// only after that save; on failure the in-memory config is unchanged
/// and the error surfaces.
pub async fn op_update_gateway_config(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    patch: Value,
) -> ApiResult {
    let (mut cfg, live) = config_write_for_journal(state).await;
    let pre_cfg = cfg.clone();
    // Validate before journaling, like the other journaling sites: an
    // invalid patch used to write a "Gateway configuration changed"
    // entry and then fail the save, leaving a phantom pending change
    // that cancel refused (finding PS-6, docs/code-review-v0.1.60.md).
    let new_cfg = patched_gateway_config(&pre_cfg, patch)?;
    // The port and bind address travel on the gateway's command line,
    // not in the YAML: journal them regardless.
    let listen_changed = pre_cfg.gateway.port != new_cfg.gateway.port
        || pre_cfg.gateway.bind_host != new_cfg.gateway.bind_host;
    let journaled = record_gateway_change(
        state,
        &live,
        &pre_cfg,
        &new_cfg,
        "gateway_config",
        gateway_change_summary(&pre_cfg, &new_cfg)?,
        listen_changed,
    )
    .await?;
    save_journaled(state, &new_cfg, &journaled)?;
    *cfg = new_cfg;
    drop(cfg);
    drop_net_zero_gateway_entries(state).await;
    Ok(json!({ "ok": true, "persisted": true }))
}

/// Check a gateway patch without applying it (the Admin API's
/// composition of this operation with the bind address one).
pub async fn check_gateway_patch(state: &AppState, patch: Value) -> Result<(), ApiError> {
    let cfg = state.config.read().await.clone();
    patched_gateway_config(&cfg, patch).map(|_| ())
}

/// `cfg` with the gateway patch merged in, validated. The patch is
/// closed: an unknown field or a wrong type is the caller's error.
fn patched_gateway_config(cfg: &AppConfig, patch: Value) -> Result<AppConfig, ApiError> {
    if !patch.is_object() {
        return Err(ApiError::InvalidInput(
            "the gateway patch must be a JSON object".to_string(),
        ));
    }
    if let Some(port) = patch.get("port") {
        check_gateway_port(port)?;
    }
    let mut new_cfg = cfg.clone();
    let mut v = serde_json::to_value(&new_cfg.gateway)?;
    merge_json(&mut v, patch.clone());
    new_cfg.gateway = serde_json::from_value(v)
        .map_err(|e| ApiError::InvalidInput(format!("gateway settings: {e}")))?;
    // What the typed configuration does not have was ignored by the
    // parse: the typo would have been a silent no-op.
    if let Some(field) = unknown_field(&patch, &serde_json::to_value(&new_cfg.gateway)?, "") {
        return Err(ApiError::InvalidInput(format!(
            "unknown gateway setting `{field}`"
        )));
    }
    // A new or renamed group goes by the one naming rule; stored names
    // that predate it stay as they are.
    for g in &new_cfg.gateway.groups {
        if !cfg.gateway.groups.iter().any(|p| p.name == g.name) {
            manager_config::validate_group_name(&g.name).map_err(ApiError::InvalidInput)?;
        }
    }
    validate_plain(&new_cfg)?;
    Ok(new_cfg)
}

/// The first field of `patch` that `known` (the same document, typed)
/// does not have. A group's computed `enabled` and `enabled_members`
/// (`get_gateway_config`) may ride along, so a read can be written back.
fn unknown_field(patch: &Value, known: &Value, path: &str) -> Option<String> {
    match (patch, known) {
        (Value::Object(p), Value::Object(k)) => p.iter().find_map(|(key, value)| {
            let computed = path == "groups[]" && (key == "enabled" || key == "enabled_members");
            let here = if path.is_empty() {
                key.clone()
            } else {
                format!("{path}.{key}")
            };
            match k.get(key) {
                Some(inner) => unknown_field(value, inner, &here),
                None if computed => None,
                None => Some(here),
            }
        }),
        (Value::Array(p), Value::Array(k)) => {
            let here = format!("{path}[]");
            p.iter()
                .zip(k)
                .find_map(|(p, k)| unknown_field(p, k, &here))
        }
        _ => None,
    }
}

/// Refuse an unusable gateway port in plain words (the typed parse
/// would answer "invalid value: integer `70000`, expected u16").
fn check_gateway_port(port: &Value) -> Result<(), ApiError> {
    match port.as_u64() {
        Some(1..=65535) => Ok(()),
        _ => Err(ApiError::InvalidInput(format!(
            "the gateway port must be a whole number from 1 to 65535 (got {port})"
        ))),
    }
}

/// [`manager_config::validate`] with the problems alone as the message
/// (no "config error: validation failed:" in front).
fn validate_plain(cfg: &AppConfig) -> Result<(), ApiError> {
    manager_config::validate(cfg).map_err(|e| match e {
        manager_config::ConfigError::Validation(problems) => ApiError::InvalidInput(problems),
        other => ApiError::from(other),
    })
}

/// Which gateway settings an edit changed, with old and new values
/// where they are short: "Gateway settings changed: port (4000 → 4001),
/// group `coder` deleted". An edit of groups alone names them directly:
/// "Group `coder` changed: strategy (simple-shuffle → least-busy)".
fn gateway_change_summary(pre: &AppConfig, post: &AppConfig) -> Result<String, ApiError> {
    let (a, b) = (
        serde_json::to_value(&pre.gateway)?,
        serde_json::to_value(&post.gateway)?,
    );
    let mut fields: Vec<String> = Vec::new();
    let mut groups: Vec<String> = Vec::new();
    for (key, new) in b.as_object().into_iter().flatten() {
        let old = a.get(key).unwrap_or(&Value::Null);
        if old == new {
            continue;
        }
        if key == "groups" {
            groups = group_changes(&pre.gateway.groups, &post.gateway.groups)?;
        }
        if key != "groups" || groups.is_empty() {
            fields.push(field_change_text(key, old, new));
        }
    }
    if fields.is_empty() && !groups.is_empty() {
        return Ok(groups.join("; ").replacen("group", "Group", 1));
    }
    fields.extend(groups);
    if fields.is_empty() {
        return Ok("Gateway settings changed".to_string());
    }
    Ok(format!("Gateway settings changed: {}", fields.join(", ")))
}

/// What happened to each group, at most four of them.
fn group_changes(
    pre: &[manager_config::ModelGroup],
    post: &[manager_config::ModelGroup],
) -> Result<Vec<String>, ApiError> {
    const SHOWN: usize = 4;
    let list = |ids: &[String]| match ids {
        [] => "none".to_string(),
        ids => ids.join(", "),
    };
    let mut out: Vec<String> = Vec::new();
    for g in post {
        let Some(old) = pre.iter().find(|p| p.name == g.name) else {
            out.push(match g.members.as_slice() {
                [] => format!("group `{}` created", g.name),
                m => format!("group `{}` created (members: {})", g.name, list(m)),
            });
            continue;
        };
        let (a, b) = (serde_json::to_value(old)?, serde_json::to_value(g)?);
        let changed: Vec<String> = b
            .as_object()
            .into_iter()
            .flatten()
            .filter(|(k, v)| a.get(k.as_str()) != Some(v))
            .map(|(k, v)| match k.as_str() {
                "members" => format!("members ({} → {})", list(&old.members), list(&g.members)),
                _ => field_change_text(k, a.get(k.as_str()).unwrap_or(&Value::Null), v),
            })
            .collect();
        if !changed.is_empty() {
            out.push(format!(
                "group `{}` changed: {}",
                g.name,
                changed.join(", ")
            ));
        }
    }
    for p in pre {
        if !post.iter().any(|g| g.name == p.name) {
            out.push(format!("group `{}` deleted", p.name));
        }
    }
    if out.len() > SHOWN {
        let more = out.len() - SHOWN;
        out.truncate(SHOWN);
        out.push(format!("and {more} more"));
    }
    Ok(out)
}

/// One changed field for a plan summary: `name (old → new)` for short
/// plain values, else the name alone.
pub(crate) fn field_change_text(name: &str, old: &Value, new: &Value) -> String {
    let short = |v: &Value| match v {
        Value::Null => Some("none".to_string()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Number(n) => Some(n.to_string()),
        Value::String(s) if s.chars().count() <= 24 => Some(s.clone()),
        _ => None,
    };
    match (short(old), short(new)) {
        (Some(o), Some(n)) => format!("{name} ({o} → {n})"),
        _ => name.to_string(),
    }
}

/// Drop the pending gateway-scope entries when together they changed
/// nothing: the configuration generates exactly what the last
/// successful start was spawned with, on the same port and bind
/// address (a backend moved to another group and back). Best-effort —
/// a kept entry only asks for a restart nobody needs.
pub(crate) async fn drop_net_zero_gateway_entries(state: &AppState) {
    let journal = activation::load_journal(&state.data_dir);
    if !journal.entries.iter().any(|e| e.scope == "gateway") {
        return;
    }
    let cfg = state.config.read().await.clone();
    let listen = manager_wizard::gateway_listen_pending(&cfg, &state.data_dir);
    if listen != (false, false) || manager_wizard::applied_gateway_yaml(&state.data_dir).is_none() {
        return;
    }
    let presence = mcp_credential_presence(&[&cfg]).await;
    if activation::config_drift(&cfg, &state.data_dir, |t| presence.contains(t)) {
        return;
    }
    match activation::clear_gateway_scope(&state.data_dir) {
        Ok(()) if !journal.entries.iter().any(|e| e.scope == "backend") => {
            // Resolved by undoing it: no "plan applied" toast.
            let mut watch = state.notify_watch.lock().unwrap_or_else(|e| e.into_inner());
            watch.activation_cancelled = true;
        }
        Ok(()) => {}
        Err(e) => tracing::warn!("activation journal: net-zero entries not dropped: {e}"),
    }
}

/// Journal a change for the gateway activation plan when it
/// affects what the running gateway serves: the gateway
/// configuration a start would generate differs between `pre` and
/// `post` (the armed-only MCP view on both sides — see
/// [`activation::config_drift`] for why the raw registry is the
/// wrong comparison), or `force` is set for changes the YAML does
/// not carry (the bind address). Saves that leave the gateway
/// configuration identical (a backend's context length, a tensor
/// split) are backend-process concerns and create no gateway plan.
///
/// Callers invoke this BEFORE persisting the configuration and
/// propagate the error: a journal write failure then aborts the
/// change with the configuration untouched, instead of stranding a
/// saved change no plan will ever show (code-review 4.3). The
/// journal's own resolution uses the running backends' real start
/// times, so already-live backend-scope entries prune correctly
/// (4.1). Those come from `live`, gathered by
/// [`config_write_for_journal`] BEFORE the config write lock was
/// taken (PS-11): the journal writers never wait on the supervisor
/// while holding `config.write()`.
pub(crate) async fn record_gateway_change(
    state: &AppState,
    live: &JournalLiveness,
    pre: &AppConfig,
    post: &AppConfig,
    kind: &str,
    summary: String,
    force: bool,
) -> Result<activation::JournalRecords, ApiError> {
    if !force {
        // Compare what a gateway start would WRITE, before and
        // after: the armed-only MCP view on both sides (presence
        // gathered across the pair, so a removed server still
        // resolves). The raw registry is the wrong input — an edit
        // to an unarmed server's stored fields changes the raw
        // YAML but not the started one, and journaling it would
        // demand a gateway restart that changes nothing (the same
        // defect as the drift comparison's, tracker #42).
        let presence = mcp_credential_presence(&[pre, post]).await;
        let before_cfg =
            manager_wizard::config_with_armed_gateway_mcp_servers(pre, |t| presence.contains(t));
        let after_cfg =
            manager_wizard::config_with_armed_gateway_mcp_servers(post, |t| presence.contains(t));
        let before = manager_gateway::generate_config_yaml(&before_cfg).unwrap_or_default();
        let after = manager_gateway::generate_config_yaml(&after_cfg).unwrap_or_default();
        if before == after {
            // The YAML does not carry the routing rule table: a
            // callback-only change (a window, the output reserve,
            // or the advisory toggle on an already-enabled rule)
            // leaves the YAML identical but must still journal, or
            // the activation plan would never offer the restart
            // that regenerates llmdil_routing.py (design §6.3/§7).
            let before_py = manager_gateway::generate_routing_callback(&before_cfg);
            let after_py = manager_gateway::generate_routing_callback(&after_cfg);
            if before_py == after_py {
                return Ok(activation::JournalRecords::none());
            }
        }
    }
    let configured: std::collections::HashSet<String> =
        post.backends.iter().map(|b| b.id.clone()).collect();
    activation::record_change(
        &state.data_dir,
        pre,
        post,
        kind,
        summary,
        "gateway",
        activation::touched_backends(pre, post),
        &live.started_at,
        &configured,
        &live.running,
    )
    .map(activation::JournalRecords::one)
    .map_err(ApiError::Io)
}

/// Journal a change that only the target backend's next
/// (re)start activates — its flags, tensor split, or model
/// assignment. These never appear in the gateway YAML, so the
/// gateway rule in [`record_gateway_change`] cannot see them; the
/// activation plan carries them as backend-scope entries that
/// resolve when the backend (re)starts (or is deleted).
/// Same record-before-save contract as [`record_gateway_change`].
pub(crate) async fn record_backend_change(
    state: &AppState,
    live: &JournalLiveness,
    pre: &AppConfig,
    post: &AppConfig,
    kind: &str,
    summary: String,
    backend_id: &str,
) -> Result<activation::JournalRecords, ApiError> {
    let configured: std::collections::HashSet<String> =
        post.backends.iter().map(|b| b.id.clone()).collect();
    activation::record_change(
        &state.data_dir,
        pre,
        post,
        kind,
        summary,
        "backend",
        vec![backend_id.to_string()],
        &live.started_at,
        &configured,
        &live.running,
    )
    .map(activation::JournalRecords::one)
    .map_err(ApiError::Io)
}

/// Backend liveness for the activation journal writers
/// ([`record_gateway_change`], [`record_backend_change`]): the live
/// backends' start instants and the live set, from ONE
/// [`lifecycle::backend_liveness_for`] snapshot. Only
/// [`config_write_for_journal`] builds it, before it takes the config
/// write lock (PS-11).
pub(crate) struct JournalLiveness {
    started_at: std::collections::HashMap<String, chrono::DateTime<chrono::Utc>>,
    running: std::collections::HashSet<String>,
}

impl JournalLiveness {
    fn from_liveness(live: &HashMap<String, lifecycle::BackendLive>) -> Self {
        Self {
            started_at: live
                .values()
                .filter(|l| l.state.is_live())
                .filter_map(|l| l.started_at.map(|at| (l.id.clone(), at)))
                .collect(),
            running: running_ids_of(live),
        }
    }
}

/// The parts of a configuration [`lifecycle::backend_liveness_for`]
/// reads: the configured backends' ids and ports, and the
/// supervision mode. Two configurations with equal inputs yield the
/// same liveness at the same instant.
fn liveness_inputs(cfg: &AppConfig) -> (Vec<(&str, u16)>, manager_config::SupervisionMode) {
    (
        cfg.backends
            .iter()
            .map(|b| (b.id.as_str(), b.port))
            .collect(),
        cfg.supervision.mode,
    )
}

/// Take the config write lock for a journaled edit, with the backend
/// liveness the journal needs gathered FIRST (finding PS-11,
/// docs/code-review-v0.1.60.md). Liveness takes the supervisor lock,
/// which a backend start holds for up to ~10 s; gathered under
/// `config.write()` (as it used to be), every status/list poll
/// queued behind the parked writer. Here the supervisor (and the SCM
/// snapshot) are read against a config clone with NO config guard
/// held, and the write lock is taken afterwards.
///
/// The race: another writer can change the configuration between
/// the clone and the write lock. The snapshot is kept only when the
/// locked configuration has the same liveness inputs (backend ids,
/// ports, supervision mode — [`liveness_inputs`]); otherwise the
/// guard is dropped and both steps repeat, so the liveness handed
/// to the journal is always the liveness of the configuration being
/// written. Time-wise the snapshot can only be older than the record
/// by the wait for the write lock; that staleness is safe for the
/// journal: an older start time resolves fewer entries (kept as
/// pending, the record-before-save safe direction), and a backend
/// that (re)started since the snapshot launched from the saved
/// configuration that already carried every existing entry, so
/// pruning its entries as already effective stays correct.
pub(crate) async fn config_write_for_journal(
    state: &AppState,
) -> (
    tokio::sync::RwLockWriteGuard<'_, AppConfig>,
    JournalLiveness,
) {
    loop {
        let snapshot = state.config.read().await.clone();
        let live = lifecycle::backend_liveness_for(state, &snapshot).await;
        let guard = state.config.write().await;
        if liveness_inputs(&guard) == liveness_inputs(&snapshot) {
            return (guard, JournalLiveness::from_liveness(&live));
        }
        drop(guard);
    }
}

/// Persist a configuration whose change was journaled ahead of the
/// save (record-before-save, code-review 4.3), rolling the journal
/// back when the save fails (PS-6): the entries this command
/// recorded are removed by id, so a failed save leaves no phantom
/// pending change behind while every other writer's entries and
/// offload overrides stay (see [`activation::remove_entries`]).
pub(crate) fn save_journaled(
    state: &AppState,
    cfg: &AppConfig,
    journaled: &activation::JournalRecords,
) -> Result<(), ApiError> {
    if let Err(e) = manager_config::save(cfg, &state.config_path) {
        roll_back_journal(state, journaled);
        return Err(ApiError::from(e));
    }
    Ok(())
}

/// Fold a further record of the same command into `journaled`; when
/// that record failed, roll back what the command had already
/// journaled before propagating the error (PS-6).
pub(crate) fn chain_journaled(
    state: &AppState,
    journaled: &mut activation::JournalRecords,
    next: Result<activation::JournalRecords, ApiError>,
) -> Result<(), ApiError> {
    match next {
        Ok(records) => {
            journaled.extend(records);
            Ok(())
        }
        Err(e) => {
            roll_back_journal(state, journaled);
            Err(e)
        }
    }
}

/// Best-effort rollback: a failure leaves the phantom entry in
/// place — the safe direction of the record-before-save contract
/// (a plan asking for a restart nobody needed) — and is logged.
pub(crate) fn roll_back_journal(state: &AppState, journaled: &activation::JournalRecords) {
    if let Err(err) = journaled.roll_back(&state.data_dir) {
        tracing::warn!(
            "activation journal: could not roll back {} entries after a failed save: {err}",
            journaled.ids().len()
        );
    }
}

/// The advanced-flags-mode refusal (tracker #60): while a backend
/// is in advanced mode, its raw flags are operator-owned text and
/// the single source of truth for its launch flags, so every
/// structured write path for the backend is refused with this
/// error. The UI disables its controls in the mode; this is the
/// enforcement REST callers and the Optimize apply executor meet.
pub(crate) fn advanced_flags_refusal(backend_id: &str) -> ApiError {
    ApiError::InvalidInput(format!(
        "backend {backend_id} is in advanced flags mode: its flags are \
         edited as raw text (backend_set_raw_flags) and structured flags \
         writes are refused until advanced mode is turned off \
         (backend_set_flags_mode)"
    ))
}

/// Refuse a structured flags write for a backend in advanced
/// flags mode (tracker #60); a no-op otherwise.
pub(crate) fn ensure_backend_not_advanced(
    b: &manager_config::BackendConfig,
) -> Result<(), ApiError> {
    if b.flags_advanced {
        Err(advanced_flags_refusal(&b.id))
    } else {
        Ok(())
    }
}

/// Live backends' start instants, both run modes, from the shared
/// liveness view ([`lifecycle::backend_liveness`]): supervisor uptime
/// for spawned processes, the OS process creation time for adopted
/// ones and for service-managed llama-servers. For activation-plan
/// resolution: a backend-scope entry resolves once every target has
/// (re)started at or after it.
pub(crate) async fn backend_start_times(
    state: &AppState,
    cfg: &AppConfig,
) -> std::collections::HashMap<String, chrono::DateTime<chrono::Utc>> {
    lifecycle::backend_liveness_for(state, cfg)
        .await
        .into_values()
        .filter(|l| l.state.is_live())
        .filter_map(|l| l.started_at.map(|at| (l.id, at)))
        .collect()
}

/// Ids of live backends (running, starting, restarting or not
/// responding), both run modes — a running service-managed backend
/// counts exactly like a supervised one (ACT-4 / SCM-4).
pub(crate) async fn backend_running_ids(
    state: &AppState,
    cfg: &AppConfig,
) -> std::collections::HashSet<String> {
    running_ids_of(&lifecycle::backend_liveness_for(state, cfg).await)
}

/// The live ids of a liveness snapshot (pure).
pub(crate) fn running_ids_of(
    live: &HashMap<String, lifecycle::BackendLive>,
) -> std::collections::HashSet<String> {
    live.values()
        .filter(|l| l.state.is_live())
        .map(|l| l.id.clone())
        .collect()
}

/// Move a backend to a group, or out of all groups
/// (predicted-throughput design §8.5 — the Groups page's drop and
/// "Move to" menu). Move semantics (decision D5): a backend is in
/// at most one group, so the id is removed from every group's
/// member list and appended to the target's; `group: None` is
/// ungroup. A hand-edited config that lists the backend in several
/// groups is normalized by the move — the response's
/// `removed_from` names every group it left, so the page can state
/// the collapse. Dropping a backend on the lane it solely occupies
/// (or ungrouping an ungrouped backend) succeeds without a config
/// write. Persists through the same clone-validate-save path as
/// `backend_add`/`backend_delete` and journals as a gateway-scope
/// change, so the Activation Plan applies the move at the next
/// gateway activation, exactly like every other membership change;
/// the gateway does not restart here.
pub async fn op_gateway_set_backend_group(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    id: String,
    group: Option<String>,
) -> ApiResult {
    let target: Option<String> = match group {
        Some(name) => {
            let name = name.trim().to_string();
            if name.is_empty() {
                return Err(ApiError::InvalidInput(
                    "group name must not be empty; pass null to ungroup the backend".to_string(),
                ));
            }
            Some(name)
        }
        None => None,
    };
    let (mut cfg, live) = config_write_for_journal(state).await;
    let pre_cfg = cfg.clone();
    let mut new_cfg = cfg.clone();
    if !new_cfg.backends.iter().any(|b| b.id == id) {
        return Err(ApiError::NotFound(format!("backend {id}")));
    }
    if let Some(name) = &target {
        if !new_cfg.gateway.groups.iter().any(|g| &g.name == name) {
            return Err(ApiError::NotFound(format!("group {name}")));
        }
    }
    let mut removed_from: Vec<String> = Vec::new();
    let mut added = false;
    for g in new_cfg.gateway.groups.iter_mut() {
        let is_target = target.as_deref() == Some(g.name.as_str());
        if !is_target && g.members.iter().any(|m| m == &id) {
            g.members.retain(|m| m != &id);
            removed_from.push(g.name.clone());
        }
    }
    if let Some(name) = &target {
        let g = new_cfg
            .gateway
            .groups
            .iter_mut()
            .find(|g| &g.name == name)
            .expect("target group existence checked above");
        if !g.members.iter().any(|m| m == &id) {
            g.members.push(id.clone());
            added = true;
        }
    }
    let changed = added || !removed_from.is_empty();
    if changed {
        manager_config::validate(&new_cfg).map_err(ApiError::from)?;
        let summary = match &target {
            Some(name) => format!("Backend `{id}` moved to group `{name}`"),
            None => format!("Backend `{id}` removed from all groups (ungrouped)"),
        };
        let journaled = record_gateway_change(
            state,
            &live,
            &pre_cfg,
            &new_cfg,
            "gateway_set_backend_group",
            summary,
            false,
        )
        .await?;
        save_journaled(state, &new_cfg, &journaled)?;
        *cfg = new_cfg;
        drop(cfg);
        drop_net_zero_gateway_entries(state).await;
    }
    Ok(json!({
        "backend_id": id,
        "group": target,
        "removed_from": removed_from,
        "changed": changed,
        // Same note as the flags setters: after a successful save
        // the desired state is on disk, and when nothing changed
        // it already was.
        "persisted": true,
    }))
}

/// Create an empty group (predicted-throughput design §8.5,
/// decision D11 — the Groups page's New group). The name follows
/// the same slug rule `backend_add` applies to the group it joins
/// or creates. The new group starts with the creation default
/// strategy and no members or fallbacks; it generates no gateway
/// deployments until a backend moves into it, so the save
/// journals an activation change only if the generated gateway
/// configuration actually moves (an empty group does not move
/// it — `record_gateway_change` decides, as for every gateway
/// edit).
pub async fn op_gateway_create_group(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    name: String,
) -> ApiResult {
    let name = name.trim().to_string();
    manager_config::validate_group_name(&name).map_err(ApiError::InvalidInput)?;
    let (mut cfg, live) = config_write_for_journal(state).await;
    let pre_cfg = cfg.clone();
    let mut new_cfg = cfg.clone();
    if new_cfg.gateway.groups.iter().any(|g| g.name == name) {
        return Err(ApiError::InvalidInput(format!(
            "group '{name}' already exists"
        )));
    }
    new_cfg.gateway.groups.push(manager_config::ModelGroup {
        name: name.clone(),
        members: Vec::new(),
        strategy: manager_config::RoutingStrategy::SimpleShuffle,
        fallbacks: Vec::new(),
        purpose: String::new(),
    });
    manager_config::validate(&new_cfg).map_err(ApiError::from)?;
    let journaled = record_gateway_change(
        state,
        &live,
        &pre_cfg,
        &new_cfg,
        "gateway_create_group",
        format!("Group `{name}` created"),
        false,
    )
    .await?;
    save_journaled(state, &new_cfg, &journaled)?;
    *cfg = new_cfg;
    Ok(json!({ "name": name, "persisted": true }))
}

/// Delete a group (predicted-throughput design §8.5, decision
/// D11). Only an empty group deletes: a group with members is
/// refused with the member count, never cascaded — emptying a
/// group is a series of visible moves on the Groups page, not a
/// side effect of a delete. (Member here is the config's member
/// list, which includes `host:port` external endpoints; a group
/// holding only those is still not empty.) Same save path and
/// gateway journaling as the other group ops.
pub async fn op_gateway_delete_group(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    name: String,
) -> ApiResult {
    let name = name.trim().to_string();
    let (mut cfg, live) = config_write_for_journal(state).await;
    let pre_cfg = cfg.clone();
    let mut new_cfg = cfg.clone();
    let pos = new_cfg
        .gateway
        .groups
        .iter()
        .position(|g| g.name == name)
        .ok_or_else(|| ApiError::NotFound(format!("group {name}")))?;
    let members = new_cfg.gateway.groups[pos].members.len();
    if members > 0 {
        return Err(ApiError::InvalidInput(format!(
            "group '{name}' is not empty: it has {members} member(s); \
             move them elsewhere before deleting the group"
        )));
    }
    new_cfg.gateway.groups.remove(pos);
    manager_config::validate(&new_cfg).map_err(ApiError::from)?;
    let journaled = record_gateway_change(
        state,
        &live,
        &pre_cfg,
        &new_cfg,
        "gateway_delete_group",
        format!("Group `{name}` deleted"),
        false,
    )
    .await?;
    save_journaled(state, &new_cfg, &journaled)?;
    *cfg = new_cfg;
    Ok(json!({ "name": name, "deleted": true, "persisted": true }))
}

/// Gateway liveness summary: the configured shape plus a live probe.
/// The probe is a plain TCP/HTTP check against 127.0.0.1:<port>
/// (`GET /v1/models`; any syntactically valid HTTP status line = up)
/// run on a blocking thread — the gateway is a separate LiteLLM
/// process, so config alone cannot say whether it is up.
/// `groups` counts ENABLED groups only (a group is enabled iff at
/// least one member backend is enabled — the derived rule in
/// manager-gateway): the generated config serves enabled groups
/// only, so the configured total would misstate what the gateway
/// runs (Pete, 2026-10-17, tracker #11).
/// `groups`, `effective_strategy` and `group_strategies` describe the
/// RUNNING gateway (its applied config); `saved_groups` and
/// `saved_effective_strategy` are present when the saved configuration
/// differs. `running_port` is the port it answers on, `port_pending`
/// whether another one is saved. `requests_1h`/`errors_1h` count client
/// requests over the trailing hour (`null` until measured).
/// `routing_log_len` is the routing decision ring's actual current
/// entry count (not its capacity). `openrouter_cred_present` reports
/// whether the cloud tier's configured credential reference exists in
/// Windows Credential Manager (presence only — the value is never
/// read); it is null when no reference is configured.
pub async fn op_gateway_status(state: &Arc<AppState>, _sink: &Arc<dyn EventSink>) -> ApiResult {
    let (port, bind_host, saved_groups, saved_routing, routing_rules, cred_ref) = {
        let cfg = state.config.read().await;
        (
            cfg.gateway.port,
            cfg.gateway.bind_host.clone(),
            manager_gateway::enabled_group_count(&cfg),
            manager_gateway::AppliedRouting {
                strategy: manager_gateway::effective_global_strategy(&cfg),
                groups: manager_gateway::applied_group_strategies(&cfg),
            },
            cfg.gateway.routing_rules.len(),
            cfg.gateway.openrouter.cred_ref.clone(),
        )
    };
    // The derived lifecycle views (transition, SCM state, probe, live
    // pid, intent, failure): read with no config guard held, they take
    // it themselves.
    let (view, endpoint_port) = running_gateway_view(state).await;
    let pg = lifecycle::postgres_view(state).await;
    let up = view.up;
    // While a gateway operation or an activation apply is in flight
    // the plan is computed as if the gateway were up: a restart's down
    // window must not make pending changes read as already effective
    // (GW-10 / ACT-5).
    let applying = activation_apply::current_progress(state);
    let up_for_pending = up || view.transition.is_some() || applying.is_some();
    // The workstation's primary LAN address, for the client endpoint
    // shown when the gateway binds beyond loopback. A UDP connect
    // sends nothing; it selects the egress interface.
    let lan_ip = if bind_host != "127.0.0.1" {
        tokio::task::spawn_blocking(primary_lan_ip)
            .await
            .unwrap_or(None)
    } else {
        None
    };
    // The presence check is a blocking OS call; run it off the async
    // worker like the MCP presence checks.
    let openrouter_cred_present: Option<bool> = match cred_ref.filter(|r| !r.is_empty()) {
        Some(target) => Some(
            tokio::task::spawn_blocking(move || credential_target_exists(&target))
                .await
                .unwrap_or(false),
        ),
        None => None,
    };
    let routing_log_len = state.gateway.routing_log.lock().await.len();
    // Activation-plan state for the status surfaces: saved changes
    // that are genuinely pending — a running process still holds
    // the pre-change state (pending_entries applies the live-state
    // rule: changes whose targets are all stopped, or whose
    // gateway is down, are already effective at the next start and
    // never count here) — or live config drift while the gateway
    // is up serving the stale file.
    let (activation_pending, activation_changes) = {
        // A clone, not a held read guard (PS-11): liveness below waits on
        // the supervisor, and a read guard held across that wait queues
        // every config writer (and, behind it, every later reader).
        let cfg = state.config.read().await.clone();
        let journal = activation::load_journal(&state.data_dir);
        let started_at = backend_start_times(state, &cfg).await;
        let running = backend_running_ids(state, &cfg).await;
        let configured: std::collections::HashSet<String> =
            cfg.backends.iter().map(|b| b.id.clone()).collect();
        let pending = activation::pending_entries(
            &state.data_dir,
            &journal,
            &started_at,
            &configured,
            &running,
            up_for_pending,
        );
        let presence = mcp_credential_presence(&[&cfg]).await;
        let drift = activation::drift_unexplained(
            activation::drift_is_pending(
                activation::config_drift(&cfg, &state.data_dir, |t| presence.contains(t)),
                up_for_pending,
            ),
            &pending,
        );
        (
            !pending.is_empty() || drift,
            // The count the activation plan itself reports: its rows
            // plus the synthesized drift item when drift is pending
            // work of its own (tracker #57) — never "pending" beside
            // a zero count.
            activation::plan_rows(&pending).len() + usize::from(drift),
        )
    };
    // What the running gateway serves, from the config it was started
    // with (B5); the saved configuration when nothing runs.
    let running = up
        .then(|| manager_wizard::applied_gateway_yaml(&state.data_dir))
        .flatten()
        .and_then(|yaml| manager_gateway::applied_routing(&yaml));
    let routing = running.as_ref().unwrap_or(&saved_routing);
    let groups = running.as_ref().map_or(saved_groups, |r| r.groups.len());
    let window = state.gateway.stats.lock().await.gateway_window();
    // OpenRouter daily spend vs cap (spec §7), from the app's ledger.
    let openrouter_spend = {
        let cfg = state.config.read().await;
        openrouter_spend::status_for(&state.data_dir, &cfg)
    };
    let mut status = json!({
        "port": port,
        "running_port": up.then_some(endpoint_port),
        "port_pending": up && endpoint_port != port,
        "bind_host": bind_host,
        "lan_ip": lan_ip,
        "openrouter_spend": openrouter_spend,
        "groups": groups,
        "effective_strategy": routing.strategy,
        "group_strategies": group_strategies_json(&routing.groups),
        "requests_1h": window.map(|w| w.requests),
        "errors_1h": window.map(|w| w.errors),
        "stats_window_secs": window.map(|w| w.window_secs),
        "routing_rules": routing_rules,
        // The liveness probe alone (kept for older clients); `state`
        // is the lifecycle state every surface renders.
        "up": up,
        "state": view.state,
        "transition": view.transition,
        "intent": view.intent,
        "pid": view.pid,
        "found_by": view.found_by,
        "service_managed": view.service_managed,
        "failure": view.failure.as_ref().map(|f| json!({
            "at": f.at,
            "reason": f.reason,
            "exit_code": f.exit_code,
        })),
        "postgres": {
            "installed": pg.installed,
            "state": pg.state,
            "pid": pg.pid,
            "port": pg.port,
            "port_mismatch": pg.port_mismatch,
            "transition": pg.transition,
        },
        // The server-side activation apply in flight (step, target,
        // message, source), null when idle.
        "activation_applying": applying,
        "routing_log_len": routing_log_len,
        "openrouter_cred_present": openrouter_cred_present,
        "activation_pending": activation_pending,
        "activation_changes": activation_changes,
        // The LiteLLM version recorded by the last gateway spawn
        // (MCP design §4.3); null when no spawn state has recorded
        // one — the UI renders that as "not measured".
        "litellm_version": manager_wizard::recorded_litellm_version(&state.data_dir),
    });
    if let Some(running) = &running {
        if running.groups.len() != saved_routing.groups.len() {
            status["saved_groups"] = json!(saved_routing.groups.len());
        }
        if running.strategy != saved_routing.strategy {
            status["saved_effective_strategy"] = json!(saved_routing.strategy);
        }
        if running.groups != saved_routing.groups {
            status["saved_group_strategies"] = group_strategies_json(&saved_routing.groups);
        }
    }
    Ok(status)
}

/// The workstation's primary LAN IPv4 address, or None when it
/// cannot be determined. A UDP "connect" sends no traffic; it makes
/// the OS select the egress interface, whose address is the one LAN
/// clients should use.
pub(crate) fn primary_lan_ip() -> Option<String> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    let ip = socket.local_addr().ok()?.ip();
    if ip.is_loopback() || ip.is_unspecified() {
        None
    } else {
        Some(ip.to_string())
    }
}

/// Set the gateway bind address (spec: Pete's decision 2026-10-15 —
/// 0.0.0.0 by default so LAN clients can use the gateway, 127.0.0.1
/// for this computer only; any other value must be an IP address on
/// this machine). Applies at the next gateway start; the firewall
/// rule is reconciled by the start-gateway step at that point.
pub async fn op_gateway_set_bind_host(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    host: String,
) -> ApiResult {
    if host.parse::<std::net::IpAddr>().is_err() {
        return Err(ApiError::InvalidInput(format!(
            "bind host {host:?} is not an IP address"
        )));
    }
    let (mut cfg, live) = config_write_for_journal(state).await;
    let pre_cfg = cfg.clone();
    let mut new_cfg = cfg.clone();
    new_cfg.gateway.bind_host = host.clone();
    manager_config::validate(&new_cfg).map_err(ApiError::from)?;
    let journaled = record_gateway_change(
        state,
        &live,
        &pre_cfg,
        &new_cfg,
        "gateway_bind",
        format!("Gateway network access changed to {host}"),
        true,
    )
    .await?;
    save_journaled(state, &new_cfg, &journaled)?;
    *cfg = new_cfg;
    drop(cfg);
    drop_net_zero_gateway_entries(state).await;
    Ok(json!({ "bind_host": host, "restart_required": true }))
}

/// Preview the generated LiteLLM config.yaml — plus, when a typed
/// routing rule is enabled, the generated `llmdil_routing.py`
/// callback (the Generated config card's second tab; design §7).
/// Null when no rule is enabled and no callback file is generated.
pub async fn op_gateway_preview_yaml(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
) -> ApiResult {
    let cfg = state.config.read().await;
    let yaml = manager_gateway::generate_config_yaml(&cfg).map_err(ApiError::from)?;
    let routing_py = manager_gateway::generate_routing_callback(&cfg);
    Ok(json!({ "yaml": yaml, "routing_py": routing_py }))
}

/// Trace routing for a tagged request: `request` carries the model group
/// name (`{"group": "planner"}`). Runs `pick_backend` as an explainer
/// over the current config (every group member assumed healthy, no
/// load/latency data — the reason string says so) and records the
/// decision in the request ring. This is a dry run for the UI: live
/// traffic is routed by LiteLLM under its single global strategy, not
/// by `pick_backend`.
pub async fn op_gateway_test_routing(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
    request: Value,
) -> ApiResult {
    let group_name = request
        .get("group")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            ApiError::InvalidInput("request must carry a \"group\" string".to_string())
        })?;
    let cfg = state.config.read().await;
    let group = cfg
        .gateway
        .groups
        .iter()
        .find(|g| g.name == group_name)
        .ok_or_else(|| ApiError::NotFound(format!("model group {group_name}")))?;
    let stats: Vec<manager_gateway::BackendStats> = group
        .members
        .iter()
        .map(|m| manager_gateway::BackendStats::healthy(m.clone()))
        .collect();
    let decision = manager_gateway::trace_routing(group, &stats, false, now_ms());
    state
        .gateway
        .routing_log
        .lock()
        .await
        .record(decision.clone());
    Ok(serde_json::to_value(&decision)?)
}

/// Recent gateway routing decisions (traced via `gateway_test_routing`),
/// newest first.
pub async fn op_gateway_recent_requests(
    state: &Arc<AppState>,
    _sink: &Arc<dyn EventSink>,
) -> ApiResult {
    let recent = state.gateway.routing_log.lock().await.recent();
    Ok(json!({ "requests": recent }))
}

/// One raw HTTP GET against the gateway, returning (status, body)
/// — the transport for the gateway-exposure MCP test below.
/// src-tauri has no HTTP-client dependency; `fetch_gateway_metrics`
/// is the in-repo precedent for hand-rolled gateway HTTP over a
/// TcpStream. `None` on any transport failure (gateway down,
/// malformed reply, chunked body — this helper's conservative
/// reading; the metrics fetcher de-chunks instead, see
/// [`decode_metrics_response`]).
pub(crate) fn gateway_http_get(
    host: &str,
    port: u16,
    path: &str,
    master_key: &str,
) -> Option<(u16, String)> {
    use std::io::{Read, Write};
    let ip: std::net::IpAddr = host.parse().ok()?;
    let addr = std::net::SocketAddr::new(ip, port);
    let mut stream =
        std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(800)).ok()?;
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(2)));
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nAuthorization: Bearer {master_key}\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(req.as_bytes()).ok()?;
    let mut raw: Vec<u8> = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&buf[..n]);
                if raw.len() > 8 * 1024 * 1024 {
                    return None;
                }
            }
            Err(_) => break,
        }
    }
    let text = String::from_utf8_lossy(&raw);
    let mut parts = text.splitn(2, "\r\n\r\n");
    let head = parts.next()?;
    let status: u16 = head
        .lines()
        .next()?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()?;
    if head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        return None;
    }
    Some((status, parts.next()?.to_string()))
}

/// One raw HTTP POST against the gateway, returning (status, body)
/// — the POST sibling of [`gateway_http_get`], used only by the
/// explicit MCP test's real tool call below. Same conservative
/// reading (chunked bodies are rejected; these JSON routes answer
/// with a content length — unlike /metrics, which streams chunked
/// and is de-chunked by [`metrics_http_get`]). The read timeout is
/// longer than the GET helper's:
/// a cold stdio server spawn plus a live Brave query legitimately
/// takes seconds.
pub(crate) fn gateway_http_post(
    host: &str,
    port: u16,
    path: &str,
    master_key: &str,
    body: &str,
) -> Option<(u16, String)> {
    use std::io::{Read, Write};
    let ip: std::net::IpAddr = host.parse().ok()?;
    let addr = std::net::SocketAddr::new(ip, port);
    let mut stream =
        std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(800)).ok()?;
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(30)));
    let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(5)));
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nAuthorization: Bearer {master_key}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes()).ok()?;
    let mut raw: Vec<u8> = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&buf[..n]);
                if raw.len() > 8 * 1024 * 1024 {
                    return None;
                }
            }
            Err(_) => break,
        }
    }
    let text = String::from_utf8_lossy(&raw);
    let mut parts = text.splitn(2, "\r\n\r\n");
    let head = parts.next()?;
    let status: u16 = head
        .lines()
        .next()?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()?;
    if head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        return None;
    }
    Some((status, parts.next()?.to_string()))
}

#[cfg(test)]
mod tests;

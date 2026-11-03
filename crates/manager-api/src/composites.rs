//! Server-side compositions (PLAN.md §3.6, rest-api.md §6):
//! endpoints that are not thin wrappers over one registry
//! operation. Each is registered as an explicit derived route; its
//! authorization class is derived from the operations it composes
//! (see `http::derived_class`), so a composite cannot be exposed
//! below the privilege of its parts — the activation apply is the
//! one declared exception, classed L by the plan (decision 5:
//! operators may apply activation plans) even though it internally
//! runs the admin-class start-gateway step, exactly as the desktop
//! flow composes the same primitives for the signed-in user.

use manager_app::registry::{dispatch, Op};
use manager_app::ApiError;
use serde_json::{json, Value};

use crate::ApiCtx;

async fn call(ctx: &ApiCtx, op: Op, args: Value) -> Result<Value, ApiError> {
    dispatch(&ctx.app, &ctx.sink, op, args).await
}

fn invalid(message: impl Into<String>) -> ApiError {
    ApiError::InvalidInput(message.into())
}

fn closed_object(body: &Value) -> Result<&serde_json::Map<String, Value>, ApiError> {
    body.as_object()
        .ok_or_else(|| invalid("request body must be a JSON object"))
}

fn reject_unknown(
    obj: &serde_json::Map<String, Value>,
    allowed: &[&str],
) -> Result<(), ApiError> {
    for key in obj.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(invalid(format!("unknown field `{key}`")));
        }
    }
    Ok(())
}

/// Fetch one item of a list-shaped op payload by its `id` field.
async fn get_one(ctx: &ApiCtx, op: Op, list_key: &str, id: &str) -> Result<Value, ApiError> {
    let payload = call(ctx, op, json!({})).await?;
    let items = payload
        .get(list_key)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    items
        .into_iter()
        .find(|item| item.get("id").and_then(Value::as_str) == Some(id))
        .ok_or_else(|| ApiError::NotFound(format!("no resource with id `{id}`")))
}

/// PATCH /api/v1/models/{id}: `display_name`, `enabled`, the MoE
/// offload declaration (`moe_layers` + `default_offload_layers`),
/// and/or the MTP capability declaration (`mtp`), dispatched to
/// the single-field operations (rest-api §6.3). The offload pair
/// is applied atomically: when either key is present, both are
/// read (an absent key counts as null) and passed to
/// `model_set_offload_spec` — both null clears the declaration.
/// `mtp` is a boolean or null (null clears the declaration).
pub async fn patch_model(ctx: &ApiCtx, id: &str, body: Value) -> Result<Value, ApiError> {
    let obj = closed_object(&body)?;
    reject_unknown(
        obj,
        &["display_name", "enabled", "moe_layers", "default_offload_layers", "mtp"],
    )?;
    if obj.is_empty() {
        return Err(invalid("no fields to apply"));
    }
    if let Some(name) = obj.get("display_name") {
        let name = name
            .as_str()
            .ok_or_else(|| invalid("`display_name` must be a string"))?;
        call(
            ctx,
            Op::ModelSetShortName,
            json!({ "id": id, "name": name }),
        )
        .await?;
    }
    if let Some(enabled) = obj.get("enabled") {
        let enabled = enabled
            .as_bool()
            .ok_or_else(|| invalid("`enabled` must be a boolean"))?;
        call(
            ctx,
            Op::ModelSetEnabled,
            json!({ "id": id, "enabled": enabled }),
        )
        .await?;
    }
    if obj.contains_key("moe_layers") || obj.contains_key("default_offload_layers") {
        let layer_arg = |key: &str| -> Result<Value, ApiError> {
            match obj.get(key) {
                None | Some(Value::Null) => Ok(Value::Null),
                Some(v) => {
                    let n = v
                        .as_u64()
                        .ok_or_else(|| invalid(format!("`{key}` must be a non-negative integer or null")))?;
                    u32::try_from(n)
                        .map(|n| json!(n))
                        .map_err(|_| invalid(format!("`{key}` is out of range")))
                }
            }
        };
        call(
            ctx,
            Op::ModelSetOffloadSpec,
            json!({
                "id": id,
                "moe_layers": layer_arg("moe_layers")?,
                "default_offload_layers": layer_arg("default_offload_layers")?,
            }),
        )
        .await?;
    }
    if let Some(mtp) = obj.get("mtp") {
        let mtp_arg = match mtp {
            Value::Null => Value::Null,
            Value::Bool(b) => json!(b),
            _ => return Err(invalid("`mtp` must be a boolean or null")),
        };
        call(
            ctx,
            Op::ModelSetMtp,
            json!({
                "id": id,
                "mtp": mtp_arg,
            }),
        )
        .await?;
    }
    let model = get_one(ctx, Op::ListModels, "models", id).await?;
    Ok(json!({
        "id": id,
        "name": model.get("display_name").or_else(|| model.get("name")).cloned().unwrap_or(Value::Null),
        "enabled": model.get("enabled").cloned().unwrap_or(json!(true)),
        "moe_layers": model.get("moe_layers").cloned().unwrap_or(Value::Null),
        "default_offload_layers": model.get("default_offload_layers").cloned().unwrap_or(Value::Null),
        "mtp": model.get("mtp").cloned().unwrap_or(Value::Null),
    }))
}

/// The gateway config's groups array, read through the op. The
/// read model decorates each group with computed `enabled` /
/// `enabled_members` fields; those are not stored fields and are
/// stripped so read-modify-write never persists them.
async fn gateway_groups(ctx: &ApiCtx) -> Result<Vec<Value>, ApiError> {
    let cfg = call(ctx, Op::GetGatewayConfig, json!({})).await?;
    let mut groups = cfg
        .get("groups")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for group in &mut groups {
        if let Some(obj) = group.as_object_mut() {
            obj.remove("enabled");
            obj.remove("enabled_members");
        }
    }
    Ok(groups)
}

async fn write_groups(ctx: &ApiCtx, groups: Vec<Value>) -> Result<(), ApiError> {
    call(
        ctx,
        Op::UpdateGatewayConfig,
        json!({ "patch": { "groups": groups } }),
    )
    .await?;
    Ok(())
}

fn group_name(group: &Value) -> Option<&str> {
    group.get("name").and_then(Value::as_str)
}

/// Validate a candidate group object against the app's own
/// `ModelGroup` shape, filling the optional fields' defaults.
/// The stored form is the normalized serialization, so what the
/// API accepts is exactly what the app will read back.
fn normalize_group(body: &Value) -> Result<Value, ApiError> {
    let obj = closed_object(body)?;
    reject_unknown(obj, &["name", "members", "strategy", "fallbacks", "purpose"])?;
    let mut normalized = json!({
        "name": obj.get("name").cloned().unwrap_or(Value::Null),
        "members": obj.get("members").cloned().unwrap_or_else(|| json!([])),
        "strategy": obj.get("strategy").cloned().unwrap_or_else(|| json!("simple-shuffle")),
        "fallbacks": obj.get("fallbacks").cloned().unwrap_or_else(|| json!([])),
        "purpose": obj.get("purpose").cloned().unwrap_or_else(|| json!("")),
    });
    let parsed: manager_config::ModelGroup = serde_json::from_value(normalized.clone())
        .map_err(|e| invalid(format!("invalid group: {e}")))?;
    if parsed.name.trim().is_empty() {
        return Err(invalid("group `name` is required"));
    }
    normalized = serde_json::to_value(&parsed).map_err(ApiError::from)?;
    Ok(normalized)
}

/// POST /api/v1/gateway/groups: create one group (identity is its
/// name; groups have no separate id in the app).
pub async fn create_group(ctx: &ApiCtx, body: Value) -> Result<Value, ApiError> {
    let _guard = ctx.config_write_lock.lock().await;
    create_group_inner(ctx, body).await
}

async fn create_group_inner(ctx: &ApiCtx, body: Value) -> Result<Value, ApiError> {
    let group = normalize_group(&body)?;
    let name = group_name(&group).unwrap_or_default().to_string();
    let mut groups = gateway_groups(ctx).await?;
    if groups.iter().any(|g| group_name(g) == Some(name.as_str())) {
        return Err(invalid(format!("a group named `{name}` already exists")));
    }
    groups.push(group);
    write_groups(ctx, groups).await?;
    Ok(json!({ "name": name }))
}

/// PATCH /api/v1/gateway/groups/{name}: merge `members`,
/// `strategy`, `fallbacks`, and/or `purpose` into the named group.
pub async fn patch_group(ctx: &ApiCtx, name: &str, body: Value) -> Result<Value, ApiError> {
    let _guard = ctx.config_write_lock.lock().await;
    patch_group_inner(ctx, name, body).await
}

async fn patch_group_inner(ctx: &ApiCtx, name: &str, body: Value) -> Result<Value, ApiError> {
    let obj = closed_object(&body)?;
    reject_unknown(obj, &["members", "strategy", "fallbacks", "purpose"])?;
    let mut groups = gateway_groups(ctx).await?;
    let group = groups
        .iter_mut()
        .find(|g| group_name(g) == Some(name))
        .ok_or_else(|| ApiError::NotFound(format!("no gateway group named `{name}`")))?;
    let target = group
        .as_object_mut()
        .ok_or_else(|| invalid("stored group is not an object"))?;
    for (k, v) in obj {
        target.insert(k.clone(), v.clone());
    }
    // The merged group must still satisfy the app's shape.
    let merged = group.clone();
    *group = normalize_group(&merged)?;
    write_groups(ctx, groups).await?;
    Ok(json!({ "name": name }))
}

/// DELETE /api/v1/gateway/groups/{name}.
pub async fn delete_group(ctx: &ApiCtx, name: &str) -> Result<Value, ApiError> {
    let _guard = ctx.config_write_lock.lock().await;
    delete_group_inner(ctx, name).await
}

async fn delete_group_inner(ctx: &ApiCtx, name: &str) -> Result<Value, ApiError> {
    let mut groups = gateway_groups(ctx).await?;
    let before = groups.len();
    groups.retain(|g| group_name(g) != Some(name));
    if groups.len() == before {
        return Err(ApiError::NotFound(format!(
            "no gateway group named `{name}`"
        )));
    }
    write_groups(ctx, groups).await?;
    Ok(json!({ "deleted": name }))
}

/// POST /api/v1/gateway/start: the start-gateway wizard step under
/// a domain name (rest-api §6.5). Same operation, same semantics.
pub async fn gateway_start(ctx: &ApiCtx) -> Result<Value, ApiError> {
    call(
        ctx,
        Op::RunWizardStep,
        json!({ "step_id": "start-gateway" }),
    )
    .await
}

/// POST /api/v1/activation-plan/apply: the one server-side
/// composition (PLAN.md §3.6). Restarts the plan's stale backends,
/// then — when the gateway has pending changes — optionally drains,
/// stops the gateway, runs the start-gateway step, and performs
/// the completion bookkeeping server-side.
/// Validate an apply body (closed object, mode in {drain, now})
/// and return the mode. Split out so the HTTP handler can refuse
/// a malformed request synchronously, before the operation is
/// queued (the composition re-validates on entry).
pub fn validate_apply_body(body: &Value) -> Result<String, ApiError> {
    let obj = closed_object(body)?;
    reject_unknown(obj, &["mode"])?;
    let mode = obj
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or("now");
    if mode != "now" && mode != "drain" {
        return Err(invalid("`mode` must be \"drain\" or \"now\""));
    }
    Ok(mode.to_string())
}

pub async fn activation_apply(ctx: &ApiCtx, body: Value) -> Result<Value, ApiError> {
    let mode = validate_apply_body(&body)?;
    let mode = mode.as_str();
    let plan = call(ctx, Op::GatewayActivationPlan, json!({})).await?;
    let restart_ids: Vec<String> = plan
        .get("backend_restart_ids")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let gateway_pending = plan
        .get("gateway_pending")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let mut restarted: Vec<String> = Vec::new();
    for id in &restart_ids {
        call(
            ctx,
            Op::RestartBackend,
            json!({ "id": id, "graceful": true }),
        )
        .await
        .map_err(|e| {
            ApiError::Gateway(format!(
                "activation apply: restarting backend `{id}` failed: {e}"
            ))
        })?;
        restarted.push(id.clone());
    }

    let mut quiesce: Option<Value> = None;
    let mut gateway_restarted = false;
    if gateway_pending {
        if mode == "drain" {
            // On timeout the apply proceeds (the doc's contract);
            // the outcome is reported, not treated as failure.
            quiesce = call(
                ctx,
                Op::GatewayQuiesceWait,
                json!({ "timeout_secs": 120 }),
            )
            .await
            .ok();
        }
        call(ctx, Op::GatewayStop, json!({})).await?;
        gateway_start(ctx).await.map_err(|e| {
            ApiError::Gateway(format!("activation apply: gateway start failed: {e}"))
        })?;
        gateway_restarted = true;
    }

    call(ctx, Op::GatewayActivationComplete, json!({})).await?;
    let mut result = json!({
        "applied": true,
        "mode": mode,
        "restarted_backends": restarted,
        "gateway_restarted": gateway_restarted,
    });
    if let Some(q) = quiesce {
        result["quiesce"] = q;
    }
    Ok(result)
}

/// PATCH /api/v1/gateway/config composition: a `bind_host` field in
/// the patch goes through the dedicated bind operation (IP
/// validation, activation journaling); the rest of the patch goes
/// through the generic gateway-config update. Called by the op
/// route handler in place of a direct dispatch.
pub async fn update_gateway_config(ctx: &ApiCtx, patch: Value) -> Result<Value, ApiError> {
    let _guard = ctx.config_write_lock.lock().await;
    let mut patch = patch;
    let bind = patch
        .as_object_mut()
        .and_then(|o| o.remove("bind_host"));
    let mut bind_result: Option<Value> = None;
    if let Some(host) = bind {
        let host = host
            .as_str()
            .ok_or_else(|| invalid("`bind_host` must be a string"))?;
        bind_result = Some(
            call(
                ctx,
                Op::GatewaySetBindHost,
                json!({ "host": host }),
            )
            .await?,
        );
    }
    let has_rest = patch
        .as_object()
        .map(|o| !o.is_empty())
        .unwrap_or(false);
    if has_rest {
        let mut result = call(ctx, Op::UpdateGatewayConfig, json!({ "patch": patch })).await?;
        if let (Some(b), Some(obj)) = (bind_result, result.as_object_mut()) {
            obj.insert(
                "bind_host".into(),
                b.get("bind_host").cloned().unwrap_or(Value::Null),
            );
            obj.insert("restart_required".into(), json!(true));
        }
        return Ok(result);
    }
    match bind_result {
        Some(b) => Ok(b),
        None => call(ctx, Op::UpdateGatewayConfig, json!({ "patch": patch })).await,
    }
}
